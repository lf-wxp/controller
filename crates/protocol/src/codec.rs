//! 帧编解码：Frame ⇄ [u8; FRAME_LEN]
//!
//! # 帧总长
//! [`FRAME_LEN`] = 2 (magic) + 1 (ver) + 4 (seq) + 12 (payload) + 4 (dest_mask) + 2 (crc) = **25 字节**
//!
//! # 字节序
//! 全部 little-endian（ESP32 是小端 CPU，直接内存转换最快）
//!
//! # CRC 覆盖范围
//! magic..dest_mask 全部 23 字节（**不含**尾部 CRC 本身）
//!
//! # 目标寻址（dest_mask）
//! - `dest_mask: u32` —— 位图目标寻址：bit-i = 1 表示 `receiver_id == i` 的
//!   接收方需要处理该帧；`0xFFFF_FFFF` = 广播（所有接收方）；`0` = 静默丢弃。

use super::crc::crc16_ibm;
use super::frame::{FRAME_MAGIC, Frame, FrameHeader, PROTOCOL_VERSION};
use super::state::{GamepadState, PAYLOAD_LEN};

/// 帧头长度（bytes）= magic(2) + ver(1) + seq(4)
const HEADER_LEN: usize = 2 + 1 + 4;
/// dest_mask 长度（bytes）
const DEST_MASK_LEN: usize = 4;
/// CRC 长度（bytes）
const CRC_LEN: usize = 2;

/// 完整帧序列化后的固定长度（25 字节）
pub const FRAME_LEN: usize = HEADER_LEN + PAYLOAD_LEN + DEST_MASK_LEN + CRC_LEN;

/// 字段编译期偏移（供 encode/decode + 测试共用）
const PAYLOAD_OFFSET: usize = HEADER_LEN;
const DEST_MASK_OFFSET: usize = PAYLOAD_OFFSET + PAYLOAD_LEN;
const CRC_OFFSET: usize = DEST_MASK_OFFSET + DEST_MASK_LEN;

// 编译期确认：25 字节，改布局时会当场炸掉
const _: () = assert!(FRAME_LEN == 25);
const _: () = assert!(CRC_OFFSET == 23);

/// 解码失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
  /// 缓冲区长度不等于 [`FRAME_LEN`]
  BadLength,
  /// 魔数不匹配（帧未对齐或数据损坏）
  BadMagic,
  /// 协议版本不支持
  UnsupportedVersion(u8),
  /// CRC 校验失败
  BadCrc { expected: u16, actual: u16 },
}

#[cfg(feature = "defmt")]
impl defmt::Format for DecodeError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::BadLength => defmt::write!(f, "DecodeError::BadLength"),
      Self::BadMagic => defmt::write!(f, "DecodeError::BadMagic"),
      Self::UnsupportedVersion(v) => defmt::write!(f, "DecodeError::UnsupportedVersion({})", v),
      Self::BadCrc { expected, actual } => {
        defmt::write!(
          f,
          "DecodeError::BadCrc(expected=0x{:04x}, actual=0x{:04x})",
          expected,
          actual
        )
      }
    }
  }
}

/// 将 [`Frame`] 编码到 [`FRAME_LEN`] 字节数组
///
/// 帧布局：
/// ```text
///  offset | size | field
///  -------+------+--------------
///    0    |  2   | magic (LE)
///    2    |  1   | version (= 2)
///    3    |  4   | seq (LE)
///    7    | 12   | payload (GamepadState)
///   19    |  4   | dest_mask (LE)
///   23    |  2   | crc16_ibm(bytes[0..23]) (LE)
/// ```
pub fn encode_frame(frame: &Frame) -> [u8; FRAME_LEN] {
  let mut buf = [0_u8; FRAME_LEN];

  // header
  buf[0..2].copy_from_slice(&frame.header.magic.to_le_bytes());
  buf[2] = frame.header.version;
  buf[3..7].copy_from_slice(&frame.header.seq.to_le_bytes());

  // payload
  let payload_bytes = frame.payload.to_bytes();
  buf[PAYLOAD_OFFSET..PAYLOAD_OFFSET + PAYLOAD_LEN].copy_from_slice(&payload_bytes);

  // dest_mask
  buf[DEST_MASK_OFFSET..DEST_MASK_OFFSET + DEST_MASK_LEN]
    .copy_from_slice(&frame.dest_mask.to_le_bytes());

  // crc: 覆盖 header + payload + dest_mask
  let crc = crc16_ibm(&buf[..CRC_OFFSET]);
  buf[CRC_OFFSET..FRAME_LEN].copy_from_slice(&crc.to_le_bytes());

  buf
}

/// 从字节切片解码 [`Frame`]（严格校验 magic / version / crc）
pub fn decode_frame(buf: &[u8]) -> Result<Frame, DecodeError> {
  if buf.len() != FRAME_LEN {
    return Err(DecodeError::BadLength);
  }

  let magic = u16::from_le_bytes([buf[0], buf[1]]);
  if magic != FRAME_MAGIC {
    return Err(DecodeError::BadMagic);
  }

  let version = buf[2];
  if version != PROTOCOL_VERSION {
    return Err(DecodeError::UnsupportedVersion(version));
  }

  let expected_crc = crc16_ibm(&buf[..CRC_OFFSET]);
  let actual_crc = u16::from_le_bytes([buf[CRC_OFFSET], buf[CRC_OFFSET + 1]]);
  if expected_crc != actual_crc {
    return Err(DecodeError::BadCrc {
      expected: expected_crc,
      actual: actual_crc,
    });
  }

  let seq = u32::from_le_bytes([buf[3], buf[4], buf[5], buf[6]]);
  let mut payload_arr = [0_u8; PAYLOAD_LEN];
  payload_arr.copy_from_slice(&buf[PAYLOAD_OFFSET..PAYLOAD_OFFSET + PAYLOAD_LEN]);
  let payload = GamepadState::from_bytes(&payload_arr);
  let dest_mask = u32::from_le_bytes([
    buf[DEST_MASK_OFFSET],
    buf[DEST_MASK_OFFSET + 1],
    buf[DEST_MASK_OFFSET + 2],
    buf[DEST_MASK_OFFSET + 3],
  ]);

  Ok(Frame {
    header: FrameHeader {
      magic,
      version,
      seq,
    },
    payload,
    dest_mask,
  })
}

#[cfg(test)]
mod tests {
  use super::super::state::ButtonBits;
  use super::*;

  #[test]
  fn roundtrip_empty() {
    let frame = Frame::new(0, GamepadState::EMPTY);
    let bytes = encode_frame(&frame);
    let decoded = decode_frame(&bytes).expect("decode ok");
    assert_eq!(frame, decoded);
  }

  #[test]
  fn roundtrip_full() {
    let mut state = GamepadState {
      buttons: 0,
      joy_x: -777,
      joy_y: 321,
      knob_1: 500,
      knob_2: 999,
      _reserved: 0,
    };
    state.set_button(ButtonBits::Btn1, true);
    state.set_button(ButtonBits::Btn4, true);
    let frame = Frame::with_dest(0xDEADBEEF, state, 0x1234_5678);
    let bytes = encode_frame(&frame);
    assert_eq!(bytes.len(), FRAME_LEN);

    let decoded = decode_frame(&bytes).unwrap();
    assert_eq!(frame, decoded);
    assert_eq!(decoded.dest_mask, 0x1234_5678);
  }

  #[test]
  fn broadcast_mask_roundtrips() {
    let frame = Frame::new(1, GamepadState::EMPTY);
    assert_eq!(frame.dest_mask, u32::MAX);
    let bytes = encode_frame(&frame);
    let decoded = decode_frame(&bytes).unwrap();
    assert_eq!(decoded.dest_mask, u32::MAX);
  }

  #[test]
  fn detect_bad_magic() {
    let mut bytes = encode_frame(&Frame::new(1, GamepadState::EMPTY));
    bytes[0] ^= 0xFF;
    assert_eq!(decode_frame(&bytes), Err(DecodeError::BadMagic));
  }

  #[test]
  fn detect_bad_crc() {
    let mut bytes = encode_frame(&Frame::new(1, GamepadState::EMPTY));
    // 篡改 payload 中间一字节但不改 CRC → 应检测出来
    bytes[10] ^= 0xFF;
    assert!(matches!(
      decode_frame(&bytes),
      Err(DecodeError::BadCrc { .. })
    ));
  }

  #[test]
  fn detect_bad_length() {
    assert_eq!(decode_frame(&[0u8; 10]), Err(DecodeError::BadLength));
  }

  #[test]
  fn detect_unsupported_version() {
    let mut bytes = encode_frame(&Frame::new(1, GamepadState::EMPTY));
    bytes[2] = 99;
    // 版本字节参与 CRC 计算，改动版本时 CRC 也不再匹配 —— 但版本检查在 CRC 之前
    assert_eq!(
      decode_frame(&bytes),
      Err(DecodeError::UnsupportedVersion(99))
    );
  }

  #[test]
  fn dest_mask_tamper_detected_by_crc() {
    // 篡改 dest_mask 而不同步 CRC → 应被 CRC 拒绝
    let mut bytes = encode_frame(&Frame::new(1, GamepadState::EMPTY));
    bytes[DEST_MASK_OFFSET] ^= 0xFF;
    assert!(matches!(
      decode_frame(&bytes),
      Err(DecodeError::BadCrc { .. })
    ));
  }
}
