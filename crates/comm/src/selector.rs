//! # Selector —— 目标 receiver 双状态选择器
//!
//! ## 双状态设计
//! - `pending`：**用户正在编辑**的选择（UI 层可能一直在改，不影响真实发送）
//! - `active`：**当前真正生效**的选择（`Notifier::send` 会用这个 mask）
//!
//! 用户按下"确认"时 [`commit`](Self::commit) 把 `pending` 拷贝到 `active`；
//! 按下"取消" [`cancel`](Self::cancel) 把 `pending` 恢复成 `active`。
//!
//! ## 与 [`PeerRegistry`](crate::PeerRegistry) 的关系
//! - `PeerRegistry` 决定 "**能选的**"（发现到的 peer 列表）
//! - `Selector` 决定 "**选中的**"（`dest_mask` 里哪些 bit 置 1）
//!
//! ## 为什么用 `AtomicU32` 而不是 `Mutex`
//! `dest_mask` 只是 32-bit 位图，`AtomicU32` 天然支持无锁读写；避免关中断开销。

use core::sync::atomic::{AtomicU32, Ordering};

use crate::peer_registry::MAX_PEERS;

/// 目标 receiver 的位图（每个 bit 对应一个 `receiver_id`）
pub type DestMask = u32;

/// 全部广播（所有 32 个 bit 都置 1）
pub const DEST_MASK_ALL: DestMask = u32::MAX;

/// 全部空选（没有任何目标）
pub const DEST_MASK_NONE: DestMask = 0;

/// 双状态目标选择器
///
/// # 语义
/// | 状态       | 语义                                               |
/// |------------|----------------------------------------------------|
/// | `active`   | Frame 发送时使用的 mask                             |
/// | `pending`  | UI 层正在编辑的 mask；`commit()` 前不影响 `active`   |
pub struct Selector {
  pending: AtomicU32,
  active: AtomicU32,
}

impl Selector {
  /// 构造一个初始 selector（`pending = active = DEST_MASK_NONE`）
  #[must_use]
  pub const fn new() -> Self {
    Self {
      pending: AtomicU32::new(DEST_MASK_NONE),
      active: AtomicU32::new(DEST_MASK_NONE),
    }
  }

  /// 构造一个"默认广播"selector
  #[must_use]
  pub const fn broadcast_all() -> Self {
    Self {
      pending: AtomicU32::new(DEST_MASK_ALL),
      active: AtomicU32::new(DEST_MASK_ALL),
    }
  }

  // ---- pending 操作 ----

  /// 读取 pending mask
  #[must_use]
  pub fn pending(&self) -> DestMask {
    self.pending.load(Ordering::Relaxed)
  }

  /// 覆盖 pending mask
  pub fn set_pending(&self, mask: DestMask) {
    self.pending.store(mask, Ordering::Relaxed);
  }

  /// 在 pending 中切换某个 `receiver_id`
  ///
  /// # 参数
  /// - `receiver_id`：必须 `< MAX_PEERS`；否则本调用无副作用
  pub fn toggle_pending(&self, receiver_id: u8) {
    if (receiver_id as usize) >= MAX_PEERS {
      return;
    }
    let bit = 1_u32 << receiver_id;
    // fetch_xor 是 lock-free 位翻转
    self.pending.fetch_xor(bit, Ordering::Relaxed);
  }

  // ---- active 操作 ----

  /// 读取 active mask（Notifier 发送时用这个）
  #[must_use]
  pub fn active(&self) -> DestMask {
    self.active.load(Ordering::Relaxed)
  }

  /// 直接覆盖 active mask（跳过 pending 编辑流程）
  ///
  /// # 副作用
  /// 为保持"active 是 pending 的已提交快照"的不变量，**同时**把 `pending`
  /// 覆写为相同值 —— 相当于隐式 [`commit`](Self::commit)。若后续需要独立
  /// 编辑，请直接调用 [`set_pending`](Self::set_pending) 或
  /// [`toggle_pending`](Self::toggle_pending)。
  pub fn set_active(&self, mask: DestMask) {
    self.active.store(mask, Ordering::Relaxed);
    self.pending.store(mask, Ordering::Relaxed);
  }

  // ---- 状态迁移 ----

  /// 把当前 pending 提交为 active
  pub fn commit(&self) {
    let p = self.pending.load(Ordering::Relaxed);
    self.active.store(p, Ordering::Relaxed);
  }

  /// 把 pending 恢复为 active（丢弃未提交的编辑）
  pub fn cancel(&self) {
    let a = self.active.load(Ordering::Relaxed);
    self.pending.store(a, Ordering::Relaxed);
  }

  // ---- 查询 helper ----

  /// 判断某 receiver 是否在 active mask 中被选中
  #[must_use]
  pub fn is_active_selected(&self, receiver_id: u8) -> bool {
    if (receiver_id as usize) >= MAX_PEERS {
      return false;
    }
    let bit = 1_u32 << receiver_id;
    self.active.load(Ordering::Relaxed) & bit != 0
  }

  /// pending 与 active 是否一致（UI 显示"未保存改动"用）
  #[must_use]
  pub fn is_dirty(&self) -> bool {
    self.pending.load(Ordering::Relaxed) != self.active.load(Ordering::Relaxed)
  }
}

impl Default for Selector {
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

  #[test]
  fn new_starts_empty() {
    let s = Selector::new();
    assert_eq!(s.pending(), 0);
    assert_eq!(s.active(), 0);
    assert!(!s.is_dirty());
  }

  #[test]
  fn toggle_pending_flips_bits() {
    let s = Selector::new();
    s.toggle_pending(0);
    s.toggle_pending(3);
    assert_eq!(s.pending(), 0b1001);
    s.toggle_pending(0);
    assert_eq!(s.pending(), 0b1000);
  }

  #[test]
  fn commit_promotes_pending_to_active() {
    let s = Selector::new();
    s.toggle_pending(1);
    s.toggle_pending(4);
    assert!(s.is_dirty());
    s.commit();
    assert_eq!(s.active(), 0b10010);
    assert!(!s.is_dirty());
  }

  #[test]
  fn cancel_reverts_pending() {
    let s = Selector::new();
    s.set_active(0b101);
    s.toggle_pending(2);
    assert_ne!(s.pending(), s.active());
    s.cancel();
    assert_eq!(s.pending(), s.active());
  }

  #[test]
  fn out_of_range_toggle_is_ignored() {
    let s = Selector::new();
    s.toggle_pending(32); // MAX_PEERS = 32，超界
    s.toggle_pending(200);
    assert_eq!(s.pending(), 0);
  }

  #[test]
  fn broadcast_all_constructor() {
    let s = Selector::broadcast_all();
    assert_eq!(s.active(), DEST_MASK_ALL);
    assert!(s.is_active_selected(0));
    assert!(s.is_active_selected(31));
  }
}
