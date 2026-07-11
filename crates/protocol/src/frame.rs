//! 传输帧结构 —— 网络字节层
//!
//! [`Frame`] 是编码器/传输层看到的完整帧对象（header + payload + dest_mask + crc）。
//! 编解码逻辑在 [`super::codec`]。
//!
//! 当前版本使用 `dest_mask: u32` 位图寻址（bit-i = 1 ⇒ receiver_id==i 处理该帧），
//! 帧总长 25 字节。

use super::state::GamepadState;

/// 协议魔数：little-endian 存储时字节序列为 `[0x1E, 0xC7]`
///
/// 让接收端能在流式解析时对齐帧起点，并快速拒绝损坏数据。
pub const FRAME_MAGIC: u16 = 0xC71E;

/// 协议版本号（当前为 2，帧携带 `dest_mask` 位图寻址字段）
pub const PROTOCOL_VERSION: u8 = 2;

/// 广播 mask（32 bit 全 1）—— 所有接收器都应处理该帧
///
/// 与 [`crate::state::GamepadState`] 无关，供 UI/传输层作为默认目标使用。
pub const BROADCAST_DEST_MASK: u32 = u32::MAX;

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

/// 完整帧：帧头 + 负载 + 目标 mask
///
/// CRC 由编码器在序列化时**基于 header + payload + dest_mask 的字节**计算并附加，
/// 不作为 `Frame` 的字段——避免"手动构造 Frame 时 CRC 错" 的风险。
///
/// # `dest_mask` 语义
/// - `bit-i == 1` ⇒ `receiver_id == i` 的接收方应处理该帧
/// - `0xFFFF_FFFF`（[`BROADCAST_DEST_MASK`]）= 全体接收（默认）
/// - `0` = 静默丢弃（可用于"暂停下发"）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
  pub header: FrameHeader,
  pub payload: GamepadState,
  /// 目标寻址位图：bit-i 对应 receiver_id == i 的接收方
  pub dest_mask: u32,
}

impl Frame {
  /// 用给定序号和状态构造一帧，默认广播到所有接收方
  ///
  /// 等价于 `Frame::with_dest(seq, state, BROADCAST_DEST_MASK)`。
  pub const fn new(seq: u32, state: GamepadState) -> Self {
    Self::with_dest(seq, state, BROADCAST_DEST_MASK)
  }

  /// 用给定序号、状态、目标 mask 构造一帧
  ///
  /// # 参数
  /// - `dest_mask`：位图目标；见结构体级文档说明
  pub const fn with_dest(seq: u32, state: GamepadState, dest_mask: u32) -> Self {
    Self {
      header: FrameHeader::new(seq),
      payload: state,
      dest_mask,
    }
  }

  /// 判断某个 `receiver_id` 是否被本帧寻址
  ///
  /// # 参数
  /// - `receiver_id`：接收方逻辑 ID，取值范围 `[0, 31]`；超出范围时恒返回 `false`
  #[inline]
  #[must_use]
  pub const fn is_addressed_to(&self, receiver_id: u8) -> bool {
    if receiver_id >= 32 {
      return false;
    }
    (self.dest_mask & (1u32 << receiver_id)) != 0
  }
}
