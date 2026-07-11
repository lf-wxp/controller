//! # Control Command 协议（Host → 手柄，反向通道）
//!
//! ## 帧格式（固定 24 字节，含 HMAC 认证 + 抗重放 seq + 密钥轮换 key_id + 10B payload）
//! ```text
//!  offset | size | field
//!  -------+------+-------------
//!    0    |  2   | magic (0xCB01, LE)
//!    2    |  1   | version_byte:
//!         |      |    bits[7..4] = key_id (0..=15)   —— O 选项
//!         |      |    bits[3..0] = protocol_version  (= 5)
//!    3    |  1   | kind (u8)
//!    4    |  4   | seq (u32 LE)
//!    8    | 10   | payload
//!   18    |  4   | hmac —— HMAC-SHA256(SHARED_SECRETS[key_id], nonce || bytes[0..18])[..4]
//!    22    |  2   | crc16_ibm(bytes[0..22]) (LE)
//! ```
//!
//! ## seq 语义
//! - Host 端维护 `tx_counter`，每次发命令前 `+1`，从 1 开始（**0 保留**为无效值）
//! - **每个 key_id 拥有独立的 seq 空间**：Host 切换 key_id 时应重置 tx_counter；
//!   手柄端 [`crate::transport::control::REPLAY_WINDOWS`] 也按 key_id 分开维护单独的滑动位图
//! - seq 也是 HMAC 计算范围的一部分 —— 篡改 seq 会同时导致 HMAC 失败
//!
//! ## 命令种类
//! - `Nop`                 —— 空命令（心跳/连接性检查）
//! - `LedBlink { .. }`     —— 令某颗 LED 闪烁 N 次
//! - `SetSensitivity { .. }` —— 修改摇杆 / 旋钮灵敏度（0..=1000 定点数）
//! - `ShowToast { .. }`    —— OLED 底部弹出一段短提示（最多 5 ASCII 字节）
//! - `SetBatteryMode { .. }` —— 切换电池模拟 / 真实模式
//! - `Announce`            —— Peer 发现广播，payload 全 0
//! - `AssignId { .. }`     —— 向指定 MAC 的 receiver 分配一个 receiver_id
//!
//! ## 校验顺序（decode 侧）
//! 1. 长度（`COMMAND_LEN`）
//! 2. Magic（快速过滤非 Command 帧）
//! 3. version_byte 拆分：version == 5？key_id 在 [`KEY_SLOTS`] 范围内？
//! 4. CRC（数据完整性；便宜过滤随机噪声）
//! 5. HMAC（身份认证；抗得伪造，根据 key_id 选密钥）
//! 6. Kind + payload 语义
//!
//! **抗重放窗口检查不在此文件内**——由 [`crate::transport::control::dispatch_command`]
//! 在 decode 成功之后统一执行（因为窗口是全局共享状态）。
//!
//! [`KEY_SLOTS`]: crate::config::keyring::KEY_SLOTS

use super::auth::{KeyId, compute_hmac_tag, verify_hmac_tag};
use super::crc::crc16_ibm;

use crate::config::auth::HMAC_TAG_LEN;
use crate::config::keyring::KEY_SLOTS;

/// Command 协议魔数
pub const COMMAND_MAGIC: u16 = 0xCB01;
/// Command 协议版本（payload 扩展到 10B，新增 Announce/AssignId）
pub const COMMAND_VERSION: u8 = 5;

/// 版本字段低 4 位掩码：bits[3..0] = protocol_version
const VERSION_NIBBLE_MASK: u8 = 0x0F;
/// 将 key_id 打包到版本字段高 4 位的偏移量
const KEY_ID_SHIFT: u8 = 4;
/// header 长度 = magic(2) + version(1) + kind(1) = 4
const HEADER_LEN: usize = 2 + 1 + 1;
/// seq 长度（bytes）
const SEQ_LEN: usize = 4;
/// payload 长度（bytes）
pub const PAYLOAD_LEN: usize = 10;
/// crc 长度（bytes）
const CRC_LEN: usize = 2;
/// 完整 Command 帧字节数（24 = header 4 + seq 4 + payload 10 + hmac 4 + crc 2）
pub const COMMAND_LEN: usize = HEADER_LEN + SEQ_LEN + PAYLOAD_LEN + HMAC_TAG_LEN + CRC_LEN;

// 各字段编译期偏移
const SEQ_OFFSET: usize = HEADER_LEN;
const PAYLOAD_OFFSET: usize = SEQ_OFFSET + SEQ_LEN;
const HMAC_OFFSET: usize = PAYLOAD_OFFSET + PAYLOAD_LEN;
const CRC_OFFSET: usize = HMAC_OFFSET + HMAC_TAG_LEN;

const _: () = assert!(COMMAND_LEN == 24);

// ============================================================
// CommandKind —— 命令种类枚举
// ============================================================

/// 命令种类 discriminant（对应 wire 上的第 3 字节 `kind`）
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
  /// 空命令（心跳）
  Nop = 0x00,
  /// LED 闪烁：`payload = [led_idx, count, period_lo, period_hi, 0, 0, 0, 0, 0, 0]`
  LedBlink = 0x01,
  /// 设置灵敏度：`payload = [joy_lo, joy_hi, knob_lo, knob_hi, 0, 0, 0, 0, 0, 0]`
  SetSensitivity = 0x02,
  /// 显示 Toast：`payload = [len (0..=5), b0, b1, b2, b3, b4, 0, 0, 0, 0]`
  ShowToast = 0x03,
  /// 切换电池模式：`payload = [simulate (0 = real, 非 0 = simulate), 0, 0, 0, 0, 0, 0, 0, 0, 0]`
  SetBatteryMode = 0x04,
  /// Peer 发现广播（controller 进入 Selecting 时广播）：payload 全 0
  Announce = 0x05,
  /// 向指定 MAC 的 receiver 分配 receiver_id：
  /// `payload = [receiver_id, mac[0..6], 0, 0, 0]`（MAC 共 6B，reserved 3B）
  AssignId = 0x06,
}

impl CommandKind {
  /// 从 wire 字节反查 kind
  pub const fn from_wire(byte: u8) -> Option<Self> {
    match byte {
      0x00 => Some(Self::Nop),
      0x01 => Some(Self::LedBlink),
      0x02 => Some(Self::SetSensitivity),
      0x03 => Some(Self::ShowToast),
      0x04 => Some(Self::SetBatteryMode),
      0x05 => Some(Self::Announce),
      0x06 => Some(Self::AssignId),
      _ => None,
    }
  }
}

// ============================================================
// Command —— 解码后的强类型命令
// ============================================================

/// 已解码的命令（含发送端 seq + key_id）
///
/// - `seq` 由 Host 端自行维护（每发一条 +1），手柄侧解码后交给
///   [`crate::transport::control::REPLAY_WINDOWS`]（per-key-id）做重放检查。
/// - `key_id` 描述本帧使用哪一份密钥计算 HMAC（O 选项）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
  /// Host 端 tx_counter 当前值（>= 1）
  pub seq: u32,
  /// 使用的密钥槽（O 选项）
  pub key_id: KeyId,
  /// 命令载荷（强类型）
  pub kind: CommandBody,
}

/// 命令载荷（按 kind 区分的强类型 payload）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandBody {
  /// 空命令
  Nop,
  /// LED 闪烁
  LedBlink {
    /// LED 索引（0 = LED1, 1 = LED2）
    led_idx: u8,
    /// 闪烁次数
    count: u8,
    /// 周期（毫秒；u16 LE）
    period_ms: u16,
  },
  /// 设置灵敏度
  SetSensitivity {
    /// 摇杆缩放（0..=1000，1000 = 100%）
    joy_scale: u16,
    /// 旋钮缩放（0..=1000）
    knob_scale: u16,
  },
  /// 显示 Toast
  ShowToast {
    /// 有效字节数（0..=5）
    len: u8,
    /// ASCII 字节内容
    bytes: [u8; 5],
  },
  /// 切换电池模式
  SetBatteryMode {
    /// true = 模拟递减；false = 真实测量
    simulate: bool,
  },
  /// Peer 发现广播（payload 全 0，手柄广播邀请 receivers 回发 AnnounceReply）
  Announce,
  /// 向指定 MAC 的 receiver 分配 receiver_id（广播下发，接收方自行匹配本机 MAC）
  AssignId {
    /// 目标 receiver 的 MAC-48 地址
    mac: [u8; 6],
    /// 手柄分配给该 receiver 的逻辑 ID（0..=31）
    receiver_id: u8,
  },
}

impl CommandBody {
  /// 取得命令种类 discriminant
  pub const fn kind(&self) -> CommandKind {
    match self {
      Self::Nop => CommandKind::Nop,
      Self::LedBlink { .. } => CommandKind::LedBlink,
      Self::SetSensitivity { .. } => CommandKind::SetSensitivity,
      Self::ShowToast { .. } => CommandKind::ShowToast,
      Self::SetBatteryMode { .. } => CommandKind::SetBatteryMode,
      Self::Announce => CommandKind::Announce,
      Self::AssignId { .. } => CommandKind::AssignId,
    }
  }
}

impl Command {
  /// 便捷构造：给定 seq + body（默认 key_id = 0，典型内部测试使用）
  #[must_use]
  pub const fn new(seq: u32, kind: CommandBody) -> Self {
    Self {
      seq,
      key_id: KeyId::DEFAULT,
      kind,
    }
  }

  /// 便捷构造：三元组（同时指定 key_id）
  #[must_use]
  pub const fn with_key(seq: u32, key_id: KeyId, kind: CommandBody) -> Self {
    Self { seq, key_id, kind }
  }
}

// ============================================================
// 解码错误
// ============================================================

/// Command 解码失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDecodeError {
  /// 缓冲区长度不等于 [`COMMAND_LEN`]
  BadLength,
  /// 魔数不匹配（不是 Command 帧，可能是 Frame 或干扰）
  BadMagic,
  /// 协议版本不支持（version_byte 低 4 位）
  UnsupportedVersion(u8),
  /// key_id 超出 [`KEY_SLOTS`] 范围（本固件不支持这个 slot）
  UnsupportedKeyId(u8),
  /// 命令种类未知（可能是新版本发的旧客户端不认识的命令）
  UnknownKind(u8),
  /// CRC 校验失败
  BadCrc { expected: u16, actual: u16 },
  /// HMAC 签名校验失败（认证失败，可能是伪造帧、密钥错误、或 key_id 已 revoke）
  AuthFailed,
  /// payload 内容不合法（例如 Toast len > 5）
  InvalidPayload,
}

#[cfg(feature = "defmt")]
impl defmt::Format for CommandDecodeError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::BadLength => defmt::write!(f, "CommandDecodeError::BadLength"),
      Self::BadMagic => defmt::write!(f, "CommandDecodeError::BadMagic"),
      Self::UnsupportedVersion(v) => {
        defmt::write!(f, "CommandDecodeError::UnsupportedVersion({})", v)
      }
      Self::UnsupportedKeyId(k) => {
        defmt::write!(f, "CommandDecodeError::UnsupportedKeyId({})", k)
      }
      Self::UnknownKind(k) => defmt::write!(f, "CommandDecodeError::UnknownKind(0x{:02x})", k),
      Self::BadCrc { expected, actual } => defmt::write!(
        f,
        "CommandDecodeError::BadCrc(exp=0x{:04x}, act=0x{:04x})",
        expected,
        actual
      ),
      Self::AuthFailed => defmt::write!(f, "CommandDecodeError::AuthFailed"),
      Self::InvalidPayload => defmt::write!(f, "CommandDecodeError::InvalidPayload"),
    }
  }
}

// ============================================================
// 编码 / 解码
// ============================================================

/// 将 [`Command`] 编码到 [`COMMAND_LEN`] 字节数组
///
/// # Panics
/// 当 `cmd.key_id` 对应的 slot 未启用（[`SHARED_SECRETS[i] == None`](crate::config::keyring::SHARED_SECRETS)）
/// 时 [`compute_hmac_tag`] 会返回 `None`，本函数会 `expect()` —— 编码时选一个未启用
/// 的 key_id 是一个编程错误（应在调用前确保 slot 启用）。
pub fn encode_command(cmd: &Command) -> [u8; COMMAND_LEN] {
  let mut buf = [0_u8; COMMAND_LEN];

  // header
  buf[0..2].copy_from_slice(&COMMAND_MAGIC.to_le_bytes());
  // version_byte = (key_id << 4) | (protocol_version & 0x0F)
  buf[2] = pack_version_byte(cmd.key_id, COMMAND_VERSION);
  buf[3] = cmd.kind.kind() as u8;

  // seq
  buf[SEQ_OFFSET..SEQ_OFFSET + SEQ_LEN].copy_from_slice(&cmd.seq.to_le_bytes());

  // payload
  let payload = encode_payload(&cmd.kind);
  buf[PAYLOAD_OFFSET..PAYLOAD_OFFSET + PAYLOAD_LEN].copy_from_slice(&payload);

  // hmac: 覆盖 header + seq + payload；按 key_id 选密钥
  let tag = compute_hmac_tag(&buf[..HMAC_OFFSET], cmd.key_id)
    .expect("encode_command called with a key_id whose slot is disabled");
  buf[HMAC_OFFSET..HMAC_OFFSET + HMAC_TAG_LEN].copy_from_slice(&tag);

  // crc: 覆盖 header + seq + payload + hmac（保护 hmac 本身不被无损篡改）
  let crc = crc16_ibm(&buf[..CRC_OFFSET]);
  buf[CRC_OFFSET..COMMAND_LEN].copy_from_slice(&crc.to_le_bytes());

  buf
}

/// 把 `(key_id, protocol_version)` 打包成一个 `u8`（与 wire 上的 version 字段对齐）
#[inline]
const fn pack_version_byte(key_id: KeyId, protocol_version: u8) -> u8 {
  (key_id.as_u8() << KEY_ID_SHIFT) | (protocol_version & VERSION_NIBBLE_MASK)
}

/// 从 wire 上的 version 字段拆出 `(key_id, protocol_version)`
#[inline]
const fn unpack_version_byte(byte: u8) -> (u8, u8) {
  let key_id_nibble = (byte >> KEY_ID_SHIFT) & VERSION_NIBBLE_MASK;
  let protocol_version = byte & VERSION_NIBBLE_MASK;
  (key_id_nibble, protocol_version)
}

fn encode_payload(body: &CommandBody) -> [u8; PAYLOAD_LEN] {
  let mut payload = [0_u8; PAYLOAD_LEN];
  match *body {
    CommandBody::Nop | CommandBody::Announce => {}
    CommandBody::LedBlink {
      led_idx,
      count,
      period_ms,
    } => {
      payload[0] = led_idx;
      payload[1] = count;
      payload[2..4].copy_from_slice(&period_ms.to_le_bytes());
    }
    CommandBody::SetSensitivity {
      joy_scale,
      knob_scale,
    } => {
      payload[0..2].copy_from_slice(&joy_scale.to_le_bytes());
      payload[2..4].copy_from_slice(&knob_scale.to_le_bytes());
    }
    CommandBody::ShowToast { len, bytes } => {
      payload[0] = len;
      payload[1..6].copy_from_slice(&bytes);
    }
    CommandBody::SetBatteryMode { simulate } => {
      payload[0] = u8::from(simulate);
    }
    CommandBody::AssignId { mac, receiver_id } => {
      // payload[0..6] = mac；payload[6] = receiver_id；payload[7..10] 保留
      payload[0..6].copy_from_slice(&mac);
      payload[6] = receiver_id;
    }
  }
  payload
}

/// 从字节切片解码 [`Command`]
///
/// # 校验顺序
/// 长度 → magic → version_byte(拆分 version+key_id) → CRC → HMAC → kind + payload
///
/// **抗重放窗口检查不在此函数内**——由调用方（`dispatch_command`）
/// 拿到 decode 后的 `cmd.seq` / `cmd.key_id` 交给全局
/// [`crate::transport::control::REPLAY_WINDOWS`] 判断。
pub fn decode_command(buf: &[u8]) -> Result<Command, CommandDecodeError> {
  if buf.len() != COMMAND_LEN {
    return Err(CommandDecodeError::BadLength);
  }

  let magic = u16::from_le_bytes([buf[0], buf[1]]);
  if magic != COMMAND_MAGIC {
    return Err(CommandDecodeError::BadMagic);
  }

  // v4：version 字节拆分为高 4 位 key_id + 低 4 位 protocol_version
  let (key_id_nibble, protocol_version) = unpack_version_byte(buf[2]);
  if protocol_version != COMMAND_VERSION {
    // 保留 buf[2] 原值用于报错，方便定位 v3/v4 不匹配
    return Err(CommandDecodeError::UnsupportedVersion(buf[2]));
  }
  if (key_id_nibble as usize) >= KEY_SLOTS {
    // 当前固件不支持的 slot（未来可能升级后支持）→ 拒绝
    return Err(CommandDecodeError::UnsupportedKeyId(key_id_nibble));
  }
  let key_id = KeyId::from_wire_nibble(key_id_nibble);

  // CRC 校验（廉价，先做）
  let expected_crc = crc16_ibm(&buf[..CRC_OFFSET]);
  let actual_crc = u16::from_le_bytes([buf[CRC_OFFSET], buf[CRC_OFFSET + 1]]);
  if expected_crc != actual_crc {
    return Err(CommandDecodeError::BadCrc {
      expected: expected_crc,
      actual: actual_crc,
    });
  }

  // HMAC 校验（较贵，后做；同时根据 key_id 选密钥）
  let tag_bytes = &buf[HMAC_OFFSET..HMAC_OFFSET + HMAC_TAG_LEN];
  if !verify_hmac_tag(&buf[..HMAC_OFFSET], tag_bytes, key_id) {
    return Err(CommandDecodeError::AuthFailed);
  }

  let kind_byte = buf[3];
  let kind = CommandKind::from_wire(kind_byte).ok_or(CommandDecodeError::UnknownKind(kind_byte))?;
  let seq = u32::from_le_bytes([
    buf[SEQ_OFFSET],
    buf[SEQ_OFFSET + 1],
    buf[SEQ_OFFSET + 2],
    buf[SEQ_OFFSET + 3],
  ]);
  let payload = &buf[PAYLOAD_OFFSET..PAYLOAD_OFFSET + PAYLOAD_LEN];

  let body = decode_body(kind, payload)?;
  Ok(Command {
    seq,
    key_id,
    kind: body,
  })
}

fn decode_body(kind: CommandKind, payload: &[u8]) -> Result<CommandBody, CommandDecodeError> {
  match kind {
    CommandKind::Nop => Ok(CommandBody::Nop),
    CommandKind::LedBlink => Ok(CommandBody::LedBlink {
      led_idx: payload[0],
      count: payload[1],
      period_ms: u16::from_le_bytes([payload[2], payload[3]]),
    }),
    CommandKind::SetSensitivity => Ok(CommandBody::SetSensitivity {
      joy_scale: u16::from_le_bytes([payload[0], payload[1]]),
      knob_scale: u16::from_le_bytes([payload[2], payload[3]]),
    }),
    CommandKind::ShowToast => {
      let len = payload[0];
      if len > 5 {
        return Err(CommandDecodeError::InvalidPayload);
      }
      let mut bytes = [0_u8; 5];
      bytes.copy_from_slice(&payload[1..6]);
      Ok(CommandBody::ShowToast { len, bytes })
    }
    CommandKind::SetBatteryMode => Ok(CommandBody::SetBatteryMode {
      simulate: payload[0] != 0,
    }),
    CommandKind::Announce => Ok(CommandBody::Announce),
    CommandKind::AssignId => {
      let mut mac = [0_u8; 6];
      mac.copy_from_slice(&payload[0..6]);
      let receiver_id = payload[6];
      if receiver_id >= 32 {
        // receiver_id 必须在 dest_mask u32 位图可寻址范围内
        return Err(CommandDecodeError::InvalidPayload);
      }
      Ok(CommandBody::AssignId { mac, receiver_id })
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::auth::AUTH_ENABLED;

  #[test]
  fn frame_length_is_24() {
    let bytes = encode_command(&Command::new(1, CommandBody::Nop));
    assert_eq!(bytes.len(), 24);
  }

  #[test]
  fn roundtrip_nop() {
    let cmd = Command::new(42, CommandBody::Nop);
    let bytes = encode_command(&cmd);
    assert_eq!(decode_command(&bytes), Ok(cmd));
  }

  #[test]
  fn roundtrip_led_blink() {
    let cmd = Command::new(
      100,
      CommandBody::LedBlink {
        led_idx: 1,
        count: 3,
        period_ms: 250,
      },
    );
    let bytes = encode_command(&cmd);
    assert_eq!(decode_command(&bytes), Ok(cmd));
  }

  #[test]
  fn roundtrip_sensitivity() {
    let cmd = Command::new(
      7,
      CommandBody::SetSensitivity {
        joy_scale: 750,
        knob_scale: 1000,
      },
    );
    let bytes = encode_command(&cmd);
    assert_eq!(decode_command(&bytes), Ok(cmd));
  }

  #[test]
  fn roundtrip_toast() {
    let cmd = Command::new(
      12345,
      CommandBody::ShowToast {
        len: 5,
        bytes: *b"HELLO",
      },
    );
    let bytes = encode_command(&cmd);
    assert_eq!(decode_command(&bytes), Ok(cmd));
  }

  #[test]
  fn seq_survives_roundtrip() {
    // seq 域必须准确编解码
    for &s in &[1_u32, 2, 100, 65535, 65536, u32::MAX] {
      let cmd = Command::new(s, CommandBody::Nop);
      let bytes = encode_command(&cmd);
      let decoded = decode_command(&bytes).unwrap();
      assert_eq!(decoded.seq, s);
    }
  }

  #[test]
  fn detect_bad_magic() {
    let mut bytes = encode_command(&Command::new(1, CommandBody::Nop));
    bytes[0] ^= 0xFF;
    assert_eq!(decode_command(&bytes), Err(CommandDecodeError::BadMagic));
  }

  #[test]
  fn detect_bad_crc() {
    let mut bytes = encode_command(&Command::new(
      1,
      CommandBody::LedBlink {
        led_idx: 0,
        count: 1,
        period_ms: 100,
      },
    ));
    // 篡改 payload；CRC 会先失败
    bytes[PAYLOAD_OFFSET + 1] ^= 0xFF;
    assert!(matches!(
      decode_command(&bytes),
      Err(CommandDecodeError::BadCrc { .. })
    ));
  }

  #[test]
  fn detect_auth_failed_when_crc_repaired() {
    // 篡改 seq 并同步修正 CRC，模拟"攻击者知道 CRC 但不知道密钥"
    let mut bytes = encode_command(&Command::new(1, CommandBody::Nop));
    bytes[SEQ_OFFSET] ^= 0xFF; // 篡改 seq
    let crc = crc16_ibm(&bytes[..CRC_OFFSET]);
    bytes[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    if AUTH_ENABLED {
      assert_eq!(decode_command(&bytes), Err(CommandDecodeError::AuthFailed));
    }
  }

  #[test]
  fn detect_unknown_kind() {
    // 构造合法版本 + 未知 kind，让它通过 HMAC + CRC 校验后触发 UnknownKind
    let mut buf = [0_u8; COMMAND_LEN];
    buf[0..2].copy_from_slice(&COMMAND_MAGIC.to_le_bytes());
    buf[2] = pack_version_byte(KeyId::DEFAULT, COMMAND_VERSION);
    buf[3] = 0x7F; // 未知 kind
    buf[SEQ_OFFSET..SEQ_OFFSET + SEQ_LEN].copy_from_slice(&1_u32.to_le_bytes());
    // payload 全 0
    let tag = compute_hmac_tag(&buf[..HMAC_OFFSET], KeyId::DEFAULT).unwrap();
    buf[HMAC_OFFSET..HMAC_OFFSET + HMAC_TAG_LEN].copy_from_slice(&tag);
    let crc = crc16_ibm(&buf[..CRC_OFFSET]);
    buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
      decode_command(&buf),
      Err(CommandDecodeError::UnknownKind(0x7F))
    );
  }

  // ---- O 选项：密钥轮换相关测试 ----

  #[test]
  fn version_byte_pack_unpack_roundtrip() {
    for raw_key in 0..=15_u8 {
      let key_id = KeyId::new(raw_key).unwrap();
      let byte = pack_version_byte(key_id, COMMAND_VERSION);
      let (got_key, got_ver) = unpack_version_byte(byte);
      assert_eq!(got_key, raw_key, "key_id round-trip broke at {}", raw_key);
      assert_eq!(got_ver, COMMAND_VERSION);
    }
  }

  #[test]
  fn v4_is_rejected() {
    // version_byte 低 4 位 protocol_version = 4，与当前协议版本（COMMAND_VERSION）不符，应被拒绝
    let mut bytes = encode_command(&Command::new(1, CommandBody::Nop));
    bytes[2] = 0x04; // 伪造 v4
    match decode_command(&bytes) {
      Err(CommandDecodeError::UnsupportedVersion(0x04)) => {}
      other => panic!("expected UnsupportedVersion(0x04), got {:?}", other),
    }
  }

  #[test]
  fn roundtrip_announce() {
    let cmd = Command::new(11, CommandBody::Announce);
    let bytes = encode_command(&cmd);
    assert_eq!(decode_command(&bytes), Ok(cmd));
  }

  #[test]
  fn roundtrip_assign_id() {
    let cmd = Command::new(
      12,
      CommandBody::AssignId {
        mac: [0x24, 0x0A, 0xC4, 0x11, 0x22, 0x33],
        receiver_id: 7,
      },
    );
    let bytes = encode_command(&cmd);
    assert_eq!(decode_command(&bytes), Ok(cmd));
  }

  #[test]
  fn assign_id_rejects_receiver_id_out_of_range() {
    // 手工构造 receiver_id = 32 的包 → InvalidPayload
    let mut buf = [0_u8; COMMAND_LEN];
    buf[0..2].copy_from_slice(&COMMAND_MAGIC.to_le_bytes());
    buf[2] = pack_version_byte(KeyId::DEFAULT, COMMAND_VERSION);
    buf[3] = CommandKind::AssignId as u8;
    buf[SEQ_OFFSET..SEQ_OFFSET + SEQ_LEN].copy_from_slice(&1_u32.to_le_bytes());
    // payload[0..6] = mac 全 0；payload[6] = 32 超出范围
    buf[PAYLOAD_OFFSET + 6] = 32;
    let tag = compute_hmac_tag(&buf[..HMAC_OFFSET], KeyId::DEFAULT).unwrap();
    buf[HMAC_OFFSET..HMAC_OFFSET + HMAC_TAG_LEN].copy_from_slice(&tag);
    let crc = crc16_ibm(&buf[..CRC_OFFSET]);
    buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
      decode_command(&buf),
      Err(CommandDecodeError::InvalidPayload)
    );
  }

  #[test]
  fn v3_is_rejected() {
    // 古老 v3 Host 发的包也必须拒绝
    let mut bytes = encode_command(&Command::new(1, CommandBody::Nop));
    bytes[2] = 0x03; // 伪造 v3
    match decode_command(&bytes) {
      Err(CommandDecodeError::UnsupportedVersion(0x03)) => {}
      other => panic!("expected UnsupportedVersion(0x03), got {:?}", other),
    }
  }

  #[test]
  fn roundtrip_with_non_default_key_id() {
    let cmd = Command::with_key(
      42,
      KeyId::new(1).unwrap(),
      CommandBody::LedBlink {
        led_idx: 1,
        count: 2,
        period_ms: 200,
      },
    );
    let bytes = encode_command(&cmd);
    let decoded = decode_command(&bytes).unwrap();
    assert_eq!(decoded, cmd);
    assert_eq!(decoded.key_id.as_u8(), 1);
  }

  #[test]
  fn reject_unsupported_key_id_slot() {
    // 手工构造一个包，key_id = KEY_SLOTS（当前不支持）
    let mut buf = [0_u8; COMMAND_LEN];
    buf[0..2].copy_from_slice(&COMMAND_MAGIC.to_le_bytes());
    // 启用高 4 位 = 4（未支持），低 4 位 = 4（当前版本）
    buf[2] = ((KEY_SLOTS as u8) << 4) | COMMAND_VERSION;
    buf[3] = CommandKind::Nop as u8;
    let crc = crc16_ibm(&buf[..CRC_OFFSET]);
    buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    match decode_command(&buf) {
      Err(CommandDecodeError::UnsupportedKeyId(k)) => assert_eq!(k as usize, KEY_SLOTS),
      other => panic!("expected UnsupportedKeyId, got {:?}", other),
    }
  }

  #[test]
  fn cross_key_id_signature_rejected() {
    // 用 key_id=0 签名，手工把 wire 上的 key_id 改为 1 → HMAC 校验应失败
    let mut bytes = encode_command(&Command::with_key(
      1,
      KeyId::new(0).unwrap(),
      CommandBody::Nop,
    ));
    // 将 wire version_byte 的 key_id 从 0 改为 1
    bytes[2] = pack_version_byte(KeyId::new(1).unwrap(), COMMAND_VERSION);
    // 修正 CRC（避免 CRC 先失败）
    let crc = crc16_ibm(&bytes[..CRC_OFFSET]);
    bytes[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    if crate::config::auth::AUTH_ENABLED {
      assert_eq!(decode_command(&bytes), Err(CommandDecodeError::AuthFailed));
    }
  }

  // ---- N-8：encode_command 在 slot 未启用时的 panic 覆盖 ----

  #[test]
  #[should_panic(expected = "slot is disabled")]
  fn encode_command_panics_on_disabled_slot() {
    // slot 2 在 SHARED_SECRETS 中为 None → encode_command 应触发 expect() panic
    let cmd = Command::with_key(1, KeyId::new(2).unwrap(), CommandBody::Nop);
    let _ = encode_command(&cmd);
  }
}
