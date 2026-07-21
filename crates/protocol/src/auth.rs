//! # HMAC 认证原语（K 选项 + K3 Nonce 挑战 + O 密钥轮换）
//!
//! ## 职责
//! 给 [`Command`](super::Command) 与 [`CommandResponse`](super::CommandResponse)
//! 帧添加 4 字节 HMAC-SHA256 截断签名，防止空气中未授权设备伪造。
//!
//! ## 算法
//! ```text
//!   secret   = SHARED_SECRETS[key_id]         // O 选项：按 key_id 查密钥环
//!   full_tag = HMAC-SHA256(secret, session_nonce || message)
//!   tag      = full_tag[..HMAC_TAG_LEN]        // 取前 4 字节
//! ```
//!
//! - **K1**（历史）：单密钥 HMAC
//! - **K3**：session nonce 混入 HMAC 前缀，重启即换 nonce
//! - **O**：`SHARED_SECRETS` 数组按 `key_id` 索引，可平滑轮换（多密钥并存）
//!
//! ## Nonce 广播
//! 手柄每 [`crate::config::auth::NONCE_BROADCAST_INTERVAL_MS`] 通过 ESP-NOW
//! 广播一次 [`ResponseKind::NonceHello`](super::response::ResponseKind::NonceHello)
//! 帧携带当前 nonce；Host 收到后同步更新自身缓存并重置 tx_counter。
//!
//! ## 常时比较
//! 校验函数 [`verify_hmac_tag`] 使用**常时比较**（constant-time compare），
//! 避免通过时序侧信道推断"前 N 字节匹配"。
//!
//! ## 认证开关
//! [`crate::config::auth::AUTH_ENABLED`] 为 `false` 时，[`verify_hmac_tag`]
//! 无条件返回 `true` —— 用于调试时手工构造包。生产环境必须开启。

use core::sync::atomic::{AtomicU32, Ordering};

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::config::auth::{AUTH_ENABLED, HMAC_TAG_LEN};
use crate::config::keyring::{KEY_SLOTS, SHARED_SECRETS};

/// HMAC-SHA256 类型别名（避免每处都写完整泛型）
type HmacSha256 = Hmac<Sha256>;

/// Session nonce 长度（字节，K3 选项）
///
/// 与 `AtomicU32` 天然吻合，4 字节 = 32 bit 熵。攻击者对同一 nonce 下的
/// HMAC 空间仍是 2^32 猜测；nonce 每次开机重置就把老的攻击成果作废。
pub const SESSION_NONCE_LEN: usize = 4;

// ============================================================
// KeyId —— 4-bit newtype（O 选项）
// ============================================================

/// wire 上 key_id 字段的最大取值（受限于 4-bit 编码）
pub const KEY_ID_MAX: u8 = 0x0F;

/// 密钥槽标识（O 选项：HMAC 密钥轮换）
///
/// # newtype 目的
/// 防止 `u8` 与"业务上的其它 u8"混淆（比如与 kind 字节撞名）；
/// 内部通过 [`KeyId::new`] 强制校验 0..=15 范围。
///
/// # wire 编码
/// 在 [`super::Command`] / [`super::CommandResponse`] 的 version 字节里
/// 占高 4 位：`byte = (key_id << 4) | protocol_version`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct KeyId(u8);

impl KeyId {
  /// 构造 [`KeyId`]；范围 `0..=15`（对应 wire 上 4-bit key_id）
  ///
  /// # Errors
  /// 当 `raw > 15` 时返回 [`KeyIdError::OutOfRange`]。
  pub const fn new(raw: u8) -> Result<Self, KeyIdError> {
    if raw > KEY_ID_MAX {
      return Err(KeyIdError::OutOfRange(raw));
    }
    Ok(Self(raw))
  }

  /// 无校验直接构造（**仅供 wire 解码路径使用**——已从 4-bit 拆解出来）
  #[inline]
  pub(crate) const fn from_wire_nibble(nibble: u8) -> Self {
    // 内部路径：调用方保证 nibble & 0x0F 已限制在 0..=15
    Self(nibble & KEY_ID_MAX)
  }

  /// 读取原始 u8（0..=15）
  #[inline]
  #[must_use]
  pub const fn as_u8(self) -> u8 {
    self.0
  }

  /// 是否落在当前固件的 [`KEY_SLOTS`] 范围内
  #[inline]
  #[must_use]
  pub const fn is_slot_supported(self) -> bool {
    (self.0 as usize) < KEY_SLOTS
  }

  /// 默认主密钥 id（0）——用于手柄主动出站（NonceHello 等无对应 Command 的帧）
  pub const DEFAULT: Self = Self(crate::config::keyring::DEFAULT_KEY_ID);
}

#[cfg(feature = "defmt")]
impl defmt::Format for KeyId {
  fn format(&self, f: defmt::Formatter<'_>) {
    defmt::write!(f, "KeyId({})", self.0);
  }
}

/// [`KeyId::new`] 的失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIdError {
  /// 传入的原始 u8 超过 15，无法编码到 wire 上的 4 位字段
  OutOfRange(u8),
}

#[cfg(feature = "defmt")]
impl defmt::Format for KeyIdError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::OutOfRange(raw) => defmt::write!(f, "KeyIdError::OutOfRange({})", raw),
    }
  }
}

/// 根据 [`KeyId`] 查找对应密钥
///
/// - `Some(&[u8; 32])`：该 slot 已启用，可用作 HMAC 密钥
/// - `None`：该 slot 未启用或已 revoke，任何 HMAC 都应视为失败
#[inline]
fn lookup_key(key_id: KeyId) -> Option<&'static [u8; 32]> {
  let idx = key_id.as_u8() as usize;
  if idx >= KEY_SLOTS {
    return None;
  }
  SHARED_SECRETS[idx]
}

// ============================================================
// Session Nonce（K3 选项）
// ============================================================

/// 全局 session nonce（K3）
///
/// 手柄启动时由 [`init_session_nonce`] 初始化；一旦初始化就**不再改变**，
/// 直到手柄断电重启。设计原则：
///
/// - **进程内不可变**：所有任务读到的是同一 nonce，HMAC 计算/校验前后一致
/// - **重启即更换**：即使密钥被 dump，旧密钥 + 老 nonce 抓的包在下次开机后
///   都无法通过校验
/// - **无锁读取**：`Ordering::Relaxed` 足够——nonce 是启动阶段一次性写入的
///   常量，全生命周期不再改变
///
/// # 默认值
/// `0` 是**哨兵值**，表示"尚未初始化"。这时 [`compute_hmac_tag`] 会用它计算
/// 但产生的 tag 也会跟 Host 侧不一致——所以务必在 spawn 任何 task 之前调用
/// [`init_session_nonce`]。
pub static SESSION_NONCE: AtomicU32 = AtomicU32::new(0);

/// 初始化 session nonce（**必须**在启动阶段调用一次）
///
/// # 参数
/// - `seed`：熵源（通常来自硬件 RNG / 启动时钟抖动 / 未初始化 SRAM 内容等）
///
/// # 幂等性
/// 二次调用会覆盖首次值；出于安全考虑，调用方应保证只调用一次。
pub fn init_session_nonce(seed: u32) {
  // seed = 0 会退化到哨兵值；用 `max(1)` 避免所有 seed 撞车到默认值 0
  //
  // M-1: 使用 `Release` 保证在 spawn 任何 task 之前该写入对所有读取端可见；
  // 与 `session_nonce()` 的 `Acquire` 读配对形成 happens-before 关系。
  SESSION_NONCE.store(seed.max(1), Ordering::Release);
}

/// 读取当前 session nonce
///
/// # Ordering
/// 使用 `Acquire` 与 [`init_session_nonce`] 的 `Release` 配对，保证首次
/// 读取到 nonce 之后所有后续读取（比如密钥、命令处理路径）都能看到 init
/// 时点之前的所有写入。
#[inline]
#[must_use]
pub fn session_nonce() -> u32 {
  SESSION_NONCE.load(Ordering::Acquire)
}

// ============================================================
// HMAC 计算 / 校验
// ============================================================

/// 计算消息的 HMAC 截断签名
///
/// # 参数
/// - `message`：要签名的字节序列（Command/Response 除 hmac + crc 之外的所有字段）
/// - `key_id`：使用哪个 slot 的密钥；调用方应保证该 slot 已启用
///
/// # 返回
/// - `Some([u8; HMAC_TAG_LEN])`：密钥可用，返回签名
/// - `None`：该 `key_id` 对应的 slot 未启用（`SHARED_SECRETS[i] == None`），
///   无法生成合法签名 —— 调用方（编码路径）应把这种情况视为编程错误
///
/// # Nonce 混入（K3）
/// 内部会读取全局 [`SESSION_NONCE`] 并把它的 LE 字节序列作为 HMAC 输入
/// 的**前缀**：`HMAC(secret, nonce_bytes || message)`。
#[must_use]
pub fn compute_hmac_tag(message: &[u8], key_id: KeyId) -> Option<[u8; HMAC_TAG_LEN]> {
  let secret = lookup_key(key_id)?;
  // 从密钥新建 MAC 状态；new_from_slice 只有在密钥长度不合法时才失败，
  // 但 HMAC 允许任意长度密钥（内部会做 padding/hash），所以 expect 是安全的
  let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
  // 前缀：session_nonce（LE 4 字节）
  let nonce = session_nonce().to_le_bytes();
  mac.update(&nonce);
  // 消息本体
  mac.update(message);
  let full = mac.finalize().into_bytes(); // 32 bytes

  let mut tag = [0_u8; HMAC_TAG_LEN];
  tag.copy_from_slice(&full[..HMAC_TAG_LEN]);
  Some(tag)
}

/// 校验消息的 HMAC 截断签名
///
/// # 参数
/// - `message`：帧中除 hmac 与 crc 之外的所有字节
/// - `expected_tag`：帧中读到的 hmac 字节
/// - `key_id`：命令 wire 上声明的 key_id
///
/// # 返回
/// - `true`：签名匹配，或 [`AUTH_ENABLED`] = false（调试后门）
/// - `false`：签名不匹配 / key_id 对应密钥未启用 / tag 长度错误
///
/// # 常时比较
/// 内部用 [`constant_time_eq`]，即使部分字节匹配也不会短路，
/// 防止通过响应时间推断签名内容。
///
/// # AUTH_ENABLED = false 的行为
/// 直接返回 `true`——**注意**：调试模式下仍然会跳过 key_id 检查，
/// 生产环境必须打开认证。
#[must_use]
pub fn verify_hmac_tag(message: &[u8], expected_tag: &[u8], key_id: KeyId) -> bool {
  if !AUTH_ENABLED {
    // 调试后门：跳过校验
    return true;
  }
  if expected_tag.len() != HMAC_TAG_LEN {
    return false;
  }
  let Some(computed) = compute_hmac_tag(message, key_id) else {
    // key_id 对应 slot 未启用 → 直接失败
    return false;
  };
  constant_time_eq(&computed, expected_tag)
}

/// 常时比较：两个字节切片长度相同且逐字节相等
///
/// **不能短路**：无论前面哪一位不同，都要走完全部字节的异或累加，
/// 才 return 最终结果 —— 抵御时序侧信道攻击。
///
/// 手写实现避免引入 `subtle` crate 依赖（该 crate 已在 lockfile 里，
/// 但直接依赖会引入额外的编译单元开销；本函数逻辑简单，手写更清晰）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
  if a.len() != b.len() {
    return false;
  }
  let mut diff: u8 = 0;
  for (x, y) in a.iter().zip(b.iter()) {
    diff |= x ^ y;
  }
  diff == 0
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 测试辅助：临时把 SESSION_NONCE 设为已知值再跑 body
  ///
  /// # 并发注意
  /// 单元测试默认多线程并发跑（`cargo test` 的 libtest harness）。多个测试
  /// 共同修改 `SESSION_NONCE` 会互相干扰。**运行本模块单测请加参数
  /// `--test-threads=1`**：
  ///
  /// ```bash
  /// cargo test -p protocol -- --test-threads=1
  /// ```
  ///
  /// 手柄单线程环境（no_std, embassy 单执行器）下不存在此问题；这是仅在
  /// 宿主机测试环境下的约束。
  fn with_nonce(nonce: u32) {
    SESSION_NONCE.store(nonce, Ordering::Relaxed);
  }

  /// 默认可用密钥 slot（config 里 slot 0 为 Some(SECRET_V1)）
  fn key0() -> KeyId {
    KeyId::new(0).expect("KeyId(0) always in range")
  }

  /// 备用可用密钥 slot（config 里 slot 1 为 Some(SECRET_V2)）
  fn key1() -> KeyId {
    KeyId::new(1).expect("KeyId(1) always in range")
  }

  /// 未启用的 slot（config 里 slot 2 为 None）
  fn key_disabled() -> KeyId {
    KeyId::new(2).expect("KeyId(2) always in range")
  }

  #[test]
  fn key_id_range_enforced() {
    assert!(KeyId::new(0).is_ok());
    assert!(KeyId::new(15).is_ok());
    assert_eq!(KeyId::new(16), Err(KeyIdError::OutOfRange(16)));
    assert_eq!(KeyId::new(255), Err(KeyIdError::OutOfRange(255)));
  }

  #[test]
  fn key_id_default_is_zero() {
    assert_eq!(KeyId::DEFAULT.as_u8(), 0);
  }

  #[test]
  fn hmac_deterministic_with_key_id() {
    with_nonce(0xDEAD_BEEF);
    let msg = b"hello world";
    let tag1 = compute_hmac_tag(msg, key0()).unwrap();
    let tag2 = compute_hmac_tag(msg, key0()).unwrap();
    assert_eq!(tag1, tag2);
    assert_eq!(tag1.len(), HMAC_TAG_LEN);
  }

  #[test]
  fn hmac_differs_by_message() {
    with_nonce(0xDEAD_BEEF);
    let t1 = compute_hmac_tag(b"foo", key0()).unwrap();
    let t2 = compute_hmac_tag(b"bar", key0()).unwrap();
    assert_ne!(t1, t2);
  }

  #[test]
  fn hmac_differs_by_nonce() {
    // K3 核心断言
    with_nonce(0x1111_1111);
    let t1 = compute_hmac_tag(b"same message", key0()).unwrap();
    with_nonce(0x2222_2222);
    let t2 = compute_hmac_tag(b"same message", key0()).unwrap();
    assert_ne!(t1, t2);
  }

  #[test]
  fn hmac_differs_by_key_id() {
    // O 核心断言：不同 key_id 使用不同 SECRET → 不同 tag
    with_nonce(0xCAFE_BABE);
    let t0 = compute_hmac_tag(b"same message", key0()).unwrap();
    let t1 = compute_hmac_tag(b"same message", key1()).unwrap();
    assert_ne!(t0, t1);
  }

  #[test]
  fn hmac_none_for_disabled_slot() {
    with_nonce(0xCAFE_BABE);
    assert!(compute_hmac_tag(b"anything", key_disabled()).is_none());
  }

  #[test]
  fn verify_accepts_valid_tag() {
    with_nonce(0xCAFE_BABE);
    let msg = b"authenticated message";
    let tag = compute_hmac_tag(msg, key0()).unwrap();
    assert!(verify_hmac_tag(msg, &tag, key0()));
  }

  #[test]
  fn verify_rejects_tampered_tag() {
    with_nonce(0xCAFE_BABE);
    let msg = b"authenticated message";
    let mut tag = compute_hmac_tag(msg, key0()).unwrap();
    tag[0] ^= 0xFF;
    if AUTH_ENABLED {
      assert!(!verify_hmac_tag(msg, &tag, key0()));
    }
  }

  #[test]
  fn verify_rejects_tampered_message() {
    with_nonce(0xCAFE_BABE);
    let msg = b"authenticated message";
    let tag = compute_hmac_tag(msg, key0()).unwrap();
    let tampered = b"authenticated message!";
    if AUTH_ENABLED {
      assert!(!verify_hmac_tag(tampered, &tag, key0()));
    }
  }

  #[test]
  fn verify_rejects_after_nonce_change() {
    with_nonce(0xAAAA_AAAA);
    let msg = b"replay after restart";
    let old_tag = compute_hmac_tag(msg, key0()).unwrap();
    with_nonce(0xBBBB_BBBB);
    if AUTH_ENABLED {
      assert!(!verify_hmac_tag(msg, &old_tag, key0()));
    }
  }

  #[test]
  fn verify_rejects_wrong_key_id() {
    // 用 key_id=0 签，用 key_id=1 校验 → 应失败
    with_nonce(0xCAFE_BABE);
    let msg = b"cross-key attack";
    let tag = compute_hmac_tag(msg, key0()).unwrap();
    if AUTH_ENABLED {
      assert!(!verify_hmac_tag(msg, &tag, key1()));
    }
  }

  #[test]
  fn verify_rejects_disabled_key_id() {
    // key_id 指向未启用的 slot → 无论 tag 是什么都拒绝
    with_nonce(0xCAFE_BABE);
    let bogus_tag = [0_u8; HMAC_TAG_LEN];
    if AUTH_ENABLED {
      assert!(!verify_hmac_tag(b"whatever", &bogus_tag, key_disabled()));
    }
  }

  #[test]
  fn init_session_nonce_avoids_zero_sentinel() {
    init_session_nonce(0);
    assert_ne!(session_nonce(), 0);
  }

  #[test]
  fn constant_time_eq_handles_len_mismatch() {
    assert!(!constant_time_eq(b"abc", b"ab"));
    assert!(!constant_time_eq(b"ab", b"abc"));
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(constant_time_eq(&[], &[]));
  }
}
