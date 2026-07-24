//! # ReplayGuard —— per-key-id 抗重放窗口的实例化封装
//!
//! ## 与 [`protocol::AntiReplayWindow`] 的分工
//! [`AntiReplayWindow`] 提供**单个窗口**的算法实现（滑动位图 + `check_and_update`）；
//! 本模块把它包装成 **`[AntiReplayWindow; KEY_SLOTS]`** 数组，因为
//! [每个 `key_id` 拥有独立的 seq 空间](`protocol::command`)，
//! 密钥轮换时不能让新 slot 的 seq=1 被旧 slot 的 last_seq 误判为重放。
//!
//! ## 与手柄原代码 (`crates/controller/src/transport/control.rs::REPLAY_WINDOWS`) 的差异
//! - 原代码：`static REPLAY_WINDOWS: [Mutex<RefCell<AntiReplayWindow>>; KEY_SLOTS]`
//!   —— 全局单例，只能存一份，无法为不同 `Receiver` 实例开独立窗口
//! - 本模块：`ReplayGuard` 是**普通结构体**，谁 own 它谁维护自己的窗口，
//!   使得多设备 / 多接收器场景可以互不干扰
//!
//! ## 并发模型
//! - 内部用 `CriticalSectionRawMutex`：多任务 `check` 时短暂关中断
//! - 关中断范围仅覆盖"改一个窗口"的极短时间，不涉及 `.await`

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use protocol::config::keyring::KEY_SLOTS;
use protocol::{AntiReplayWindow, KeyId, ReplayError};

/// [`ReplayGuard::check`] 的失败原因
///
/// 相比直接复用 [`ReplayError`]，本枚举额外区分了"key_id 越界"与
/// "seq 已见"两类彻底不同的拒绝原因，便于上层打日志 / 计量。
///
/// 既有行为（两种情况均丢包）保持不变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayCheckError {
  /// `key_id.as_u8() >= KEY_SLOTS` —— 本机不支持的 slot
  KeyIdOutOfRange(u8),
  /// 底层 [`AntiReplayWindow`] 拒绝原因（`InvalidSeq` / `AlreadySeen` / `TooOld`）
  Window(ReplayError),
}

#[cfg(feature = "defmt")]
impl defmt::Format for ReplayCheckError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::KeyIdOutOfRange(id) => {
        defmt::write!(f, "ReplayCheckError::KeyIdOutOfRange({})", id)
      }
      Self::Window(e) => defmt::write!(f, "ReplayCheckError::Window({})", e),
    }
  }
}

impl From<ReplayError> for ReplayCheckError {
  fn from(e: ReplayError) -> Self {
    Self::Window(e)
  }
}

/// per-key-id 抗重放窗口容器
///
/// # 内存代价
/// `KEY_SLOTS * (u32 + u64) = 4 * 12 = 48 字节`；相对 MCU 的 KB 级内存可忽略。
pub struct ReplayGuard {
  windows: Mutex<CriticalSectionRawMutex, RefCell<[AntiReplayWindow; KEY_SLOTS]>>,
}

impl ReplayGuard {
  /// 构造一个"所有 slot last_seq = 0"的窗口组
  ///
  /// # `const fn` 保证
  /// 可以直接放进 `static` 或 `const`，无需运行时初始化。
  #[must_use]
  pub const fn new() -> Self {
    Self {
      windows: Mutex::new(RefCell::new([AntiReplayWindow::new(); KEY_SLOTS])),
    }
  }

  /// 从持久化快照恢复所有 slot 的窗口
  ///
  /// 手柄启动时调用一次，把 flash 里的 last_seq / bitmap 灌进内存。
  pub fn from_snapshot(snapshot: [AntiReplayWindow; KEY_SLOTS]) -> Self {
    Self {
      windows: Mutex::new(RefCell::new(snapshot)),
    }
  }

  /// **就地覆盖**所有 slot 的窗口状态
  ///
  /// 与 [`Self::from_snapshot`] 的区别：本方法作用于**已存在的实例**
  /// （例如 `static REPLAY: ReplayGuard = ReplayGuard::new()`），把 flash
  /// 里读出来的快照灌进去。这是手柄启动流程的关键接口 —— static 无法在
  /// 运行时"替换成新的 `ReplayGuard`"，只能"覆盖内容"。
  ///
  /// # 参数
  /// - `snapshot`：`KEY_SLOTS` 个 `AntiReplayWindow`，通常来自
  ///   手柄侧持久化配置的 `replay_windows`（应用 crate 提供）
  ///
  /// # 并发
  /// 在极短的 critical section 内完成一次赋值；不涉及 `.await`。
  pub fn restore_from_snapshot(&self, snapshot: [AntiReplayWindow; KEY_SLOTS]) {
    self.windows.lock(|cell| {
      *cell.borrow_mut() = snapshot;
    });
  }

  /// 校验并就地更新指定 slot 的窗口
  ///
  /// # 参数
  /// - `key_id`：帧上声明的 key_id（已由 protocol 层校验在 `0..=15` 范围）
  /// - `seq`：帧上的序列号
  ///
  /// # 返回
  /// - `Ok(())`：seq 未曾见过，已计入窗口
  /// - [`ReplayCheckError::KeyIdOutOfRange`]：`key_id.as_u8() >= KEY_SLOTS`（
  ///   本机不支持的 slot；上层可记录为"非法 key"）
  /// - [`ReplayCheckError::Window`]：底层窗口拒绝（`AlreadySeen` / `TooOld` / `InvalidSeq`）
  ///
  /// # Errors
  /// 任一拒绝原因都会导致报错；调用方应当直接丢弃帧（不需重试）。
  pub fn check(&self, key_id: KeyId, seq: u32) -> Result<(), ReplayCheckError> {
    let slot = key_id.as_u8() as usize;
    if slot >= KEY_SLOTS {
      return Err(ReplayCheckError::KeyIdOutOfRange(key_id.as_u8()));
    }
    self.windows.lock(|cell| {
      let mut arr = cell.borrow_mut();
      arr[slot]
        .check_and_update(seq)
        .map_err(ReplayCheckError::from)
    })
  }

  /// 只读快照：一次拿全部 slot 的窗口
  ///
  /// 用于把窗口状态落盘到 flash（U 选项）。
  #[must_use]
  pub fn snapshot(&self) -> [AntiReplayWindow; KEY_SLOTS] {
    self.windows.lock(|cell| *cell.borrow())
  }

  /// 只读快照：指定 slot 的当前 last_seq（用于日志 / 度量）
  #[must_use]
  pub fn last_seq(&self, key_id: KeyId) -> u32 {
    let slot = key_id.as_u8() as usize;
    if slot >= KEY_SLOTS {
      return 0;
    }
    self.windows.lock(|cell| cell.borrow()[slot].last_seq())
  }
}

impl Default for ReplayGuard {
  fn default() -> Self {
    Self::new()
  }
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
  use super::*;

  fn key(i: u8) -> KeyId {
    KeyId::new(i).expect("valid KeyId")
  }

  #[test]
  fn fresh_guard_accepts_first_seq() {
    let g = ReplayGuard::new();
    assert_eq!(g.check(key(0), 1), Ok(()));
    assert_eq!(g.last_seq(key(0)), 1);
  }

  #[test]
  fn independent_windows_per_key_id() {
    let g = ReplayGuard::new();
    // key 0 消耗 seq 1..=5
    for seq in 1..=5 {
      g.check(key(0), seq).unwrap();
    }
    // key 1 seq 1 应仍可用（独立空间）
    assert_eq!(g.check(key(1), 1), Ok(()));
    // key 0 seq 3 应被拒绝（已见过）
    assert!(g.check(key(0), 3).is_err());
  }

  #[test]
  fn snapshot_reflects_current_state() {
    let g = ReplayGuard::new();
    g.check(key(0), 100).unwrap();
    g.check(key(1), 42).unwrap();
    let snap = g.snapshot();
    assert_eq!(snap[0].last_seq(), 100);
    assert_eq!(snap[1].last_seq(), 42);
  }

  #[test]
  fn from_snapshot_restores_state() {
    let g1 = ReplayGuard::new();
    g1.check(key(0), 500).unwrap();
    let snap = g1.snapshot();

    let g2 = ReplayGuard::from_snapshot(snap);
    // 恢复后 seq=500 应被拒绝为 replay
    assert!(g2.check(key(0), 500).is_err());
    // 但 501 可用
    assert_eq!(g2.check(key(0), 501), Ok(()));
  }

  #[test]
  fn restore_from_snapshot_overwrites_in_place() {
    let g = ReplayGuard::new();
    // 先消耗 key 0 的 seq 1..=3
    for seq in 1..=3 {
      g.check(key(0), seq).unwrap();
    }
    assert_eq!(g.last_seq(key(0)), 3);

    // 用另一个 guard 造一份 last_seq=999 的快照
    let source = ReplayGuard::new();
    source.check(key(0), 999).unwrap();
    let snap = source.snapshot();

    // 就地覆盖当前 guard —— last_seq 应变成 999
    g.restore_from_snapshot(snap);
    assert_eq!(g.last_seq(key(0)), 999);
    // 覆盖后 seq=999 应被拒绝（已在快照里见过）
    assert!(g.check(key(0), 999).is_err());
    // 但 seq=1000 可用
    assert_eq!(g.check(key(0), 1000), Ok(()));
  }
}
