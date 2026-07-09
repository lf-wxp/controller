//! 传输帧结构 —— 网络字节层
//!
//! [`Frame`] 是编码器/传输层看到的完整帧对象（header + payload + crc）。
//! 编解码逻辑在 [`super::codec`]。

use super::state::GamepadState;

/// 协议魔数：little-endian 存储时字节序列为 `[0x1E, 0xC7]`
///
/// 让接收端能在流式解析时对齐帧起点，并快速拒绝损坏数据。
pub const FRAME_MAGIC: u16 = 0xC71E;

/// 协议版本号
///
/// 加新字段（复用保留区）不需要升版本；改变字段布局或含义时才升。
pub const PROTOCOL_VERSION: u8 = 1;

/// 帧头（7 字节）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
  /// 魔数，固定 [`FRAME_MAGIC`]
  pub magic: u16,
  /// 协议版本，见 [`PROTOCOL_VERSION`]
  pub version: u8,
  /// 序列号，发送端递增；接收端可用来检测丢包/乱序
  pub seq: u32,
}

impl FrameHeader {
  /// 构造一个当前版本的帧头
  pub const fn new(seq: u32) -> Self {
    Self {
      magic: FRAME_MAGIC,
      version: PROTOCOL_VERSION,
      seq,
    }
  }
}

/// 完整帧：帧头 + 负载
///
/// CRC 由编码器在序列化时**基于 header + payload 的字节**计算并附加，
/// 不作为 `Frame` 的字段——避免"手动构造 Frame 时 CRC 错" 的风险。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
  pub header: FrameHeader,
  pub payload: GamepadState,
}

impl Frame {
  /// 用给定序号和状态构造一帧
  pub const fn new(seq: u32, state: GamepadState) -> Self {
    Self {
      header: FrameHeader::new(seq),
      payload: state,
    }
  }
}
