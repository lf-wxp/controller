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
//! 1. 实现 [`CommLink`](crate::CommLink)
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

use controller_protocol::{Command, CommandResponse, ErrorCode, Frame, ResponseBody};
// 仅 `endpoint-initiated-command` opt-in 下才用到；默认关闭时避免 unused import warning。
#[cfg(feature = "endpoint-initiated-command")]
use controller_protocol::{CommandBody, encode_command};

use crate::dispatch::{DispatchCtx, dispatch_packet};
use crate::keyring::Keyring;
use crate::link::CommLink;
use crate::notifier::signals::{CommandOutSignal, FrameSignal, ResponseSignal};
use crate::replay::ReplayGuard;

pub use builder::ReceiverBuilder;

/// Receiver 端出站 loop —— 直接复用 [`crate::notifier::run_broadcast_loop`]
///
/// # 复用理由
/// 出站编排逻辑（三路 select：Frame + Response + Command）与 Notifier 完全一致，
/// 一份实现两处复用（`proj-pub-use-reexport`），避免维护双份 select 树。
///
/// # 使用示例
/// ```ignore
/// #[embassy_executor::task]
/// async fn receiver_broadcast_task(link: MyLink) -> ! {
///     comm::receiver::run_broadcast_loop(link, &FRAME_SIG, &CMD_SIG, &RESP_SIG).await
/// }
/// ```
pub use crate::notifier::run_broadcast_loop;

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
  /// [`ResponseSignal`]
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
/// # `dead_code` 说明
/// 字段供外部 [`run_receive_loop`] 读取；本结构体自身不循环，因此本地未读，
/// 采用字段级 `#[allow(dead_code)]` 只隔离真正无本地读写的字段。
pub struct Receiver<L: CommLink> {
  pub(crate) link: L,
  pub(crate) keyring: &'static Keyring,
  #[allow(dead_code)]
  pub(crate) replay: &'static ReplayGuard,
  pub(crate) response_signal: &'static ResponseSignal,
  /// Frame 出站 Signal —— 供 [`Receiver::send_frame`] 主动上行
  pub(crate) frame_signal: &'static FrameSignal,
  /// Command 出站 Signal —— 供 [`Receiver::send_command`] 主动上行
  ///
  /// # `dead_code` 说明
  /// 仅在 `endpoint-initiated-command` feature 开启时才被 [`Receiver::send_command`]
  /// 读取；默认关闭下无读点，用字段级 `allow` 屏蔽 lint 而不牵连整个 struct。
  #[allow(dead_code)]
  pub(crate) command_signal: &'static CommandOutSignal,
  #[allow(dead_code)]
  pub(crate) role_tag: [u8; 3],
  #[allow(dead_code)]
  pub(crate) my_mac: [u8; 6],
  pub(crate) my_id: &'static AtomicU8,
  #[allow(dead_code)]
  pub(crate) handler: CommandHandler,
  /// 可选的 Frame handler：`None` 时 receiver 不消费入站 `Frame`
  #[allow(dead_code)]
  pub(crate) frame_handler: Option<FrameHandler>,
}

impl<L: CommLink> Receiver<L> {
  /// 开始构造 Receiver
  //
  // 无需 `#[must_use]`：`ReceiverBuilder` 已在结构体层面标了 `#[must_use]`，
  // 这里再加会触发 clippy::double_must_use。
  pub const fn builder() -> ReceiverBuilder<L> {
    ReceiverBuilder::<L>::new()
  }

  /// 借用 link（供后台 loop 使用）
  pub fn link_mut(&mut self) -> &mut L {
    &mut self.link
  }

  /// 当前分配到的 receiver_id（未分配时返回 `UNASSIGNED_ID`）
  #[must_use]
  pub fn assigned_id(&self) -> u8 {
    self.my_id.load(Ordering::Relaxed)
  }

  // ---- Getter（与 [`crate::Notifier`] 对称）----

  /// 借用内部 [`ResponseSignal`]（供 nonce 广播 / 外部 report 复用）
  ///
  /// # 用途
  /// - `run_nonce_broadcast_loop` 需要 `&'static ResponseSignal` 才能塞
  ///   [`CommandResponse`]；直接暴露避免用户在 crate 里再维护一份别名
  /// - 业务侧可以复用该 signal 做自定义上报（不通过 [`Self::report`]）
  #[must_use]
  pub fn response_signal(&self) -> &'static ResponseSignal {
    self.response_signal
  }

  /// 借用内部 [`Keyring`]（供 key 轮换 / 会话 nonce 初始化等场景）
  #[must_use]
  pub fn keyring(&self) -> &'static Keyring {
    self.keyring
  }

  /// 本机 MAC 地址（3 字节以下的地址请靠自己 pad）
  #[must_use]
  pub fn my_mac(&self) -> [u8; 6] {
    self.my_mac
  }

  /// 本机 role tag（3 字节 ASCII）
  #[must_use]
  pub fn role_tag(&self) -> [u8; 3] {
    self.role_tag
  }

  // ---- 主动出站 API（endpoint-initiated publishing）----

  /// 主动上报一条 [`ResponseBody`]（**Endpoint → Coordinator** 的主要通道）
  ///
  /// # 语义
  /// - `req_seq = 0`：表示"非请求触发"（与 [`ResponseBody::NonceHello`] 广播一致）
  /// - `key_id`：使用 keyring 当前 active key
  /// - 覆盖式：若上一条上报还没被 [`run_broadcast_loop`] 消费，本次会覆盖它
  ///   （高频上报场景通常只关心最新值；如需可靠队列请自行改用 channel）
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
    self.response_signal.signal(resp);
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
    self.command_signal.signal(encode_command(&cmd));
  }
}

// ============================================================
// 仅测试可见的构造入口
// ============================================================

/// 仅测试用：不依赖真实 link 构造一个 Receiver 实体
///
/// # 存在动机
/// 生产 API `Receiver::builder()` 强制填入 `link`，且 link 会被 broadcast/receive
/// loop 拿走 `&mut`。集成测试里 loop 已经用 `LoopbackSendEnd`/`LoopbackRecvEnd`
/// spawn 掉了，此时若要**同时**验证 `Receiver::report()` / `Receiver::send_frame()`
/// 等 `&self` API 的真实行为，就需要一个"只挂 signals & keyring、不占 link"的
/// 轻量实体。
///
/// # `test-utils` feature 门控
/// 只有开启 `test-utils` feature 时才能编译到，防止生产代码误用。
///
/// # 语义
/// - `link` 用一个 dummy `()`——因此**不要**再把返回的实体拿去 spawn loop
/// - `handler` / `frame_handler` 传 `None` 时用哨兵；本函数不消费入站帧，无所谓
/// - 除 link 之外的字段与生产 build 完全一致
#[cfg(feature = "test-utils")]
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn test_receiver_from_parts(
  keyring: &'static Keyring,
  replay: &'static ReplayGuard,
  response_signal: &'static ResponseSignal,
  frame_signal: &'static FrameSignal,
  command_signal: &'static CommandOutSignal,
  role_tag: [u8; 3],
  my_mac: [u8; 6],
  my_id: &'static AtomicU8,
  handler: CommandHandler,
) -> Receiver<crate::link::DummyLink> {
  Receiver {
    link: crate::link::DummyLink,
    keyring,
    replay,
    response_signal,
    frame_signal,
    command_signal,
    role_tag,
    my_mac,
    my_id,
    handler,
    frame_handler: None,
  }
}

// ============================================================
// 后台 loop
// ============================================================

/// 从 link 里连续 recv，解码 → 派发 → 自动回执
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
///     comm::receiver::run_receive_loop(link, ...).await
/// }
/// ```
///
/// # 参数
/// - `src`：本 loop 处理的所有入站帧默认来源；完整透传到用户 handler
/// - `frame_handler`：可选；`None` 时入站 `Frame` 被静默丢弃
#[allow(clippy::too_many_arguments)]
pub async fn run_receive_loop<L: CommLink>(
  mut link: L,
  keyring: &'static Keyring,
  replay: &'static ReplayGuard,
  response_signal: &'static ResponseSignal,
  role_tag: [u8; 3],
  my_mac: [u8; 6],
  my_id: &'static AtomicU8,
  handler: CommandHandler,
  src: CommandSource,
  frame_handler: Option<FrameHandler>,
) -> ! {
  let ctx = DispatchCtx {
    keyring,
    replay,
    response_signal,
    role_tag,
    my_mac,
    my_id,
    handler,
    src,
    frame_handler,
  };
  loop {
    let Ok(packet) = link.recv().await else {
      continue;
    };
    dispatch_packet(packet.data, ctx);
  }
}
