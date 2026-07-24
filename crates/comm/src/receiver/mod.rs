//! # Receiver —— 接收端门面（Endpoint 角色）
//!
//! ## 角色定位
//! `Receiver` 在网络中扮演 **Endpoint（端点）** 角色。请把它与 [`crate::Notifier`]
//! （**Coordinator**，协调者）严格区分开——两者虽然在**消息能力**层面已完全对称
//! （都支持发送/接收 Frame / Command / Response），但**职责**是不对称的：
//!
//! | 维度 | Notifier（Coordinator） | Receiver（Endpoint） |
//! |---|---|---|
//! | 拥有 `PeerRegistry` | ✅ 目录权威方 | ❌ 无目录 |
//! | 拥有 `Selector` | ✅ 决定下发目标 | ❌ 无决策权 |
//! | 主动 `discover()` | ✅ 发起会话 | ❌ 只能被动响应 |
//! | 首次遇到新 peer 时回 AssignId | ✅ 自动 | ❌ 只接收 AssignId |
//! | 主动 `send_frame` / `report` | ✅ | ✅（本次 P0+P1 支持） |
//! | 处理入站 Command | ✅（双身份可选）| ✅（必填 `command_handler`）|
//!
//! **一句话记忆**：`Notifier = Coordinator` 主导会话与拓扑，
//! `Receiver = Endpoint` 是被协调的叶子节点，即便它也能主动上报数据。
//! 有关这个设计的完整讨论，请参阅本 crate 的 [`crate`] 顶层文档。
//!
//! ## 作用
//! 让 receiver 端（led / motor / srv 等）只用 3 步就能接入 controller 网络：
//!
//! 1. 实现 [`CommLink`]
//! 2. 提供一个 `command_handler` 闭包（用户业务）
//! 3. spawn 后台任务；剩下的 AnnounceReply / Ack / HMAC / anti-replay 全部
//!    由本 crate 自动完成
//!
//! ## 内部流程
//! ```text
//!   CommLink::recv ──► decode_command
//!                      │
//!         ┌────────────┼────────────┐
//!         │            │            │
//!    Announce?    AssignId?      其它 kind
//!         │            │            │
//!    自动回        存 my_id        HMAC+replay
//!    AnnounceReply   到 static     ↓
//!                                  用户 handler
//!                                  ↓
//!                                  自动回 Ack/Error
//! ```

pub mod builder;

use core::sync::atomic::{AtomicU8, Ordering};

use protocol::{Command, CommandResponse, ErrorCode, Frame, ResponseBody};
// 仅 `endpoint-initiated-command` opt-in 下才用到；默认关闭时避免 unused import warning。
#[cfg(feature = "endpoint-initiated-command")]
use protocol::{CommandBody, encode_command};

use crate::dispatch::dispatch_packet;
use crate::keyring::Keyring;
use crate::link::CommLink;
use crate::notifier::signals::{CommandOutChannel, FrameSignal, ResponseChannel};
use crate::replay::ReplayGuard;

pub use builder::ReceiverBuilder;

/// Receiver 端出站 loop —— 复用 [`crate::notifier::run_broadcast_loop`] 的三路 select 编排
///
/// # 复用理由
/// 出站编排逻辑（三路 select：Frame + Response + Command）与 Notifier 完全一致，
/// 一份实现两处复用（`proj-pub-use-reexport`），避免维护双份 select 树。本函数只是
/// 一层极薄 wrapper，把 Notifier 侧多出来的 `peers` 参数固定填 `None`。
///
/// # 与 Notifier 侧的差异：Frame 无自动单播
/// Endpoint（Receiver）**没有** [`PeerRegistry`](crate::PeerRegistry)——无目录、无寻址
/// 决策权（那是 Coordinator 的职责）。因此本 wrapper 恒传 `peers = None`：出站 `Frame`
/// **一律广播**，不会触发"单目标自动单播"（详见 [`crate::notifier::run_broadcast_loop`]
/// 的 `peers` 参数说明）。Receiver 的调用方签名保持 4 参不变。
///
/// # 使用示例
/// ```ignore
/// #[embassy_executor::task]
/// async fn receiver_broadcast_task(link: MyLink) -> ! {
///     comm::receiver::run_broadcast_loop(link, &FRAME_SIG, &CMD_SIG, &RESP_SIG).await
/// }
/// ```
pub async fn run_broadcast_loop<L: CommLink>(
  link: L,
  frame_signal: &'static FrameSignal,
  command_signal: &'static CommandOutChannel,
  response_signal: &'static ResponseChannel,
) -> !
where
  L::Addr: From<[u8; 6]>,
{
  // Endpoint 无 PeerRegistry → peers = None → Frame 恒广播
  crate::notifier::run_broadcast_loop(link, None, frame_signal, command_signal, response_signal)
    .await
}

/// receiver 尚未被 controller 分配 id 的哨兵值
pub const UNASSIGNED_ID: u8 = u8::MAX;

/// 命令帧来源
///
/// 手柄侧存在两条并行的命令入口（BLE Write / ESP-NOW 广播）；用户 handler
/// 常需要区分来源以做不同的响应策略（比如 BLE 通道立即回 GATT notify，
/// ESP-NOW 通道走广播 response）。receiver-only 设备通常只会看到 `EspNow`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
  /// BLE GATT Write
  Ble,
  /// ESP-NOW 广播帧
  EspNow,
  /// 本地生成（自测 / 内部触发）
  Local,
}

#[cfg(feature = "defmt")]
impl defmt::Format for CommandSource {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::Ble => defmt::write!(f, "CommandSource::Ble"),
      Self::EspNow => defmt::write!(f, "CommandSource::EspNow"),
      Self::Local => defmt::write!(f, "CommandSource::Local"),
    }
  }
}

/// 用户 handler 的返回值：决定回什么 Ack
pub enum CommandOutcome {
  /// 成功执行 —— 由 comm 自动回一条 [`CommandResponse::ack_with_key`]
  Ok,
  /// 执行失败 —— 由 comm 自动回一条 [`CommandResponse::err_with_key`]
  Err(ErrorCode),
  /// 富回执 —— 用户自己构造整条 [`CommandResponse`]（比如 `BatterySnapshot`
  /// `ReceiverList` `NonceHello` 等带 payload 的响应）；comm 直接把它推入
  /// [`ResponseChannel`]
  Respond(CommandResponse),
  /// 不需要回执（比如已经手动回复了 / 心跳类命令）
  NoReply,
}

/// Receiver 相关错误
///
/// # 不完备枚举
/// 未来可能新增 handler panic / 链路重启等变体，预留 `#[non_exhaustive]`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReceiverError {
  /// 底层链路失败
  Link,
}

// ============================================================
// Receiver
// ============================================================

/// 用户提供的命令处理闭包类型
///
/// # 参数
/// - `src`：命令帧来源，参见 [`CommandSource`]
/// - `cmd`：已解码 + HMAC 校验通过的 [`Command`]
///
/// # 返回
/// 参见 [`CommandOutcome`]
pub type CommandHandler = fn(CommandSource, &Command) -> CommandOutcome;

/// 用户提供的帧处理闭包类型
///
/// # 与 [`CommandHandler`] 的区别
/// - 不需回执（`Frame` 是单向广播的高频状态流，建立回复将严重破坏链路时序），
///   因此返回类型为 `()`，不需枚举。
/// - 不需拗重放——帧本身无 seq；如果业务关心帧时序，在 handler 内对
///   `frame.header.seq` 自行去重即可。
///
/// # 参数
/// - `src`：帧来源（与命令共用 [`CommandSource`]）
/// - `frame`：已解码且通过 CRC 校验的 [`Frame`]，dest_mask 已确保命中本机
pub type FrameHandler = fn(CommandSource, &Frame);

/// 接收端门面
///
/// # 定位：**link 无关的编排句柄**（与 [`crate::Notifier`] 对称）
/// 持有一套 `&'static` 共享状态 + handler，**不持有 link**。用法两类：
/// 1. 生产者 API（`&self`）：[`report`](Self::report) / [`send_frame`](Self::send_frame) 主动上行；
/// 2. 后台 loop：[`run_broadcast_loop`](Self::run_broadcast_loop) /
///    [`run_receive_loop`](Self::run_receive_loop)，各吃一个 link 端，直接消费门面字段。
pub struct Receiver {
  pub(crate) keyring: &'static Keyring,
  pub(crate) replay: &'static ReplayGuard,
  pub(crate) response_signal: &'static ResponseChannel,
  /// Frame 出站 Signal —— 供 [`Receiver::send_frame`] 主动上行 / broadcast loop 消费
  pub(crate) frame_signal: &'static FrameSignal,
  /// Command 出站有界队列 —— 供 [`Receiver::send_command`] 主动上行 / broadcast loop 消费
  pub(crate) command_signal: &'static CommandOutChannel,
  pub(crate) role_tag: [u8; 3],
  pub(crate) my_mac: [u8; 6],
  pub(crate) my_id: &'static AtomicU8,
  pub(crate) handler: CommandHandler,
  /// 入站帧默认来源（透传给 handler）
  pub(crate) src: CommandSource,
  /// 可选的 Frame handler：`None` 时 receiver 不消费入站 `Frame`
  pub(crate) frame_handler: Option<FrameHandler>,
}

impl Receiver {
  /// 开始构造 Receiver
  //
  // 无需 `#[must_use]`：`ReceiverBuilder` 已在结构体层面标了 `#[must_use]`，
  // 这里再加会触发 clippy::double_must_use。
  pub const fn builder() -> ReceiverBuilder {
    ReceiverBuilder::new()
  }

  /// 当前分配到的 receiver_id（未分配时返回 `UNASSIGNED_ID`）
  #[must_use]
  pub fn assigned_id(&self) -> u8 {
    self.my_id.load(Ordering::Relaxed)
  }

  // ---- 主动出站 API（endpoint-initiated publishing）----

  /// 主动上报一条 [`ResponseBody`]（**Endpoint → Coordinator** 的主要通道）
  ///
  /// # 语义
  /// - `req_seq = 0`：表示"非请求触发"（与 [`ResponseBody::NonceHello`] 广播一致）
  /// - `key_id`：使用 keyring 当前 active key
  /// - 有界队列：入 [`ResponseChannel`]（深度
  ///   [`OUTBOUND_QUEUE_DEPTH`](crate::notifier::signals::OUTBOUND_QUEUE_DEPTH)）排队出站，
  ///   不再像旧版覆盖式 Signal 那样把上一条挤掉；仅当队列满时才丢弃本次上报
  ///
  /// # 使用示例
  /// ```ignore
  /// // 每 30s 上报一次电量
  /// receiver.report(ResponseBody::BatterySnapshot { percent: 85 });
  /// ```
  pub fn report(&self, body: ResponseBody) {
    let resp = CommandResponse {
      req_seq: 0,
      key_id: self.keyring.active(),
      body,
    };
    crate::notifier::signals::enqueue_response(self.response_signal, resp);
  }

  /// 主动广播一条 [`Frame`]
  ///
  /// # 何时用
  /// 少数场景 Receiver 需要向网内广播状态流（比如子控制器把传感器读数广播给
  /// 手柄 UI）；主流单向状态流仍是 Notifier→Receiver 方向。
  ///
  /// # 语义
  /// 覆盖式，同 [`crate::Notifier::send_frame`]。
  ///
  /// # Correctness
  /// Receiver 侧**不持有** [`crate::Selector`]，也不做 `dest_mask` 决策——callers
  /// 需要自己在传入的 [`Frame`] 上把 `dest_mask` 设置好：
  /// - 想广播给所有接收端：`Frame::with_dest(seq, state, u32::MAX)`
  /// - 想只发给特定 receiver_id：用户在 [`Frame`] 构造时按位组装 `dest_mask`
  ///
  /// 若不填 `dest_mask`（默认 0），对端 dispatch 会把这条帧过滤掉。
  pub fn send_frame(&self, frame: &Frame) {
    self.frame_signal.signal(*frame);
  }

  /// 主动发送一条 [`Command`]（**危险 opt-in API**）
  ///
  /// # Correctness
  /// **绝大多数场景下不要使用本方法**。90% 的部署里 Command 只走
  /// Coordinator（Notifier）→ Endpoint（Receiver）方向；如果 Endpoint 主动发
  /// Command：
  /// - 会占用共享 keyring 的 seq 计数器，可能与 Coordinator 的 seq 冲突
  /// - 会触发**接收方**的 anti-replay 状态迁移（受害方可能是另一个 Coordinator
  ///   甚至广播回环里的自己）
  /// - 若拓扑里存在多个 Coordinator，seq 冲突会引发难以调试的抖动
  ///
  /// 只有在你**明确**在做多 Coordinator / 对等发现等特殊拓扑，且理解上述后果时
  /// 才启用 crate feature `endpoint-initiated-command` 打开本方法。
  ///
  /// 需要**上报数据**请优先用 [`Self::report`]；需要**主动广播状态帧**请优先
  /// 用 [`Self::send_frame`]。
  ///
  /// # 语义
  /// - 自动分配 seq（`keyring.next_seq()`）
  /// - 自动使用 keyring 的 active key_id 计算 HMAC
  #[cfg(feature = "endpoint-initiated-command")]
  pub fn send_command(&self, body: CommandBody) {
    let seq = self.keyring.next_seq();
    let cmd = Command::with_key(seq, self.keyring.active(), body);
    // Endpoint 主动命令仍走广播（Phase 1 只单播 Notifier→Endpoint 的 AssignId）。
    crate::notifier::signals::enqueue_command(
      self.command_signal,
      crate::notifier::signals::OutboundCommand::broadcast(encode_command(&cmd)),
    );
  }

  // ---- 后台 loop（门面自带；各吃一个 link 端）----

  /// 运行**广播 loop**（送出主动上行的 Frame / Command / Response）
  ///
  /// 门面版：消费自身三条出站通道，调用方只需传 **send 端** link。
  pub async fn run_broadcast_loop<L: CommLink>(&self, link: L) -> !
  where
    L::Addr: From<[u8; 6]>,
  {
    run_broadcast_loop(
      link,
      self.frame_signal,
      self.command_signal,
      self.response_signal,
    )
    .await
  }

  /// 运行**接收 loop**（解码 / 抗重放 / 命令派发 / 自动回执）
  ///
  /// 门面版：从自身字段拼出 [`ReceiverRecvConfig`]，调用方只需传 **recv 端** link。
  pub async fn run_receive_loop<L: CommLink>(&self, link: L) -> ! {
    run_receive_loop(
      link,
      ReceiverRecvConfig {
        keyring: self.keyring,
        replay: self.replay,
        response_signal: self.response_signal,
        role_tag: self.role_tag,
        my_mac: self.my_mac,
        my_id: self.my_id,
        handler: self.handler,
        src: self.src,
        frame_handler: self.frame_handler,
      },
    )
    .await
  }
}

// ============================================================
// 后台 loop
// ============================================================

/// [`run_receive_loop`] 的静态接线配置
///
/// 把接收 loop 需要的**共享 static 状态**与 handler 打包成一个 `Copy` 结构体，
/// 替代原来的 9 个位置参数。好处：
/// - 消掉 `#[allow(clippy::too_many_arguments)]`
/// - 调用方用**具名字段**装配，避免同类型参数（`role_tag` / `my_mac` 等）被顺序写错
///
/// 本结构体**同时**充当 crate 内部派发模块的派发上下文（那边的 `DispatchCtx`
/// 就是本类型的别名），因此接收 loop 无需再做一次逐字段搬运。
#[derive(Clone, Copy)]
pub struct ReceiverRecvConfig {
  /// 共享 keyring（active key / seq）
  pub keyring: &'static Keyring,
  /// 抗重放窗口
  pub replay: &'static ReplayGuard,
  /// 出站响应队列（自动 Ack / 富回执）
  pub response_signal: &'static ResponseChannel,
  /// 本机 role tag（回 AnnounceReply 时携带）
  pub role_tag: [u8; 3],
  /// 本机 MAC（回 AnnounceReply / 判断 AssignId 目标时用）
  pub my_mac: [u8; 6],
  /// 收到 AssignId 后写入的 `receiver_id` 存储
  pub my_id: &'static AtomicU8,
  /// 用户业务命令处理器
  pub handler: CommandHandler,
  /// 本 loop 处理的所有入站帧默认来源（完整透传到 handler）
  pub src: CommandSource,
  /// 可选 Frame handler；`None` 时入站 `Frame` 被静默丢弃
  pub frame_handler: Option<FrameHandler>,
}

/// 从 link 里连续 recv，解码 → 派发 → 自动回执
///
/// # 入站帧处理
/// - `Command`（`Announce` / `AssignId` / 业务命令）：解码 + HMAC + 抗重放 → 派发
/// - `Frame`：解码 + `dest_mask` 过滤 → 可选 `frame_handler`
/// - `Response`：**仅**消费 `NonceHello` 以同步 session nonce（K3 bootstrap，见
///   crate 内部派发模块）；其余 Response 变体静默丢弃。这条路径让 Endpoint 无需
///   应用层介入即可与 Coordinator 对齐 HMAC nonce，是 Command 验签能通过的前提。
///
/// # 适用场景
/// - 纯 receiver-only 设备（led / motor / 伯服等只收命令 / 可选接收手柄 Frame 状态）
///
/// # 与 Notifier 的关系
/// 如果你需要的是"既发 Frame 又收 Command"（比如手柄设备），请使用
/// [`crate::Notifier::builder`] + `.with_command_handler(...)` 以及
/// [`crate::notifier::run_receive_loop`] —— 仅需一条 recv 通道就能同时处理
/// 两种入站帧。
///
/// # 用户包装示例
/// ```ignore
/// #[embassy_executor::task]
/// async fn my_recv_task(link: MyLink) -> ! {
///     comm::receiver::run_receive_loop(
///         link,
///         comm::receiver::ReceiverRecvConfig {
///             keyring: &KEYRING,
///             replay: &REPLAY,
///             response_signal: &RESP_SIG,
///             role_tag: *b"led",
///             my_mac: MY_MAC,
///             my_id: &MY_ID,
///             handler: on_command,
///             src: comm::CommandSource::EspNow,
///             frame_handler: Some(on_frame),
///         },
///     ).await
/// }
/// ```
pub async fn run_receive_loop<L: CommLink>(mut link: L, cfg: ReceiverRecvConfig) -> ! {
  // `ReceiverRecvConfig` 本身就是内部派发上下文（`DispatchCtx` 是它的别名），
  // 直接原样传入，无需再逐字段搬运一遍。
  loop {
    let Ok(packet) = link.recv().await else {
      continue;
    };
    dispatch_packet(packet.data, cfg);
  }
}
