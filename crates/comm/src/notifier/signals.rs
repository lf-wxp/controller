//! # Notifier 内部 Signal 类型别名
//!
//! 用户在自己的 crate 里如下声明这几个 `static`：
//! ```ignore
//! use comm::notifier::signals::*;
//!
//! static FRAME_SIG: FrameSignal = FrameSignal::new();
//! static CMD_OUT_SIG: CommandOutSignal = CommandOutSignal::new();
//! static RESP_SIG: ResponseSignal = ResponseSignal::new();
//! ```
//! 然后传给 [`Notifier::builder()`](super::Notifier::builder)。
//!
//! ## 为什么用 `Signal` 而不是 `Channel`
//! - Frame 是高频状态流，只关心**最新**；覆盖式语义（`Signal` = "最后写入者赢"）
//!   避免堆积
//! - Command/Response 频率低，覆盖式偶发丢失也是可容忍的（下一次 Announce 会重发）

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use protocol::{COMMAND_LEN, CommandResponse, Frame};

/// Frame 出站 Signal（主循环 → broadcast task）
pub type FrameSignal = Signal<CriticalSectionRawMutex, Frame>;

/// 出站命令的目标寻址
///
/// comm 的 peer / announce / assign 机制天然以 MAC-48 为中心（见
/// [`PeerRegistry`](crate::PeerRegistry) 与 `AnnounceReply`），因此这里直接以
/// `[u8; 6]` 表达单播目标，而不再额外引入地址泛型。
///
/// - [`Broadcast`](Self::Broadcast)：发给全网所有能听到的接收端（历史默认行为，
///   fire-and-forget、无链路层 ACK）
/// - [`Unicast`](Self::Unicast)：发给指定 MAC；由链路侧保证该 peer 已 `add_peer`，
///   ESP-NOW 会给出 MAC 层 ACK，[`run_broadcast_loop`](super::run_broadcast_loop)
///   据此做有界重试
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandDest {
  /// 广播给全网
  Broadcast,
  /// 单播到指定 MAC-48
  Unicast([u8; 6]),
}

/// 一条待出站命令：目标寻址 + 已编码字节
///
/// 值是**已编码**的 [`COMMAND_LEN`] 字节 + 目标寻址；避免在 Signal 里放巨大 enum。
#[derive(Clone, Copy)]
pub struct OutboundCommand {
  /// 目标寻址（广播 / 单播）
  pub dest: CommandDest,
  /// 已编码的 [`COMMAND_LEN`] 字节
  pub bytes: [u8; COMMAND_LEN],
}

impl OutboundCommand {
  /// 构造一条广播命令
  #[must_use]
  pub const fn broadcast(bytes: [u8; COMMAND_LEN]) -> Self {
    Self {
      dest: CommandDest::Broadcast,
      bytes,
    }
  }

  /// 构造一条单播命令（目标 MAC）
  #[must_use]
  pub const fn unicast(mac: [u8; 6], bytes: [u8; COMMAND_LEN]) -> Self {
    Self {
      dest: CommandDest::Unicast(mac),
      bytes,
    }
  }
}

/// Command 出站 Signal（发现流程 / send_command / AssignId → broadcast task）
///
/// 载荷为 [`OutboundCommand`]：编码字节 + 目标寻址。覆盖式语义不变。
pub type CommandOutSignal = Signal<CriticalSectionRawMutex, OutboundCommand>;

/// Response 出站 Signal（命令 handler → broadcast task）
pub type ResponseSignal = Signal<CriticalSectionRawMutex, CommandResponse>;
