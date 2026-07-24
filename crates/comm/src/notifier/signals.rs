//! # Notifier 出站通道类型别名
//!
//! 用户在自己的 crate 里如下声明这几个 `static`：
//! ```ignore
//! use comm::notifier::signals::*;
//!
//! static FRAME_SIG: FrameSignal = FrameSignal::new();
//! static CMD_OUT_CHAN: CommandOutChannel = CommandOutChannel::new();
//! static RESP_CHAN: ResponseChannel = ResponseChannel::new();
//! ```
//! 然后传给 [`Notifier::builder()`](super::Notifier::builder)。
//!
//! ## 出站语义：Frame 覆盖式，Command / Response 有界队列
//! 三条出站通道的**语义按流量特性分化**：
//!
//! | 通道 | 类型 | 语义 | 理由 |
//! |---|---|---|---|
//! | [`FrameSignal`] | `Signal` | 覆盖式（后写覆盖前写） | Frame 是高频状态流，只关心**最新**；覆盖避免堆积 |
//! | [`CommandOutChannel`] | `Channel<_, _, N>` | **有界 FIFO** | Command 低频但**每条都重要**（组播多台 / `AssignId` 不能丢） |
//! | [`ResponseChannel`] | `Channel<_, _, N>` | **有界 FIFO** | `Ack` / `AnnounceReply` / `NonceHello` 不应互相覆盖 |
//!
//! ### 为什么 Command / Response 从 `Signal` 升级成 `Channel`
//! 旧版三条都用覆盖式 `Signal`，导致一类难缠问题：
//! - 紧凑循环里对多台 `send_command_to` 连发只有最后一条能出站（**无法组播**）
//! - `AssignId` 可能被后续命令覆盖丢失（旧版靠"每次 AnnounceReply 重发"兜底）
//! - `Ack` 与 `NonceHello` 共用一个 Signal 会互相挤掉
//!
//! 换成深度 [`OUTBOUND_QUEUE_DEPTH`] 的有界队列后，上述问题消失：生产者用
//! `try_send` 非阻塞入队（队列满才丢弃**当前**这条，而不是永远丢弃**上一条**），
//! `broadcast_loop` 用 `receive().await` 逐条消费。Frame 仍保持覆盖式不变。

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use protocol::{COMMAND_LEN, CommandResponse, Frame};

/// 出站 Command / Response 队列深度
///
/// 8 是一个"够吸收中等规模发现突发 / 组播、又不至于吃内存"的经验值。每条
/// [`OutboundCommand`] 约 `COMMAND_LEN + 7` 字节、[`CommandResponse`] 更小，
/// 深度 8 的两条队列合计约一两百字节静态 RAM（对 ESP32 / C6 的数十 KB 而言可忽略）。
///
/// # 这是唯一的深度调节点
/// [`CommandOutChannel`] / [`ResponseChannel`] 是**深度写死为本常量**的类型别名，
/// 而 builder 的 [`command_signal`](super::NotifierBuilder::command_signal) /
/// [`response_signal`](super::NotifierBuilder::response_signal) 等方法签名都要求
/// `&'static CommandOutChannel` / `&'static ResponseChannel`——因此应用**无法**传入
/// 一个不同深度的自定义 `Channel`（类型不匹配）。要改深度，改本常量即可，全 crate
/// 的所有出站队列随之统一生效。
///
/// # 大规模发现的容量提示（**部署前请评估**）
/// 一次 [`Notifier::discover`](super::Notifier::discover) 若有 **多于 `OUTBOUND_QUEUE_DEPTH`
/// 个**接收端几乎同时回 `AnnounceReply`，接收 loop 会逐条把自愈 `AssignId` 入
/// [`CommandOutChannel`]（深度即本常量）；当 broadcast loop 来不及排空（还要做
/// 单播 [`MAX_UNICAST_SEND_RETRIES`](super::MAX_UNICAST_SEND_RETRIES) 重试）时，
/// 超出深度的 `AssignId` 会被 `try_send` 丢弃并计入 [`crate::metrics`]。
///
/// 实际射频到达是**串行**的，深度 8 覆盖数台～十余台的常规部署；但**接近
/// [`MAX_PEERS`](crate::peer_registry::MAX_PEERS)（32）台**的部署，单轮 `discover`
/// 仍可能无法让全员一次拿到 id，需要**多轮发现**逐步收敛（每次 `AnnounceReply` 都会
/// 幂等重发 `AssignId`，故最终一致）。若要求接近满员时单轮全员就位，把本常量再调大
/// （代价是全体出站队列一并占更多静态 RAM）。
pub const OUTBOUND_QUEUE_DEPTH: usize = 8;

/// Frame 出站 Signal（主循环 → broadcast task）
///
/// **覆盖式**：高频状态流只关心最新帧，后写覆盖前写。
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

/// Command 出站有界队列（发现流程 / send_command / AssignId → broadcast task）
///
/// 载荷为 [`OutboundCommand`]：编码字节 + 目标寻址。**有界 FIFO**：生产者
/// `try_send` 非阻塞入队，队列满时丢弃当前这条（返回 `Err`），
/// [`run_broadcast_loop`](super::run_broadcast_loop) 用 `receive().await` 逐条消费。
pub type CommandOutChannel =
  Channel<CriticalSectionRawMutex, OutboundCommand, OUTBOUND_QUEUE_DEPTH>;

/// Response 出站有界队列（命令 handler / report / nonce 广播 → broadcast task）
///
/// **有界 FIFO**：`Ack` / `AnnounceReply` / `NonceHello` / `BatterySnapshot` 各自
/// 排队，不再互相覆盖。生产者 `try_send`，消费者 `receive().await`。
pub type ResponseChannel = Channel<CriticalSectionRawMutex, CommandResponse, OUTBOUND_QUEUE_DEPTH>;

// ============================================================
// 入队助手（统一 try_send + 丢弃计数）
// ============================================================
//
// 所有出站生产者都应走这两个助手，而不是裸调 `chan.try_send(..)`：
// 队列满导致的丢弃会被记进 [`crate::metrics`]，避免"静默丢包"无从发现。

/// 把一条命令塞进出站队列；队列满则丢弃并计数（见 [`crate::metrics`]）
pub fn enqueue_command(chan: &CommandOutChannel, cmd: OutboundCommand) {
  if chan.try_send(cmd).is_err() {
    crate::metrics::record_dropped_command();
  }
}

/// 把一条回执塞进出站队列；队列满则丢弃并计数（见 [`crate::metrics`]）
pub fn enqueue_response(chan: &ResponseChannel, resp: CommandResponse) {
  if chan.try_send(resp).is_err() {
    crate::metrics::record_dropped_response();
  }
}
