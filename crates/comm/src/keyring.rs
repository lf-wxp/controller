//! # Keyring —— 运行时密钥槽状态
//!
//! ## 与 [`controller_protocol`] 的分工
//! `controller-protocol` 的 [`SHARED_SECRETS`](controller_protocol::config::keyring::SHARED_SECRETS)
//! 是 **编译期常量数组**（`build.rs` 从 `CONTROLLER_SECRET_V*` 环境变量注入）；
//! 密钥字节本身**不能在运行时改变**。
//!
//! 本模块只封装两件事：
//! 1. **当前 active `KeyId`**：出站 Command / NonceHello 会用它签名
//! 2. **每 slot 的 tx_counter**：Command 帧的 `seq` 从 1 单调递增；每个 key_id
//!    有独立的 seq 空间（对齐 protocol 顶部注释的"每个 key_id 拥有独立的 seq 空间"）
//!
//! ## 线程安全
//! - `active` 用 `AtomicU8`：跨任务读写，`Relaxed` 即可
//! - `tx_counters` 用 `[AtomicU32; KEY_SLOTS]`：`next_seq` 用 `fetch_add(Relaxed)`
//!   保证单调递增且不同任务互不覆盖

use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use controller_protocol::KeyId;
use controller_protocol::config::keyring::KEY_SLOTS;

/// 手柄默认使用的 `key_id`（=0，对应 `SHARED_SECRETS[0]`）
///
/// re-export 自 [`controller_protocol::config::keyring::DEFAULT_KEY_ID`]，避免调用方
/// 再依赖 protocol crate 的深层路径。
pub const DEFAULT_KEY_ID: u8 = controller_protocol::config::keyring::DEFAULT_KEY_ID;

/// 当 `active` 存储的原始字节意外越界时使用的 fallback slot 索引
///
/// 所有校验与 fallback 都以本常量为唯一真源，保证 [`Keyring::active`] 与
/// [`Keyring::next_seq`] 的兜底策略语义一致（两者过去分别退回
/// `KeyId::DEFAULT` 与 `slot 0`；若未来 `KeyId::DEFAULT` 不再是 0 就会撕裂）。
const FALLBACK_SLOT: usize = DEFAULT_KEY_ID as usize;

/// Keyring 相关错误
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyringError {
  /// 目标 slot 超出 [`KEY_SLOTS`] 范围
  SlotOutOfRange(u8),
  /// 目标 slot 的密钥字节被 `SHARED_SECRETS[i]` 显式设置为 `None`（已下线）
  SlotDisabled(u8),
}

#[cfg(feature = "defmt")]
impl defmt::Format for KeyringError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::SlotOutOfRange(id) => defmt::write!(f, "KeyringError::SlotOutOfRange({})", id),
      Self::SlotDisabled(id) => defmt::write!(f, "KeyringError::SlotDisabled({})", id),
    }
  }
}

/// 运行时密钥槽状态
///
/// # 生命周期语义
/// 通常放进 `static Keyring` 或 `StaticCell`；本身是纯原子字段，`&self` 就能改。
#[derive(Debug)]
pub struct Keyring {
  /// 当前 active key_id（`0..=KEY_ID_MAX`）
  active: AtomicU8,
  /// 每个 slot 的 tx_counter（下一个 seq 从这里 fetch_add 拿）
  tx_counters: [AtomicU32; KEY_SLOTS],
}

impl Keyring {
  /// 构造一个初始 keyring
  ///
  /// - `active = DEFAULT_KEY_ID`
  /// - 所有 `tx_counters = 0`（第一次 `next_seq()` 返回 1）
  #[must_use]
  pub const fn new() -> Self {
    Self {
      active: AtomicU8::new(DEFAULT_KEY_ID),
      tx_counters: [const { AtomicU32::new(0) }; KEY_SLOTS],
    }
  }

  /// 读取当前 active [`KeyId`]
  ///
  /// # Fallback
  /// `active` 字段在 [`Self::rotate_to`] 侧已经过范围与 slot 使能校验；
  /// 若原子字段被外部意外破坏，退回 [`FALLBACK_SLOT`] 对应的 `KeyId`（默认即
  /// [`DEFAULT_KEY_ID`]），与 [`Self::next_seq`] 的 fallback 策略保持一致。
  #[must_use]
  pub fn active(&self) -> KeyId {
    let raw = self.active.load(Ordering::Relaxed);
    KeyId::new(raw).unwrap_or_else(|_| {
      // KeyId::new 仅在 raw > KEY_ID_MAX 时返回 Err；此处 FALLBACK_SLOT 必然在合法范围
      KeyId::new(FALLBACK_SLOT as u8).unwrap_or(KeyId::DEFAULT)
    })
  }

  /// 切换当前 active `key_id`
  ///
  /// # Errors
  /// - [`KeyringError::SlotOutOfRange`]：`new_id.as_u8() >= KEY_SLOTS`
  /// - [`KeyringError::SlotDisabled`]：`SHARED_SECRETS[i] == None`
  ///
  /// # 副作用
  /// 切换 key 时**不重置**新 slot 的 tx_counter —— 每个 slot 的 seq 空间是**独立**的，
  /// 只在同一 slot 首次使用时从 0 开始。这样反复 rotate 时不会重放旧 seq。
  pub fn rotate_to(&self, new_id: KeyId) -> Result<(), KeyringError> {
    let raw = new_id.as_u8();
    if !new_id.is_slot_supported() {
      return Err(KeyringError::SlotOutOfRange(raw));
    }
    if controller_protocol::config::keyring::SHARED_SECRETS[raw as usize].is_none() {
      return Err(KeyringError::SlotDisabled(raw));
    }
    self.active.store(raw, Ordering::Relaxed);
    Ok(())
  }

  /// 获取当前 active slot 的下一个 seq（原子 fetch_add）
  ///
  /// # 语义
  /// - 返回值 `>= 1`；0 保留给"未初始化"
  /// - `wrapping_add`：即使溢出也不 panic；`AntiReplayWindow` 已经能处理边界
  ///
  /// # Fallback
  /// 若 `active` 字段意外越界，退回 [`FALLBACK_SLOT`] 的 tx_counter，与
  /// [`Self::active`] 的 fallback 策略同源。
  #[must_use]
  pub fn next_seq(&self) -> u32 {
    let slot = self.active.load(Ordering::Relaxed) as usize;
    let idx = if slot < KEY_SLOTS {
      slot
    } else {
      FALLBACK_SLOT
    };
    let prev = self.tx_counters[idx].fetch_add(1, Ordering::Relaxed);
    prev.wrapping_add(1)
  }

  /// 获取指定 slot 的下一个 seq（用于测试或自定义流程）
  #[must_use]
  pub fn next_seq_for(&self, key_id: KeyId) -> u32 {
    let idx = key_id.as_u8() as usize;
    if idx >= KEY_SLOTS {
      return 0;
    }
    let prev = self.tx_counters[idx].fetch_add(1, Ordering::Relaxed);
    prev.wrapping_add(1)
  }

  /// 只读快照：当前 tx_counter 值（用于 defmt 日志 / 单元测试）
  #[must_use]
  pub fn peek_counter(&self, key_id: KeyId) -> u32 {
    let idx = key_id.as_u8() as usize;
    if idx >= KEY_SLOTS {
      return 0;
    }
    self.tx_counters[idx].load(Ordering::Relaxed)
  }
}

impl Default for Keyring {
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
  fn default_keyring_starts_with_default_key_and_zero_counters() {
    let kr = Keyring::new();
    assert_eq!(kr.active().as_u8(), DEFAULT_KEY_ID);
    assert_eq!(kr.peek_counter(KeyId::DEFAULT), 0);
  }

  #[test]
  fn next_seq_is_monotonic_and_starts_at_one() {
    let kr = Keyring::new();
    assert_eq!(kr.next_seq(), 1);
    assert_eq!(kr.next_seq(), 2);
    assert_eq!(kr.next_seq(), 3);
    assert_eq!(kr.peek_counter(KeyId::DEFAULT), 3);
  }

  #[test]
  fn rotate_to_valid_slot_updates_active() {
    let kr = Keyring::new();
    let key1 = KeyId::new(1).unwrap();
    kr.rotate_to(key1)
      .expect("slot 1 is enabled in default config");
    assert_eq!(kr.active().as_u8(), 1);
  }

  #[test]
  fn rotate_to_disabled_slot_returns_err() {
    let kr = Keyring::new();
    // slot 2 在默认 config 下为 None
    let key2 = KeyId::new(2).unwrap();
    assert_eq!(kr.rotate_to(key2), Err(KeyringError::SlotDisabled(2)));
    // active 应该保持不变
    assert_eq!(kr.active().as_u8(), DEFAULT_KEY_ID);
  }

  #[test]
  fn per_slot_seq_spaces_are_independent() {
    let kr = Keyring::new();
    // slot 0 消耗 3
    assert_eq!(kr.next_seq(), 1);
    assert_eq!(kr.next_seq(), 2);
    assert_eq!(kr.next_seq(), 3);
    // 切到 slot 1，seq 从 1 重新开始
    let key1 = KeyId::new(1).unwrap();
    kr.rotate_to(key1).unwrap();
    assert_eq!(kr.next_seq(), 1);
    // 切回 slot 0，seq 继续从 4
    kr.rotate_to(KeyId::DEFAULT).unwrap();
    assert_eq!(kr.next_seq(), 4);
  }
}
