//! # Control 命令通道 —— Host → 手柄反向控制
//!
//! ## 职责
//! - 定义**全局控制上下文** [`JOY_SENSITIVITY`] / [`KNOB_SENSITIVITY`] / [`BATTERY_SIMULATED`]
//! - 提供**命令处理入口** [`dispatch_command`]：BLE / ESP-NOW 接收侧解码后调用
//! - 将副作用**转成 Signal**：LED 效果、Toast 提示通过 Signal 通知专门的 task
//! - **心跳追踪**：收到 Command 时 touch [`crate::ui::touch_host_heartbeat`]
//! - **自动回执**（H 选项）：执行完命令后往双链路（ESP-NOW + BLE）广播 [`CommandResponse::Ack`]
//! - **抗重放**（K2 选项）：**per-key-id** 的 [`REPLAY_WINDOWS`] 拒绝重复 seq，防抓包重发
//! - **密钥轮换**（O 选项）：Response 使用请求 Command 的 `key_id`，Host 可用同一密钥验签
//!
//! ## 为什么抗重放窗口需要 per-key-id？
//! [`crate::protocol::command`] 顶部注释明确：“每个 key_id 拥有独立的 seq 空间”。
//! Host 从旧密钥切换到新密钥时会**重置** tx_counter 从 1 开始；若仅一个全局
//! 窗口，新 key_id 的 seq=1 会被当作"重放"拒绝。因此每个 key_id 单独维护一个
//! [`AntiReplayWindow`]。
//!
//! ## 架构
//! ```text
//!  BLE Write ─┐               ┌─► REPLAY_WINDOWS[key_id].check(seq)  → 拒绝 or 接受
//!             ├─► dispatch_ ──┤   （静默丢弃重放帧）
//!  ESP-NOW ──┘   command()    ├─► CONTROL_CTX (AtomicU16 灵敏度 / AtomicBool)
//!                              ├─► ToastSignal            → oled_task
//!                              ├─► LedEffectSignal        → led_effects_task
//!                              ├─► touch_host_heartbeat()
//!                              └─► broadcast_response(Ack) → ESP-NOW + BLE 双链路
//! ```

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use critical_section::Mutex;
use defmt::{info, warn};

use crate::config::keyring::KEY_SLOTS;
use crate::hal::led_effects::signal_led_effect;
use crate::protocol::{
  AntiReplayWindow, Command, CommandBody, CommandDecodeError, CommandResponse, ErrorCode, KeyId,
  ReplayError, decode_command,
};
use crate::transport::ble_hid;
use crate::transport::esp_now;
use crate::ui::{signal_toast, touch_host_heartbeat};

// ============================================================
// 全局控制上下文（所有 task 共享读取）
// ============================================================

/// 摇杆灵敏度（0..=1000 定点数，1000 = 100%）
pub static JOY_SENSITIVITY: AtomicU16 = AtomicU16::new(1000);
/// 旋钮灵敏度（0..=1000 定点数）
pub static KNOB_SENSITIVITY: AtomicU16 = AtomicU16::new(1000);
/// 电池是否走模拟模式（`true` = 模拟递减；覆盖 `config::battery::SIMULATE`）
pub static BATTERY_SIMULATED: AtomicBool = AtomicBool::new(true);

/// 灵敏度默认值 —— 收到不合法命令时回退到这个值
pub const DEFAULT_SENSITIVITY: u16 = 1000;
/// 灵敏度最大值（同时用作定点分母）
pub const SENSITIVITY_MAX: u16 = 1000;

/// 是否在完成每条命令后自动回 Ack（默认开启；调试时可通过命令关闭）
pub static AUTO_ACK: AtomicBool = AtomicBool::new(true);

/// **抗重放滑动窗口阵列**（K2 + O 选项：per-key-id）
///
/// 每个 key_id slot 各自维护一个独立的 [`AntiReplayWindow`]：
///
/// | 下标 | 含义                                                                 |
/// |------|----------------------------------------------------------------------|
/// | `0`  | `SHARED_SECRETS[0]`（主密钥）对应的 seq 窗口                          |
/// | `1`  | `SHARED_SECRETS[1]`（备用密钥）对应的 seq 窗口                          |
/// | ...  | 依此类推，总共 [`KEY_SLOTS`] 个                                        |
///
/// # 为什么每个 key_id 一个窗口？
/// [`crate::protocol::command`] 顶部注释定义了“每个 key_id 拥有独立的 seq 空间”。
/// Host 在密钥轮换时重置 tx_counter 从 1 开始；若共享全局窗口，新 slot 的 seq=1 会
/// 被当作重放拒绝。为每个 slot 开一个窗口使两个 key_id 下的 seq 彼此隔离。
///
/// # 内存代价
/// [`AntiReplayWindow`] = `u32 + u64`（内存对齐后 16 字节），4 个 slot 共 64 字节。
/// 对 ESP32 128KB SRAM 完全可忽。
///
/// # 初始状态
/// 所有 slot `last_seq = 0`：手柄启动后任意密钥的首个 `seq >= 1` 都能通过第一次校验。
pub static REPLAY_WINDOWS: [Mutex<RefCell<AntiReplayWindow>>; KEY_SLOTS] =
  [const { Mutex::new(RefCell::new(AntiReplayWindow::new())) }; KEY_SLOTS];

/// 便捷 setter：设置摇杆灵敏度（自动 clamp 到 0..=1000）
pub fn set_joy_sensitivity(scale: u16) {
  JOY_SENSITIVITY.store(scale.min(SENSITIVITY_MAX), Ordering::Relaxed);
}

/// 便捷 setter：设置旋钮灵敏度（自动 clamp 到 0..=1000）
pub fn set_knob_sensitivity(scale: u16) {
  KNOB_SENSITIVITY.store(scale.min(SENSITIVITY_MAX), Ordering::Relaxed);
}

/// 便捷 getter：读取摇杆灵敏度快照
pub fn joy_sensitivity() -> u16 {
  JOY_SENSITIVITY.load(Ordering::Relaxed)
}

/// 便捷 getter：读取旋钮灵敏度快照
pub fn knob_sensitivity() -> u16 {
  KNOB_SENSITIVITY.load(Ordering::Relaxed)
}

/// **U 选项**：抗重放窗口落盘触发间隔
///
/// [`dispatch_command`] 每 N 条命令 anti-replay 通过后才会触发一次无影响的
/// [`crate::hal::persist::mark_replay_dirty`]，避免高频命令磨损 flash。
///
/// # 为什么选 100？
/// - 100Hz 命令流下最多每 1 秒写一次 → flash 磨损级别可接受
/// - 重启最多丢失 100 个 seq 的重放保护 → 攻击者需在 100 条命令内重放，
///   实际制造难度极高
pub const REPLAY_PERSIST_INTERVAL: u32 = 100;

/// **M-2**：上次触发 replay-only 落盘时该 slot 已见的最大 seq
///
/// 记录 per-key-id 的“上次已经因 replay-window 变化而写过 flash 时 last_seq 值”。
/// 每收到新的合法 seq 时，仅当 `current_last_seq - LAST_PERSISTED_LAST_SEQ >=
/// REPLAY_PERSIST_INTERVAL` 才再次触发 flash 写入，防止：
///
/// 1. 恶意攻击者拿到密钥后不断构造 `seq = 100/200/300...` 的合法命令，
///    每 100 条 seq 就在 1 秒内触发一次 flash 写，加速 NOR flash 磨损；
/// 2. seq 因 anti-replay 窗口位图乱序回落导致 `seq % 100 == 0` 反复触发。
///
/// 用 `AtomicU32` per-slot；`Relaxed` 已足够（本状态只影响“是否触发一次
/// 幂等的 mark_dirty”，无跨线程数据依赖）。
static LAST_PERSISTED_LAST_SEQ: [AtomicU32; KEY_SLOTS] = [const { AtomicU32::new(0) }; KEY_SLOTS];

/// 便捷入口：对指定 `key_id` 下的 `seq` 做 anti-replay 校验并就地更新窗口
///
/// # 为什么取 slot 而不是全局？
/// 见 [`REPLAY_WINDOWS`] 的 doc：每个 key_id 拥有独立的 seq 空间，避免密钥轮换时
/// 新 key_id 的 `seq=1` 被旧 key_id 的 `last_seq >= 1` 误判为重放。
///
/// # 参数
/// - `key_id`：已经通过 `decode_command` 校验，保证在 [`KEY_SLOTS`] 范围内
/// - `seq`：待校验的序列号
///
/// # 返回
/// 语义与 [`AntiReplayWindow::check_and_update`] 一致。
fn check_replay(key_id: KeyId, seq: u32) -> Result<(), ReplayError> {
  let slot = key_id.as_u8() as usize;
  // 防御性断言：decode_command 已保证 key_id 在范围内，这里直接用下标安全
  debug_assert!(slot < KEY_SLOTS, "key_id must be within KEY_SLOTS");
  critical_section::with(|cs| {
    REPLAY_WINDOWS[slot]
      .borrow_ref_mut(cs)
      .check_and_update(seq)
  })
}

/// **U 选项**：一次带回全部 [`KEY_SLOTS`] 个 slot 的当前窗口快照
///
/// # 为什么一次取全部？
/// [`crate::hal::persist::PersistentConfig::replay_windows`] 字段需要全部 slot；
/// 在单一 critical section 里一次拿完避免"取 slot 0 与取 slot 1 之间发生写入"
/// 导致的不一致快照。
///
/// # 使用方
/// - [`mark_persist_dirty`]：伴随业务变更一并下发到持久化层
/// - [`dispatch_command`]：每 [`REPLAY_PERSIST_INTERVAL`] 递增一次刷 replay-only 快照
pub fn snapshot_replay_windows() -> [AntiReplayWindow; KEY_SLOTS] {
  critical_section::with(|cs| {
    let mut out = [AntiReplayWindow::new(); KEY_SLOTS];
    for (i, slot) in REPLAY_WINDOWS.iter().enumerate() {
      out[i] = *slot.borrow_ref(cs);
    }
    out
  })
}

// ============================================================
// 命令分发入口
// ============================================================

/// 命令来源：只用于日志区分，不影响业务
#[derive(Debug, Clone, Copy)]
pub enum CommandSource {
  /// 通过 BLE Control characteristic 写入
  Ble,
  /// 通过 ESP-NOW 广播/单播接收
  EspNow,
}

impl defmt::Format for CommandSource {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::Ble => defmt::write!(f, "BLE"),
      Self::EspNow => defmt::write!(f, "ESP-NOW"),
    }
  }
}

/// 把原始字节解码后分发到对应的处理逻辑
///
/// 传输层收到"可能是 Command 帧"的字节后调用此函数即可，无需自己解码。
///
/// # 处理流程
/// 1. `decode_command` 校验长度 / magic / version / CRC / HMAC
/// 2. **Anti-Replay 窗口检查**（本函数内，位于 HMAC 通过**之后**）
/// 3. `handle_command` 执行副作用 + 回 Ack
///
/// # 静默/告警策略
/// - `BadMagic` / `BadLength`：静默忽略（别人家的帧）
/// - `AuthFailed`：warn（可能是攻击，也可能是密钥不匹配）
/// - `ReplayError`：warn（可能是攻击，也可能是网络重传）
/// - 其它 decode error：warn
pub fn dispatch_command(src: CommandSource, raw: &[u8]) {
  let cmd = match decode_command(raw) {
    Ok(c) => c,
    Err(CommandDecodeError::BadMagic) | Err(CommandDecodeError::BadLength) => {
      // 静默忽略：ESP-NOW 空气里的其它帧、BLE Prepare Write 等
      return;
    }
    Err(e) => {
      warn!("[CTRL] decode error from {}: {}", src, e);
      return;
    }
  };

  // Anti-Replay 窗口检查 —— 必须在 decode 成功之后（保证是"合法签名"的帧）
  // O 选项：窗口 per-key-id，不同密钥下的 seq 彼此隔离
  if let Err(e) = check_replay(cmd.key_id, cmd.seq) {
    warn!(
      "[CTRL] replay rejected from {}: kid={} seq={} reason={}",
      src, cmd.key_id, cmd.seq, e
    );
    return;
  }

  // U 选项 + M-2 加固：仅当"本 slot 的 last_seq 相比上次落盘时至少推进了
  // REPLAY_PERSIST_INTERVAL"时才再次触发 replay-only 落盘。
  // 相较旧实现 `cmd.seq % REPLAY_PERSIST_INTERVAL == 0` 有两个优势：
  //   1) 用真实的 last_seq 而非命令 seq，避免乱序 seq 反复命中触发；
  //   2) 恶意攻击者即使构造 seq=100/200/300... 也不能重复触发写 flash。
  maybe_persist_replay(cmd.key_id);

  handle_command(src, cmd);
}

/// M-2：判断当前 key_id 的 replay 窗口是否已推进足够多 seq，若是则触发 flash 落盘
///
/// 采用 CAS 保证并发下同一进度只会触发一次 `mark_replay_dirty`。
fn maybe_persist_replay(key_id: KeyId) {
  let slot = key_id.as_u8() as usize;
  debug_assert!(slot < KEY_SLOTS, "key_id must be within KEY_SLOTS");

  // 取当前该 slot 的 last_seq（critical_section 内一次读，避免与 encode 冲突）
  let current_last_seq =
    critical_section::with(|cs| REPLAY_WINDOWS[slot].borrow_ref(cs).last_seq());

  let last_persisted = LAST_PERSISTED_LAST_SEQ[slot].load(Ordering::Relaxed);
  if current_last_seq.wrapping_sub(last_persisted) < REPLAY_PERSIST_INTERVAL {
    return;
  }

  // CAS：只有"我"把 last_persisted 推进到 current_last_seq 才真正触发一次 flash 落盘。
  // 若竞态失败，说明另一路径已经在写；直接放弃即可（幂等）。
  if LAST_PERSISTED_LAST_SEQ[slot]
    .compare_exchange(
      last_persisted,
      current_last_seq,
      Ordering::Relaxed,
      Ordering::Relaxed,
    )
    .is_ok()
  {
    crate::hal::persist::mark_replay_dirty(snapshot_replay_windows());
  }
}

/// 已解码 & 通过 anti-replay 的命令的分发中枢
///
/// **不允许长阻塞**：BLE 任务栈上直接调用；LED / Toast 之类的耗时动作通过
/// Signal 交给专门的 task 处理。
///
/// # 反馈流程
/// - 每次进入都会调用 [`touch_host_heartbeat`] 刷新心跳时间戳
/// - 每次完成分发若 [`AUTO_ACK`] 为 true，则往双链路（ESP-NOW + BLE）广播
///   [`CommandResponse::Ack`]，其中：
///   - `req_seq` **直接采用 Command 的 seq**（v3 起）
///   - `key_id`  **直接采用 Command 的 key_id**（O 选项）—— Host 可用同一密钥验签
pub fn handle_command(src: CommandSource, cmd: Command) {
  // 收到任意（已通过校验的）命令都算 Host "还活着"
  touch_host_heartbeat();

  // 执行命令；返回执行结果决定 Ack 还是 Error
  let result = execute(src, &cmd);

  // 自动回执：req_seq + key_id 均直接取自 Command（Host 可用同一 slot 的密钥验签）
  if AUTO_ACK.load(Ordering::Relaxed) {
    let response = match result {
      Ok(()) => CommandResponse::ack_with_key(cmd.seq, cmd.key_id),
      Err(code) => CommandResponse::err_with_key(cmd.seq, cmd.key_id, code),
    };
    broadcast_response(response);
  }
}
/// 双链路广播一条 [`CommandResponse`]（N 选项）
///
/// 同时写入 ESP-NOW 与 BLE 两个 Signal，两边后台任务各自 notify 到已连接的 Host。
///
/// # 为什么不用单 signal + 多订阅者？
/// `embassy_sync::Signal` 只允许一个 waiter；多个 task 同时 wait 会 panic。
/// 双 signal + 写两次代价极低（`CommandResponse` 是 `Copy`，~30 字节），
/// 而且两链路彼此解耦：任一链路断开时另一链路仍能正常送达。
///
/// # 双写无副作用
/// 若链路未连接，Signal 仅覆盖旧值不产生真实流量；连接建立时 [`ble_hid`] 与
/// [`esp_now`] 任务各自 reset 自己的 Signal，避免把旧会话 Response 遗留给新 Host。
pub fn broadcast_response(resp: CommandResponse) {
  esp_now::signal_response(resp);
  ble_hid::signal_response(resp);
}

/// 具体命令执行；返回 Ok 或错误码
///
/// # 为什么拆出这个函数？
/// - `handle_command` 负责统一的"心跳 + Ack"，此函数专注业务动作
/// - 便于每种命令返回自己的 `Result<(), ErrorCode>`
fn execute(src: CommandSource, cmd: &Command) -> Result<(), ErrorCode> {
  match cmd.kind {
    CommandBody::Nop => {
      // 空命令：仅用作心跳；已由 touch_host_heartbeat() 处理
      info!("[CTRL] Nop (heartbeat) seq={} from {}", cmd.seq, src);
      Ok(())
    }
    CommandBody::LedBlink {
      led_idx,
      count,
      period_ms,
    } => {
      // 参数校验：led_idx ∈ {0, 1}，count > 0，period 至少 20ms 才可视
      if led_idx > 1 {
        warn!("[CTRL] LedBlink invalid led_idx={} from {}", led_idx, src);
        return Err(ErrorCode::InvalidArgument);
      }
      if count == 0 || period_ms < 20 {
        return Err(ErrorCode::InvalidArgument);
      }
      info!(
        "[CTRL] LedBlink seq={} from {}: led={} count={} period={}ms",
        cmd.seq, src, led_idx, count, period_ms
      );
      signal_led_effect(led_idx, count, period_ms);
      Ok(())
    }
    CommandBody::SetSensitivity {
      joy_scale,
      knob_scale,
    } => {
      let joy_clamped = joy_scale.min(SENSITIVITY_MAX);
      let knob_clamped = knob_scale.min(SENSITIVITY_MAX);
      set_joy_sensitivity(joy_clamped);
      set_knob_sensitivity(knob_clamped);
      info!(
        "[CTRL] SetSensitivity seq={} from {}: joy={} knob={}",
        cmd.seq, src, joy_clamped, knob_clamped
      );
      mark_persist_dirty();
      Ok(())
    }
    CommandBody::ShowToast { len, bytes } => {
      let len = len.min(5);
      let msg = &bytes[..len as usize];
      signal_toast(msg);
      info!(
        "[CTRL] ShowToast seq={} from {}: len={} bytes={:?}",
        cmd.seq, src, len, msg
      );
      Ok(())
    }
    CommandBody::SetBatteryMode { simulate } => {
      BATTERY_SIMULATED.store(simulate, Ordering::Relaxed);
      info!(
        "[CTRL] SetBatteryMode seq={} from {}: simulate={}",
        cmd.seq, src, simulate
      );
      mark_persist_dirty();
      Ok(())
    }
  }
}

/// 便捷入口：把当前灵敏度 / 电池模式 / replay_windows 快照到持久化脏缓冲区
///
/// # 触发时机
/// - 执行完 `SetSensitivity` / `SetBatteryMode` 后（本文件内）
/// - 未来若有其它"应落盘"的关键事件也应在此调用
///
/// # 为什么不再传 `current_seq`？
/// U 选项后 `PersistentConfig::last_seq` 从 `replay_windows[0].last_seq()` 提取；
/// 直接在 [`snapshot_replay_windows`] 中一并拿到。
///
/// # 为什么读取 `Relaxed` 就够？
/// 灵敏度、电池模式已经在同一函数早前被更新过（且用了 Relaxed）；本函数
/// 只是把最新值传递给持久化模块，不需要与外部动作建立 happens-before 关系。
fn mark_persist_dirty() {
  crate::hal::persist::mark_dirty(
    JOY_SENSITIVITY.load(Ordering::Relaxed),
    KNOB_SENSITIVITY.load(Ordering::Relaxed),
    BATTERY_SIMULATED.load(Ordering::Relaxed),
    snapshot_replay_windows(),
  );
}
