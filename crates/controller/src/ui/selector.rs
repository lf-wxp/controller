//! # 接收方选择器（Target Selector）
//!
//! ## 职责
//! 在手柄本地的 OLED 屏幕上，让用户通过物理按键**选择当前 Frame 广播的目标接收器**：
//! - 长按 Switch（≥ [`SWITCH_LONG_PRESS_MS`]）→ Normal ↔ Selecting 切换
//! - Selecting 中：摇杆 Y 上/下 = 移动光标；Btn1 = 加入目标；Btn2 = 移出目标
//! - Selecting 中：普通输入（摇杆、按钮）**不再发送 Frame**，只用于操作选择器
//! - Selecting 超过 [`SELECTOR_TIMEOUT_MS`] 无操作 → 视作长按退出（自动保存）
//!
//! ## 数据流
//! ```text
//!  main loop（100Hz）              oled_task（20Hz）
//!  ─────────────────               ─────────────────
//!  sample.buttons / joy_y  ─►  selector::handle_input(...)
//!                              │
//!                              ▼
//!                          TARGET_SELECTOR: Mutex<Inner>
//!                              │
//!                              ▲
//!                              │ snapshot()
//!                          UiState::snapshot() ─► layout::render()
//! ```
//!
//! ## 并发模型
//! - 一写（main loop 的 `handle_input`）、一读（`oled_task` 的 `snapshot`），
//!   本可用无锁 `AtomicU32 + AtomicU8`，但 [`SelectorSnapshot`] 结构较大且
//!   包含变长候选列表 → 用 [`Mutex<CriticalSectionRawMutex, _>`] 的 blocking
//!   变体：`lock()` 只在关中断的极短时间内做**纯内存 memcpy**（无 await），
//!   不违反 [`async-no-lock-await`] 规则。
//! - **不使用 async Mutex**：因为 sampler 是同步代码。
//!
//! ## 候选来源
//! 候选列表直接取自真实的 [`crate::REGISTRY`]（由 Announce / AssignId 通道动态
//! 学习到的接收方列表）。UI 层只负责展示与选择，不内置任何假 peer。

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant};
use heapless::Vec;

// PeerInfo / MAX_PEERS 统一由 comm 提供——与硬件层一致。
// UI 层只使用其介面，不再自定义类型。
pub use comm::{MAX_PEERS, PeerInfo};

/// 面板一次能同时展示的候选行数（受屏幕高度限制）
///
/// 屏幕 64px 高，去掉标题 + 分割线 + 操作提示，中间约剩 40px；
/// 一行 10px → 最多 4 行；预留 1 行防拥挤 → 展示 3 行。
pub const VISIBLE_ROWS: usize = 3;

/// Switch 长按阈值（毫秒）；超过此时长释放视作"长按"
pub const SWITCH_LONG_PRESS_MS: u64 = 800;

/// 选择模式无操作自动退出时长（毫秒）
pub const SELECTOR_TIMEOUT_MS: u64 = 10_000;

/// 摇杆 Y 边沿触发阈值（i16 摇杆值的绝对值 >=  此值算 "推到位"）
///
/// 摇杆量程约 [-2048, 2047]（12-bit ADC 中心化）。1000 ≈ 摇杆最大量程一半，
/// 需要用户明确"推到底一半以上"才触发下一项，避免死区边缘抖动误触。
pub const JOY_EDGE_THRESHOLD: i16 = 1000;

/// 摇杆 Y 边沿复位阈值（回到此绝对值以内视为"松开"，允许下一次触发）
///
/// 用双阈值（Schmitt trigger）避免在边缘反复抖动触发。
pub const JOY_EDGE_RELEASE: i16 = 400;

// ============================================================
// UI 模式
// ============================================================

/// UI 顶层模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UiMode {
  /// 正常模式：所有输入都用于发送 Frame
  #[default]
  Normal,
  /// 选择模式：拦截输入，用于操作接收方选择器
  Selecting,
}

impl UiMode {
  /// 是否处于选择模式
  #[inline]
  #[must_use]
  pub const fn is_selecting(self) -> bool {
    matches!(self, Self::Selecting)
  }
}

// PeerInfo / MAX_PEERS 已从 comm re-export（见文件头部），
// 不在本文件重复定义；以保证 UI 层与硬件层使用同一份类型。

// ============================================================
// SelectorSnapshot —— 供 UI 渲染的只读快照
// ============================================================

/// 一次渲染所需的全部选择器信息（值类型，`Clone` 语义）
///
/// 由 [`snapshot`] 生成；[`super::UiState`] 会在需要绘制选择器面板时携带此结构。
/// 因 `candidates: heapless::Vec<..>` 不实现 `Copy`，本类型只 derive `Clone`。
#[derive(Debug, Clone)]
pub struct SelectorSnapshot {
  /// 光标当前位置（`candidates[cursor]` 是高亮的那一行）
  pub cursor: u8,
  /// 已选目标位图（bit-i = 1 表示 `receiver_id == i` 的 peer 被选中）
  pub pending_mask: u32,
  /// 候选列表（Copy 后独立，不再持有对 registry 的引用）
  pub candidates: Vec<PeerInfo, MAX_PEERS>,
  /// 距离超时退出还剩多少毫秒（可选：面板上展示"倒计时"用）
  pub remaining_ms: u32,
}

impl SelectorSnapshot {
  /// 空快照（Normal 模式下 UiState 里的占位）
  #[must_use]
  pub const fn empty() -> Self {
    Self {
      cursor: 0,
      pending_mask: 0,
      candidates: Vec::new(),
      remaining_ms: 0,
    }
  }
}

// ============================================================
// 内部状态（Mutex 保护）
// ============================================================

/// 选择器内部状态；同时被 sampler（写）和 oled_task（读）访问
#[derive(Debug, Clone, Copy)]
struct TargetSelectorInner {
  /// 当前 UI 模式
  mode: UiMode,
  /// 光标位置（Selecting 才有意义；Normal 时保持上次值）
  cursor: u8,
  /// 已选目标 mask（Selecting 中累计；退出时提升为 `ACTIVE_DEST_MASK`）
  pending_mask: u32,
  /// 进入 Selecting 的时间戳（用于超时判断）
  entered_at: Instant,
  /// 最近一次用户操作时间戳（每次翻页/加/减都刷新，用于 idle 超时）
  last_activity_at: Instant,
  /// Switch 长按检测：稳态从 Released → Pressed 的时刻；None 表示未按下
  switch_press_started_at: Option<Instant>,
  /// 摇杆 Y 边沿检测：`true` 表示当前处于"已推过阈值、等待复位"的锁定态
  joy_y_edge_latched: bool,
}

impl TargetSelectorInner {
  const fn new() -> Self {
    // Instant::from_ticks(0) 是 const 可构造的初值；真实值由首次 handle 更新
    Self {
      mode: UiMode::Normal,
      cursor: 0,
      pending_mask: 0,
      entered_at: Instant::from_ticks(0),
      last_activity_at: Instant::from_ticks(0),
      switch_press_started_at: None,
      joy_y_edge_latched: false,
    }
  }
}

/// 全局选择器状态
///
/// - 生命周期：`'static`（静态存储，无需 StaticCell）
/// - 内层用 [`RefCell`] 包装：`Mutex::lock(|cell| ..)` 中的 `cell: &RefCell<_>`
///   可通过 `borrow_mut` 修e改字段；`Mutex` 本身已关中断互斥，
///   `RefCell` 只提供内部可变性的静态检查
/// - 并发：单写者（main loop 的 sampler tick）+ 单读者（oled_task），
///   `lock` 只在关中断的极短时间内做**纯内存 memcpy**（无 await），
///   不违反 [`async-no-lock-await`] 规则
static TARGET_SELECTOR: Mutex<CriticalSectionRawMutex, RefCell<TargetSelectorInner>> =
  Mutex::new(RefCell::new(TargetSelectorInner::new()));

/// 当前**已生效**的目标 mask（发送 Frame 时用）
///
/// - `0` = 未选任何目标（"广播但没人应答"的语义占位）
/// - `0xFFFF_FFFF` = 广播（全体接收）
/// - 其它 = 组播/单播位图
///
/// **AtomicU32 无锁**：主循环 hot path 每次 send 前读一下，`Ordering::Relaxed`
/// 足够（多读单写场景，只需最终一致性，无 happens-before 依赖）。
static ACTIVE_DEST_MASK: AtomicU32 = AtomicU32::new(BROADCAST_MASK);

/// 广播 mask（32 bit 全 1）
///
/// 命名为常量而不是 magic number，遵循 [`anti-stringly-typed`]。
pub const BROADCAST_MASK: u32 = 0xFFFF_FFFF;

// ============================================================
// 摇杆 Y 边沿 —— 内部 helper
// ============================================================

/// 摇杆 Y 边沿事件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoyYEdge {
  /// 向上（Y 值超过 `+JOY_EDGE_THRESHOLD`）
  Up,
  /// 向下（Y 值超过 `-JOY_EDGE_THRESHOLD`）
  Down,
  /// 无（未过阈值、或已 latch 未复位）
  None,
}

/// 检测摇杆 Y 是否越过阈值；用 Schmitt trigger 做双阈值锁存
fn detect_joy_y_edge(inner: &mut TargetSelectorInner, joy_y: i16) -> JoyYEdge {
  // 早返：处于 latch 中且尚未回到复位阈值 → 忽略
  if inner.joy_y_edge_latched {
    if joy_y.unsigned_abs() < JOY_EDGE_RELEASE.unsigned_abs() {
      inner.joy_y_edge_latched = false;
    }
    return JoyYEdge::None;
  }

  // 未 latch：看是否越过触发阈值
  if joy_y >= JOY_EDGE_THRESHOLD {
    inner.joy_y_edge_latched = true;
    return JoyYEdge::Up;
  }
  if joy_y <= -JOY_EDGE_THRESHOLD {
    inner.joy_y_edge_latched = true;
    return JoyYEdge::Down;
  }
  JoyYEdge::None
}

// ============================================================
// 输入事件类型
// ============================================================

/// 一次 sampler tick 需要交给选择器处理的按键事件集合
///
/// 由 main loop 从 [`crate::input::SampleOutput`] 组装后传入 [`handle_input`]。
/// 所有字段都用**当前 tick 的边沿/稳态**填充，UI 侧不再重复做去抖。
#[derive(Debug, Clone, Copy)]
pub struct SelectorInput {
  /// Switch 稳态是否为"按下/开"
  pub switch_on: bool,
  /// Btn1 本 tick 是否 `JustPressed`
  pub btn1_just_pressed: bool,
  /// Btn2 本 tick 是否 `JustPressed`
  pub btn2_just_pressed: bool,
  /// 摇杆 Y 值（i16 原始量，含正负）
  pub joy_y: i16,
  /// 当前时间（避免 handler 内再取一次，方便测试注入）
  pub now: Instant,
}

/// [`handle_input`] 处理完一次输入后返回的**副作用**
///
/// 让 main loop 知道"是否要吞掉本 tick 的 Frame 发送"，避免选择模式下把
/// 摇杆游走误发到接收端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectorOutcome {
  /// 本 tick 是否处于 Selecting 模式（true → main loop 跳过 `transport.send`）
  pub suppress_frame_send: bool,
  /// 本 tick 是否发生"退出 Selecting"的动作（可选：main loop 记 log / toast）
  pub just_exited: bool,
  /// 本 tick 是否发生"进入 Selecting"的动作
  pub just_entered: bool,
}

// ============================================================
// 公共 API
// ============================================================

/// 处理一次 sampler tick 的输入（main loop 每 [`INPUT_SCAN_INTERVAL_MS`] 调用一次）
///
/// # 返回值
/// [`SelectorOutcome`] —— 告知 main loop 是否要抑制 Frame 发送等副作用。
///
/// [`INPUT_SCAN_INTERVAL_MS`]: crate::config::tuning::INPUT_SCAN_INTERVAL_MS
pub fn handle_input(input: SelectorInput) -> SelectorOutcome {
  let mut outcome = SelectorOutcome {
    suppress_frame_send: false,
    just_exited: false,
    just_entered: false,
  };

  // 预取 candidates（会 lock REGISTRY），避免在 TARGET_SELECTOR lock 内嵌套锁
  let candidates = current_candidates();
  let candidates_len = candidates.len();

  TARGET_SELECTOR.lock(|cell| {
    let mut inner = *cell.borrow();

    // ---- 1) 长按 Switch 检测（跨模式共用） ----
    let switch_toggle = detect_switch_long_press(&mut inner, input.switch_on, input.now);

    if switch_toggle {
      match inner.mode {
        UiMode::Normal => enter_selecting(&mut inner, input.now),
        UiMode::Selecting => exit_selecting_saving(&mut inner),
      }
      // 记录副作用
      outcome.just_entered = matches!(inner.mode, UiMode::Selecting);
      outcome.just_exited = matches!(inner.mode, UiMode::Normal);
    }

    // ---- 2) Selecting 模式下的输入分发 ----
    if inner.mode.is_selecting() {
      // 2a) 超时检查（无操作 SELECTOR_TIMEOUT_MS 视作长按退出）
      if input.now.duration_since(inner.last_activity_at)
        >= Duration::from_millis(SELECTOR_TIMEOUT_MS)
      {
        exit_selecting_saving(&mut inner);
        outcome.just_exited = true;
      } else if candidates_len > 0 {
        // 2b) 摇杆 Y 边沿 → 移动光标
        match detect_joy_y_edge(&mut inner, input.joy_y) {
          JoyYEdge::Up => {
            move_cursor(&mut inner, -1, candidates_len);
            inner.last_activity_at = input.now;
          }
          JoyYEdge::Down => {
            move_cursor(&mut inner, 1, candidates_len);
            inner.last_activity_at = input.now;
          }
          JoyYEdge::None => {}
        }

        // 2c) Btn1 = 加入目标；Btn2 = 移出目标
        if input.btn1_just_pressed {
          add_current_to_mask(&mut inner, &candidates);
          inner.last_activity_at = input.now;
        }
        if input.btn2_just_pressed {
          remove_current_from_mask(&mut inner, &candidates);
          inner.last_activity_at = input.now;
        }
      }
    }

    // Selecting 模式下（无论本 tick 是否有动作），都要抑制 Frame 发送
    outcome.suppress_frame_send = inner.mode.is_selecting();

    // 写回
    *cell.borrow_mut() = inner;
  });

  outcome
}

/// 获取一份供 UI 渲染的只读快照
///
/// # 返回值
/// [`SelectorSnapshot`] —— 包含光标、pending_mask、当前候选列表 Copy。
///
/// Normal 模式下也可以调用（`cursor`/`pending_mask` 保留上次 Selecting 结束时
/// 的值，`candidates` 仍返回当前 registry 快照，用于稳态标题栏渲染）。
#[must_use]
pub fn snapshot(now: Instant) -> SelectorSnapshot {
  // 先获取 candidates（会 lock REGISTRY），避免在 TARGET_SELECTOR lock 内嵌套锁
  let candidates = current_candidates();

  TARGET_SELECTOR.lock(|cell| {
    let inner = *cell.borrow();
    let elapsed_ms = now.duration_since(inner.last_activity_at).as_millis() as u32;
    let remaining_ms = SELECTOR_TIMEOUT_MS.saturating_sub(u64::from(elapsed_ms)) as u32;

    SelectorSnapshot {
      cursor: inner.cursor,
      pending_mask: inner.pending_mask,
      candidates,
      remaining_ms,
    }
  })
}

/// 当前 UI 模式（用于 UiState::snapshot 快照）
#[must_use]
pub fn current_mode() -> UiMode {
  TARGET_SELECTOR.lock(|cell| cell.borrow().mode)
}

/// 当前**已生效**的目标 mask
///
/// - main loop 在决定"要不要 send Frame"以及 "Frame 里 dest_mask 填什么" 时读取
/// - 稳态显示时也会读一次用于标题栏渲染
#[inline]
#[must_use]
pub fn active_dest_mask() -> u32 {
  ACTIVE_DEST_MASK.load(Ordering::Relaxed)
}

// ============================================================
// 内部 helper
// ============================================================

/// 探测 Switch 长按并返回"是否触发一次切换"
///
/// 语义：**按下 → 保持 ≥ 800ms → 释放**这一整套动作在释放瞬间返回 `true`；
/// 短按（<800ms 释放）返回 `false`，Switch 短按的原有语义由 sampler
/// 自行处理（本函数不消费短按事件）。
fn detect_switch_long_press(
  inner: &mut TargetSelectorInner,
  switch_on: bool,
  now: Instant,
) -> bool {
  match (switch_on, inner.switch_press_started_at) {
    // Rising edge：记录起始时间
    (true, None) => {
      inner.switch_press_started_at = Some(now);
      false
    }
    // 保持按下：不触发
    (true, Some(_)) => false,
    // Falling edge：检查是否达到长按阈值
    (false, Some(started)) => {
      inner.switch_press_started_at = None;
      now.duration_since(started) >= Duration::from_millis(SWITCH_LONG_PRESS_MS)
    }
    // 未按下：不动作
    (false, None) => false,
  }
}

/// 进入 Selecting 模式；pending_mask 从 active mask 继承（广播态视作空选集）
///
/// # 为什么广播态要以"空选集"进入
/// 默认 active 是 [`BROADCAST_MASK`]（32 位全 1），其中含 31 个**根本不存在**的
/// peer 的"幽灵位"。若直接继承，UI 会把它们都当成"已选中"，用户"取消选择"某个
/// 真实 peer 后 mask 仍非 0（幽灵位还在），却恰好把该 peer 排除 → 该接收方从此
/// 收不到帧（且永远回不到广播）。因此把"广播"理解为"未做任何限制"，进入时呈现
/// 为空选集；只有非广播的具体 mask 才继承下来，供用户在既有选择上编辑。
fn enter_selecting(inner: &mut TargetSelectorInner, now: Instant) {
  inner.mode = UiMode::Selecting;
  inner.cursor = 0;
  let active = active_dest_mask();
  inner.pending_mask = if active == BROADCAST_MASK { 0 } else { active };
  inner.entered_at = now;
  inner.last_activity_at = now;
  // 进入模式时清掉可能残留的摇杆 latch，避免 UI 一开就跳选
  inner.joy_y_edge_latched = false;
}

/// 退出 Selecting 并把 pending_mask 提升为生效
///
/// # 空选集回退为广播
/// 未选任何目标（`pending_mask == 0`）时保存为 [`BROADCAST_MASK`]（发给所有接收方），
/// 而**不是** `0`。`0` 在协议里是"静默丢弃"（所有接收方都收不到），只应作为显式
/// "暂停下发"的入口，绝不能由"打开列表→没选/取消选择→退出"这种常规操作误触。
fn exit_selecting_saving(inner: &mut TargetSelectorInner) {
  inner.mode = UiMode::Normal;
  let mask = if inner.pending_mask == 0 {
    BROADCAST_MASK
  } else {
    inner.pending_mask
  };
  ACTIVE_DEST_MASK.store(mask, Ordering::Relaxed);
  inner.switch_press_started_at = None;
}

/// 光标上下移动一格（环绕）
fn move_cursor(inner: &mut TargetSelectorInner, delta: i32, len: usize) {
  // 转到 i32 做加法避免 u8 溢出；用 rem_euclid 保证正数余数
  let cur = i32::from(inner.cursor);
  let n = len as i32;
  let next = (cur + delta).rem_euclid(n);
  inner.cursor = next as u8;
}

/// 把光标当前指向的 peer 加入 pending_mask
///
/// `candidates` 由调用方在 lock 外预取，避免嵌套锁
fn add_current_to_mask(inner: &mut TargetSelectorInner, candidates: &[PeerInfo]) {
  let Some(peer) = candidates.get(usize::from(inner.cursor)) else {
    return;
  };
  inner.pending_mask |= 1u32.wrapping_shl(u32::from(peer.receiver_id));
}

/// 把光标当前指向的 peer 从 pending_mask 移除
///
/// `candidates` 由调用方在 lock 外预取，避免嵌套锁
fn remove_current_from_mask(inner: &mut TargetSelectorInner, candidates: &[PeerInfo]) {
  let Some(peer) = candidates.get(usize::from(inner.cursor)) else {
    return;
  };
  inner.pending_mask &= !1u32.wrapping_shl(u32::from(peer.receiver_id));
}

// ============================================================
// PeerRegistry 候选获取（现接真实 registry，Step A 的 MOCK_PEERS 已下线）
// ============================================================

/// 返回当前候选列表的快照（直接代理到 [`crate::REGISTRY`]）
fn current_candidates() -> Vec<PeerInfo, MAX_PEERS> {
  crate::REGISTRY.snapshot()
}

// ============================================================
// 单元测试（host 端可跑：本模块无硬件依赖，只用 embassy_time::Instant）
// ============================================================

#[cfg(test)]
mod tests {
  use super::*;

  fn base_input(now: Instant) -> SelectorInput {
    SelectorInput {
      switch_on: false,
      btn1_just_pressed: false,
      btn2_just_pressed: false,
      joy_y: 0,
      now,
    }
  }

  #[test]
  fn peer_info_role_bytes_trims_padding() {
    // PeerInfo 来自 comm：role 为定长 3 字节，末尾以 `\0` 填充
    let peer = PeerInfo {
      receiver_id: 0,
      mac: [0; 6],
      role: [b'l', b'd', 0],
      rssi_dbm: -42,
    };
    assert_eq!(peer.role_bytes(), b"ld");
  }

  #[test]
  fn broadcast_mask_is_all_ones() {
    assert_eq!(BROADCAST_MASK, u32::MAX);
  }

  #[test]
  fn snapshot_reflects_real_registry() {
    // 清理全局 registry 状态，避免与其他测试并行执行时互相污染
    crate::REGISTRY.clear_for_test();

    // 候选直接取自真实 REGISTRY；空 registry 时快照为空
    assert_eq!(snapshot(Instant::from_ticks(0)).candidates.len(), 0);

    // 注册两台真实 peer 后，快照应反映 registry 内容（含升序 receiver_id）
    let _ = crate::REGISTRY.upsert([0x11, 0, 0, 0, 0, 0], *b"mot", -40, Instant::from_ticks(0));
    let _ = crate::REGISTRY.upsert([0x22, 0, 0, 0, 0, 0], *b"led", -50, Instant::from_ticks(0));
    let snap = snapshot(Instant::from_ticks(0));
    assert_eq!(snap.candidates.len(), 2);
    assert_eq!(snap.candidates[0].receiver_id, 0);
    assert_eq!(snap.candidates[1].receiver_id, 1);
  }
}
