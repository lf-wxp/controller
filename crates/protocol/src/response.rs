//! # Command Response 协议（手柄 → Host，反向反馈）
//!
//! ## 帧格式（固定 24 字节，含 HMAC 认证 + key_id + 10B payload）
//! ```text
//!  offset | size | field
//!  -------+------+-------------
//!    0    |  2   | magic (0xCB02, LE)
//!    2    |  1   | version_byte:
//!         |      |    bits[7..4] = key_id (0..=15)  —— O 选项
//!         |      |    bits[3..0] = protocol_version (= 5)
//!    3    |  4   | req_seq (LE) —— 对应 Command 的 seq（无对应时为 0，如 NonceHello）
//!    7    |  1   | kind (u8) —— Response 种类
//!    8    | 10   | payload —— 按 kind 解释
//!   18    |  4   | hmac —— HMAC-SHA256(SHARED_SECRETS[key_id], nonce || bytes[0..18])[..4]
//!   22    |  2   | crc16_ibm(bytes[0..22]) (LE)
//! ```
//!
//! ## 消息类型对照
//! | 类型             | 长度 | Magic  | 版本低位 | 方向               |
//! |------------------|------|--------|-----------|--------------------|
//! | Frame            | 25 B | 0xC71E |  2        | 手柄 → Host 广播   |
//! | Command          | 24 B | 0xCB01 |  5        | Host → 手柄        |
//! | CommandResponse  | 24 B | 0xCB02 |  5        | 手柄 → Host（反馈）|
//!
//! ## `req_seq` 的语义
//! `req_seq` **直接取自请求 Command 的 seq 字段**（不再是手柄侧独立计数器）。
//! 这样 Host 可以精确知道"我发的 seq=42 那条命令收到了什么响应"，无需额外映射。
//!
//! ## Response 种类
//! - `Ack`             —— 命令已成功执行
//! - `Error(code)`     —— 命令执行失败，payload[0] = 错误码
//! - `BatterySnapshot` —— 电量快照，payload[0] = 电量 %
//! - `NonceHello`      —— K3：session nonce 主动广播（`req_seq = 0`）
//! - `AnnounceReply`   —— receiver 回应 controller 的 Announce：
//!   `payload = [mac[0..6], rssi_dbm(i8), role_tag[3]]`
//!
//! ## key_id 语义（O 选项）
//! - **回执类响应**（Ack / Error / BatterySnapshot / AnnounceReply）使用**请求 Command 的 key_id**
//!   —— Host 用同一份密钥就能验签
//! - **主动广播**（NonceHello）使用 [`crate::config::keyring::DEFAULT_KEY_ID`]
//!
//! ## NonceHello 广播帧（K3 选项）
//! 手柄启动 + 每 [`crate::config::auth::NONCE_BROADCAST_INTERVAL_MS`] 广播一次：
//! - `req_seq = 0`（保留值，表示"非请求响应"）
//! - `payload[0..4] = session_nonce.to_le_bytes()`
//! - `payload[4..10]` 保留，填 0
//!
//! Host 侧收到后：
//! 1. 若 nonce 变了（手柄重启）→ 更新缓存 + 重置 `tx_counter`
//! 2. 用最新 nonce 参与后续所有 Command 的 HMAC 计算

use super::auth::{KeyId, compute_hmac_tag, verify_hmac_tag};
use super::crc::crc16_ibm;

use crate::config::auth::HMAC_TAG_LEN;
use crate::config::keyring::KEY_SLOTS;

/// Response 协议魔数
pub const RESPONSE_MAGIC: u16 = 0xCB02;
/// Response 协议版本
pub const RESPONSE_VERSION: u8 = 5;

/// version 字段低 4 位掩码：bits[3..0] = protocol_version
const VERSION_NIBBLE_MASK: u8 = 0x0F;
/// 将 key_id 打包到 version 字段高 4 位的偏移量
const KEY_ID_SHIFT: u8 = 4;

const HEADER_LEN: usize = 2 + 1 + 4 + 1;
/// payload 长度
pub const PAYLOAD_LEN: usize = 10;
const CRC_LEN: usize = 2;
/// 完整 Response 帧字节数（24 = header 8 + payload 10 + hmac 4 + crc 2）
pub const RESPONSE_LEN: usize = HEADER_LEN + PAYLOAD_LEN + HMAC_TAG_LEN + CRC_LEN;

const HMAC_OFFSET: usize = HEADER_LEN + PAYLOAD_LEN;
const CRC_OFFSET: usize = HMAC_OFFSET + HMAC_TAG_LEN;

const _: () = assert!(RESPONSE_LEN == 24);

// ============================================================
// ResponseKind
// ============================================================

/// Response 种类 discriminant（wire 上的第 7 字节 `kind`）
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
  /// 命令已成功执行 —— payload 全 0
  Ack = 0x00,
  /// 命令执行失败 —— payload[0] = 错误码，剩余保留
  Error = 0x01,
  /// 电量快照 —— payload[0] = 电量 %，剩余保留
  BatterySnapshot = 0x02,
  /// K3 Session Nonce 广播 —— payload[0..4] = nonce LE，剩余保留
  NonceHello = 0x03,
  /// receiver 回应 controller Announce —— payload = [mac(6B), rssi(1B), role_tag(3B)]
  AnnounceReply = 0x04,
}

impl ResponseKind {
  /// 从 wire 字节反查 kind
  pub const fn from_wire(byte: u8) -> Option<Self> {
    match byte {
      0x00 => Some(Self::Ack),
      0x01 => Some(Self::Error),
      0x02 => Some(Self::BatterySnapshot),
      0x03 => Some(Self::NonceHello),
      0x04 => Some(Self::AnnounceReply),
      _ => None,
    }
  }
}

// ============================================================
// ErrorCode —— Error response 的 payload[0]
// ============================================================

/// 命令执行错误码
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
  /// 参数不合法（例如 LedBlink 的 led_idx 越界）
  InvalidArgument = 0x01,
  /// 命令暂不支持
  Unsupported = 0x02,
  /// 内部忙（例如 LED 特效队列已满）
  Busy = 0x03,
}

impl ErrorCode {
  pub const fn from_wire(byte: u8) -> Option<Self> {
    match byte {
      0x01 => Some(Self::InvalidArgument),
      0x02 => Some(Self::Unsupported),
      0x03 => Some(Self::Busy),
      _ => None,
    }
  }
}

// ============================================================
// CommandResponse —— 解码后的强类型
// ============================================================

/// 已解码的命令响应
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandResponse {
  /// 对应请求命令的 counter 值（手柄侧维护）；主动广播（如 NonceHello）时为 0
  pub req_seq: u32,
  /// 使用的密钥槽（O 选项）
  ///
  /// - 回执类响应（Ack / Error / BatterySnapshot）：与请求 Command 的 `key_id` 一致
  /// - 主动广播（NonceHello）：默认 [`KeyId::DEFAULT`]
  pub key_id: KeyId,
  /// 响应种类 + 载荷
  pub body: ResponseBody,
}

/// Response body（按 kind 区分的强类型 payload）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseBody {
  /// 命令成功
  Ack,
  /// 命令失败
  Error(ErrorCode),
  /// 电量快照 0..=100
  BatterySnapshot { percent: u8 },
  /// K3：Session nonce 广播（`req_seq` 应为 0，表示"非命令响应"）
  NonceHello { nonce: u32 },
  /// receiver 对 controller Announce 的应答
  ///
  /// - `mac`：RPC 侧 receiver 自己的 MAC-48，controller 侧以其为唯一标识
  /// - `rssi_dbm`：保留位（可选），-127 表示未知
  /// - `role_tag`：3 字节 ASCII 角色标签，如 `mot`/`led`/`srv`，不足右侧补 0
  AnnounceReply {
    mac: [u8; 6],
    rssi_dbm: i8,
    role_tag: [u8; 3],
  },
}

impl ResponseBody {
  /// 取得响应 kind discriminant
  pub const fn kind(&self) -> ResponseKind {
    match self {
      Self::Ack => ResponseKind::Ack,
      Self::Error(_) => ResponseKind::Error,
      Self::BatterySnapshot { .. } => ResponseKind::BatterySnapshot,
      Self::NonceHello { .. } => ResponseKind::NonceHello,
      Self::AnnounceReply { .. } => ResponseKind::AnnounceReply,
    }
  }
}

impl CommandResponse {
  /// 便捷构造：Ack（默认 `key_id = 0`；推荐用 [`Self::ack_with_key`] 显式传 key_id）
  #[must_use]
  pub const fn ack(req_seq: u32) -> Self {
    Self {
      req_seq,
      key_id: KeyId::DEFAULT,
      body: ResponseBody::Ack,
    }
  }

  /// 便捷构造：Ack（显式指定 key_id，一般取自请求 Command）
  #[must_use]
  pub const fn ack_with_key(req_seq: u32, key_id: KeyId) -> Self {
    Self {
      req_seq,
      key_id,
      body: ResponseBody::Ack,
    }
  }

  /// 便捷构造：Error（默认 `key_id = 0`）
  #[must_use]
  pub const fn err(req_seq: u32, code: ErrorCode) -> Self {
    Self {
      req_seq,
      key_id: KeyId::DEFAULT,
      body: ResponseBody::Error(code),
    }
  }

  /// 便捷构造：Error（显式指定 key_id）
  #[must_use]
  pub const fn err_with_key(req_seq: u32, key_id: KeyId, code: ErrorCode) -> Self {
    Self {
      req_seq,
      key_id,
      body: ResponseBody::Error(code),
    }
  }

  /// 便捷构造：电量快照
  #[must_use]
  pub const fn battery(req_seq: u32, percent: u8) -> Self {
    Self {
      req_seq,
      key_id: KeyId::DEFAULT,
      body: ResponseBody::BatterySnapshot { percent },
    }
  }

  /// 便捷构造：Nonce 广播（K3）
  ///
  /// `req_seq` 恒为 0：表示"这不是对某条 Command 的响应"，而是主动广播。
  /// `key_id` 固定为 [`KeyId::DEFAULT`]。
  #[must_use]
  pub const fn nonce_hello(nonce: u32) -> Self {
    Self {
      req_seq: 0,
      key_id: KeyId::DEFAULT,
      body: ResponseBody::NonceHello { nonce },
    }
  }

  /// 便捷构造：Announce 回复
  ///
  /// `req_seq` 取自触发广播的 `Announce` 的 seq（方便 controller 相关性处理）。
  #[must_use]
  pub const fn announce_reply(
    req_seq: u32,
    key_id: KeyId,
    mac: [u8; 6],
    rssi_dbm: i8,
    role_tag: [u8; 3],
  ) -> Self {
    Self {
      req_seq,
      key_id,
      body: ResponseBody::AnnounceReply {
        mac,
        rssi_dbm,
        role_tag,
      },
    }
  }
}

// ============================================================
// 解码错误
// ============================================================

/// Response 解码失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseDecodeError {
  /// 长度不匹配
  BadLength,
  /// 魔数不匹配
  BadMagic,
  /// 协议版本不支持（version_byte 低 4 位）
  UnsupportedVersion(u8),
  /// key_id 超出当前固件支持的 slot 范围
  UnsupportedKeyId(u8),
  /// 未知的 kind 字节
  UnknownKind(u8),
  /// 未知的错误码字节
  UnknownErrorCode(u8),
  /// CRC 校验失败
  BadCrc { expected: u16, actual: u16 },
  /// HMAC 签名校验失败（认证失败，可能是伪造帧、密钥错误、或 key_id 已 revoke）
  AuthFailed,
}

#[cfg(feature = "defmt")]
impl defmt::Format for ResponseDecodeError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::BadLength => defmt::write!(f, "ResponseDecodeError::BadLength"),
      Self::BadMagic => defmt::write!(f, "ResponseDecodeError::BadMagic"),
      Self::UnsupportedVersion(v) => {
        defmt::write!(f, "ResponseDecodeError::UnsupportedVersion({})", v)
      }
      Self::UnsupportedKeyId(k) => {
        defmt::write!(f, "ResponseDecodeError::UnsupportedKeyId({})", k)
      }
      Self::UnknownKind(k) => defmt::write!(f, "ResponseDecodeError::UnknownKind(0x{:02x})", k),
      Self::UnknownErrorCode(c) => {
        defmt::write!(f, "ResponseDecodeError::UnknownErrorCode(0x{:02x})", c)
      }
      Self::BadCrc { expected, actual } => defmt::write!(
        f,
        "ResponseDecodeError::BadCrc(exp=0x{:04x}, act=0x{:04x})",
        expected,
        actual
      ),
      Self::AuthFailed => defmt::write!(f, "ResponseDecodeError::AuthFailed"),
    }
  }
}

// ============================================================
// 编码 / 解码
// ============================================================

/// 将 [`CommandResponse`] 编码到 [`RESPONSE_LEN`] 字节数组
///
/// # 布局
/// ```text
///   [0..2]   magic       (0xCB02, LE)
///   [2]      version_byte:
///              bits[7..4] = key_id
///              bits[3..0] = RESPONSE_VERSION
///   [3..7]   req_seq     (u32 LE)
///   [7]      kind
///   [8..14]  payload
///   [14..18] hmac        = HMAC-SHA256(SHARED_SECRETS[key_id], nonce || bytes[0..14])[..4]
///   [18..20] crc         = crc16_ibm(bytes[0..18])
/// ```
///
/// # Panics
/// 当 `resp.key_id` 对应的 slot 未启用时 [`compute_hmac_tag`] 会返回 `None`，
/// 本函数会 `expect()` —— 编码不存在的密钥是编程错误。
pub fn encode_response(resp: &CommandResponse) -> [u8; RESPONSE_LEN] {
  let mut buf = [0_u8; RESPONSE_LEN];

  // header
  buf[0..2].copy_from_slice(&RESPONSE_MAGIC.to_le_bytes());
  buf[2] = pack_version_byte(resp.key_id, RESPONSE_VERSION);
  buf[3..7].copy_from_slice(&resp.req_seq.to_le_bytes());
  buf[7] = resp.body.kind() as u8;

  // payload
  let payload = encode_payload(&resp.body);
  buf[8..8 + PAYLOAD_LEN].copy_from_slice(&payload);

  // hmac: 覆盖 header + payload；按 key_id 选密钥
  let tag = compute_hmac_tag(&buf[..HMAC_OFFSET], resp.key_id)
    .expect("encode_response called with a key_id whose slot is disabled");
  buf[HMAC_OFFSET..HMAC_OFFSET + HMAC_TAG_LEN].copy_from_slice(&tag);

  // crc: 覆盖 header + payload + hmac
  let crc = crc16_ibm(&buf[..CRC_OFFSET]);
  buf[CRC_OFFSET..RESPONSE_LEN].copy_from_slice(&crc.to_le_bytes());

  buf
}

/// 把 `(key_id, protocol_version)` 打包成 version 字节
#[inline]
const fn pack_version_byte(key_id: KeyId, protocol_version: u8) -> u8 {
  (key_id.as_u8() << KEY_ID_SHIFT) | (protocol_version & VERSION_NIBBLE_MASK)
}

/// 拆开 version 字节
#[inline]
const fn unpack_version_byte(byte: u8) -> (u8, u8) {
  let key_id_nibble = (byte >> KEY_ID_SHIFT) & VERSION_NIBBLE_MASK;
  let protocol_version = byte & VERSION_NIBBLE_MASK;
  (key_id_nibble, protocol_version)
}

fn encode_payload(body: &ResponseBody) -> [u8; PAYLOAD_LEN] {
  let mut payload = [0_u8; PAYLOAD_LEN];
  match *body {
    ResponseBody::Ack => {}
    ResponseBody::Error(code) => payload[0] = code as u8,
    ResponseBody::BatterySnapshot { percent } => payload[0] = percent.min(100),
    ResponseBody::NonceHello { nonce } => {
      // nonce 占 payload[0..4]（LE），payload[4..10] 保留为 0
      payload[0..4].copy_from_slice(&nonce.to_le_bytes());
    }
    ResponseBody::AnnounceReply {
      mac,
      rssi_dbm,
      role_tag,
    } => {
      // payload[0..6] = mac；payload[6] = rssi；payload[7..10] = role_tag
      payload[0..6].copy_from_slice(&mac);
      payload[6] = rssi_dbm as u8;
      payload[7..10].copy_from_slice(&role_tag);
    }
  }
  payload
}

/// 从字节切片解码 [`CommandResponse`]
///
/// 校验顺序：长度 → magic → version_byte（拆 version+key_id）→ CRC → HMAC → kind
pub fn decode_response(buf: &[u8]) -> Result<CommandResponse, ResponseDecodeError> {
  if buf.len() != RESPONSE_LEN {
    return Err(ResponseDecodeError::BadLength);
  }

  let magic = u16::from_le_bytes([buf[0], buf[1]]);
  if magic != RESPONSE_MAGIC {
    return Err(ResponseDecodeError::BadMagic);
  }

  let (key_id_nibble, protocol_version) = unpack_version_byte(buf[2]);
  if protocol_version != RESPONSE_VERSION {
    return Err(ResponseDecodeError::UnsupportedVersion(buf[2]));
  }
  if (key_id_nibble as usize) >= KEY_SLOTS {
    return Err(ResponseDecodeError::UnsupportedKeyId(key_id_nibble));
  }
  let key_id = KeyId::from_wire_nibble(key_id_nibble);

  // CRC 校验（廉价，先做）
  let expected_crc = crc16_ibm(&buf[..CRC_OFFSET]);
  let actual_crc = u16::from_le_bytes([buf[CRC_OFFSET], buf[CRC_OFFSET + 1]]);
  if expected_crc != actual_crc {
    return Err(ResponseDecodeError::BadCrc {
      expected: expected_crc,
      actual: actual_crc,
    });
  }

  // HMAC 校验（较贵，后做）
  let tag_bytes = &buf[HMAC_OFFSET..HMAC_OFFSET + HMAC_TAG_LEN];
  if !verify_hmac_tag(&buf[..HMAC_OFFSET], tag_bytes, key_id) {
    return Err(ResponseDecodeError::AuthFailed);
  }

  let req_seq = u32::from_le_bytes([buf[3], buf[4], buf[5], buf[6]]);
  let kind_byte = buf[7];
  let kind =
    ResponseKind::from_wire(kind_byte).ok_or(ResponseDecodeError::UnknownKind(kind_byte))?;
  let payload = &buf[8..8 + PAYLOAD_LEN];

  let body = match kind {
    ResponseKind::Ack => ResponseBody::Ack,
    ResponseKind::Error => {
      let code = ErrorCode::from_wire(payload[0])
        .ok_or(ResponseDecodeError::UnknownErrorCode(payload[0]))?;
      ResponseBody::Error(code)
    }
    ResponseKind::BatterySnapshot => ResponseBody::BatterySnapshot {
      percent: payload[0].min(100),
    },
    ResponseKind::NonceHello => ResponseBody::NonceHello {
      nonce: u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
    },
    ResponseKind::AnnounceReply => {
      let mut mac = [0_u8; 6];
      mac.copy_from_slice(&payload[0..6]);
      let rssi_dbm = payload[6] as i8;
      let mut role_tag = [0_u8; 3];
      role_tag.copy_from_slice(&payload[7..10]);
      ResponseBody::AnnounceReply {
        mac,
        rssi_dbm,
        role_tag,
      }
    }
  };

  Ok(CommandResponse {
    req_seq,
    key_id,
    body,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::auth::AUTH_ENABLED;

  #[test]
  fn frame_length_is_24() {
    let bytes = encode_response(&CommandResponse::ack(0));
    assert_eq!(bytes.len(), 24);
  }

  #[test]
  fn roundtrip_ack() {
    let resp = CommandResponse::ack(42);
    let bytes = encode_response(&resp);
    assert_eq!(decode_response(&bytes), Ok(resp));
  }

  #[test]
  fn roundtrip_error() {
    let resp = CommandResponse::err(7, ErrorCode::InvalidArgument);
    let bytes = encode_response(&resp);
    assert_eq!(decode_response(&bytes), Ok(resp));
  }

  #[test]
  fn roundtrip_battery() {
    let resp = CommandResponse::battery(100, 85);
    let bytes = encode_response(&resp);
    assert_eq!(decode_response(&bytes), Ok(resp));
  }

  #[test]
  fn detect_bad_magic() {
    let mut bytes = encode_response(&CommandResponse::ack(0));
    bytes[0] ^= 0xFF;
    assert_eq!(decode_response(&bytes), Err(ResponseDecodeError::BadMagic));
  }

  #[test]
  fn detect_bad_crc() {
    let mut bytes = encode_response(&CommandResponse::ack(1));
    // 篡改 payload 一位
    bytes[9] ^= 0xFF;
    assert!(matches!(
      decode_response(&bytes),
      Err(ResponseDecodeError::BadCrc { .. })
    ));
  }

  #[test]
  fn detect_auth_failed_when_crc_repaired() {
    let mut bytes = encode_response(&CommandResponse::ack(5));
    bytes[8] ^= 0xFF; // 篡改 payload
    // 修正 CRC 让它"看起来完整"，但 HMAC 应拒绝
    let crc = crc16_ibm(&bytes[..CRC_OFFSET]);
    bytes[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    if AUTH_ENABLED {
      assert_eq!(
        decode_response(&bytes),
        Err(ResponseDecodeError::AuthFailed)
      );
    }
  }

  #[test]
  fn battery_clamped_on_encode() {
    let bytes = encode_response(&CommandResponse::battery(1, 250));
    let decoded = decode_response(&bytes).unwrap();
    match decoded.body {
      ResponseBody::BatterySnapshot { percent } => assert_eq!(percent, 100),
      _ => panic!("expected BatterySnapshot variant"),
    }
  }

  #[test]
  fn roundtrip_nonce_hello() {
    let resp = CommandResponse::nonce_hello(0xDEAD_BEEF);
    let bytes = encode_response(&resp);
    assert_eq!(decode_response(&bytes), Ok(resp));
  }

  #[test]
  fn nonce_hello_req_seq_is_zero() {
    // NonceHello 是主动广播，req_seq 必须是 0
    let resp = CommandResponse::nonce_hello(0x1234_5678);
    assert_eq!(resp.req_seq, 0);
  }

  #[test]
  fn nonce_hello_edge_values() {
    // u32::MAX 与 0 都应正确往返
    for nonce in [0_u32, 1, u32::MAX, 0x8000_0000] {
      let resp = CommandResponse::nonce_hello(nonce);
      let bytes = encode_response(&resp);
      let decoded = decode_response(&bytes).unwrap();
      match decoded.body {
        ResponseBody::NonceHello { nonce: got } => assert_eq!(got, nonce),
        _ => panic!("expected NonceHello variant"),
      }
    }
  }

  // ---- O 选项：密钥轮换相关测试 ----

  #[test]
  fn roundtrip_with_non_default_key_id() {
    let resp = CommandResponse::ack_with_key(99, KeyId::new(1).unwrap());
    let bytes = encode_response(&resp);
    let decoded = decode_response(&bytes).unwrap();
    assert_eq!(decoded, resp);
    assert_eq!(decoded.key_id.as_u8(), 1);
  }

  #[test]
  fn err_with_key_id_roundtrip() {
    let resp = CommandResponse::err_with_key(7, KeyId::new(1).unwrap(), ErrorCode::Unsupported);
    let bytes = encode_response(&resp);
    let decoded = decode_response(&bytes).unwrap();
    assert_eq!(decoded, resp);
  }

  #[test]
  fn v3_response_rejected() {
    // 伪造 v3（version_byte = 0x03），
    let mut bytes = encode_response(&CommandResponse::ack(1));
    bytes[2] = 0x03;
    match decode_response(&bytes) {
      Err(ResponseDecodeError::UnsupportedVersion(0x03)) => {}
      other => panic!("expected UnsupportedVersion(0x03), got {:?}", other),
    }
  }

  #[test]
  fn v4_response_rejected() {
    // version_byte 低 4 位 protocol_version = 4，与当前协议版本（RESPONSE_VERSION）不符，应被拒绝
    let mut bytes = encode_response(&CommandResponse::ack(1));
    bytes[2] = 0x04;
    match decode_response(&bytes) {
      Err(ResponseDecodeError::UnsupportedVersion(0x04)) => {}
      other => panic!("expected UnsupportedVersion(0x04), got {:?}", other),
    }
  }

  #[test]
  fn roundtrip_announce_reply() {
    let resp = CommandResponse::announce_reply(
      42,
      KeyId::DEFAULT,
      [0x24, 0x0A, 0xC4, 0x11, 0x22, 0x33],
      -55,
      *b"mot",
    );
    let bytes = encode_response(&resp);
    let decoded = decode_response(&bytes).unwrap();
    assert_eq!(decoded, resp);
  }

  #[test]
  fn reject_response_with_unsupported_key_id() {
    // 手工构造 wire，key_id = KEY_SLOTS（未支持）
    let mut buf = [0_u8; RESPONSE_LEN];
    buf[0..2].copy_from_slice(&RESPONSE_MAGIC.to_le_bytes());
    buf[2] = ((KEY_SLOTS as u8) << 4) | RESPONSE_VERSION;
    // req_seq 任意；kind = Ack; payload = 0
    buf[7] = ResponseKind::Ack as u8;
    // CRC 补上（HMAC 区已全 0）
    let crc = crc16_ibm(&buf[..CRC_OFFSET]);
    buf[CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
    match decode_response(&buf) {
      Err(ResponseDecodeError::UnsupportedKeyId(k)) => assert_eq!(k as usize, KEY_SLOTS),
      other => panic!("expected UnsupportedKeyId, got {:?}", other),
    }
  }

  #[test]
  fn response_version_byte_pack_unpack_roundtrip() {
    for raw_key in 0..=15_u8 {
      let key_id = KeyId::new(raw_key).unwrap();
      let byte = pack_version_byte(key_id, RESPONSE_VERSION);
      let (got_key, got_ver) = unpack_version_byte(byte);
      assert_eq!(got_key, raw_key);
      assert_eq!(got_ver, RESPONSE_VERSION);
    }
  }

  // ---- N-8：encode_response 在 slot 未启用时的 panic 覆盖 ----

  #[test]
  #[should_panic(expected = "slot is disabled")]
  fn encode_response_panics_on_disabled_slot() {
    // slot 2 未启用（SHARED_SECRETS[2] = None）→ encode_response 应 panic
    let resp = CommandResponse::ack_with_key(1, KeyId::new(2).unwrap());
    let _ = encode_response(&resp);
  }
}
