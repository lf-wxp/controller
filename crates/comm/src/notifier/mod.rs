//! # Notifier —— 发送端门面（Coordinator 角色）
//!
//! ## 角色定位
//! `Notifier` 在网络中扮演 **Coordinator（协调者）** 角色，是整个 Star Topology
//! 的中心。请把它与 [`crate::Receiver`]（**Endpoint**，端点）严格区分开——两者
//! 虽然在**消息能力**层面已完全对称（都支持发送/接收 Frame / Command / Response），
//! 但**职责**是不对称的：
//!
//! | 维度 | Notifier（Coordinator） | Receiver（Endpoint） |
//! |---|---|---|
//! | 拥有 `PeerRegistry` | ✅ 目录权威方 | ❌ 无目录 |
//! | 拥有 `Selector` | ✅ 决定下发目标 | ❌ 无决策权 |
//! | 主动 `discover()` | ✅ 发起会话 | ❌ 只能被动响应 |
//! | 首次遇到新 peer 时回 AssignId | ✅ 自动 | ❌ 只接收 AssignId |
//! | 主动 `send_frame` / 主动 `send_command` | ✅ | ✅（Endpoint 侧的 `send_command` 需 opt-in） |
//! | 处理入站 Command | ✅（双身份可选）| ✅（必填 `command_handler`）|
//! | 处理入站 Response（含 receiver 上报） | ✅（可选 `response_handler`）| —— |
//!
//! **一句话记忆**：`Notifier = Coordinator` 主导会话与拓扑；
//! `Receiver = Endpoint` 是被协调的叶子节点，即便它也能主动上报数据。
//!
//! ## 作用
//! 把"发送 Frame / 主动发现 peer / 选择 receiver / 处理入站 Response（包括
//! AnnounceReply → upsert peer）/ 密钥管理 / 抗重放"这套编排逻辑封装成一个
//! 通用结构体，让使用者只需要：
//!
//! 1. 实现 [`CommLink`](crate::CommLink)（唯一硬件抽象）
//! 2. 在自己 crate 里放几个 `static Signal / Keyring / PeerRegistry / ...`
//! 3. 用 [`Notifier::builder()`] 组装，然后 spawn 两个后台任务
//! 4. 主循环里调 `notifier.send_frame(&frame)`
//!
//! ## 内部架构
//! ```text
//!   主循环 ──frame──► FrameSignal ──┐
//!                                   ├──► broadcast_loop ──► CommLink::send
//!   comm 内部 ──cmd──► CmdSignal ────┤
//!                                   │
//!   handler ──resp──► RespSignal ───┘
//!
//!   CommLink::recv ──► receive_loop ──► [Response] → 更新 PeerRegistry
//!                                       [Command]  → 转给用户 handler（未来扩展）
//! ```

pub mod builder;
pub mod nonce;
pub mod signals;

use protocol::{
  COMMAND_LEN, COMMAND_MAGIC, Command, CommandBody, CommandResponse, FRAME_LEN, FRAME_MAGIC, Frame,
  KeyId, RESPONSE_LEN, RESPONSE_MAGIC, ResponseBody, decode_response, encode_command, encode_frame,
};

use crate::dispatch::{DispatchCtx, dispatch_command_frame, dispatch_frame};
use crate::keyring::Keyring;
use crate::link::CommLink;
use crate::peer_registry::{PeerRegistry, UpsertOutcome};
use crate::replay::ReplayGuard;
use crate::selector::Selector;

pub use builder::{CommandHandlerConfig, NotifierBuilder};
pub use nonce::{
  DEFAULT_NONCE_BROADCAST_INTERVAL, EntropySource, init_session, run_nonce_broadcast_loop,
};
pub use signals::{CommandDest, CommandOutSignal, FrameSignal, OutboundCommand, ResponseSignal};

/// 上报 Response 处理闭包（供上层业务消费 Receiver 主动 report 的数据）
///
/// # 何时被调用
/// 在 [`run_receive_loop`] 里，入站 [`CommandResponse`] 除了 comm 自己消费的
/// `AnnounceReply` 分支之外（用于 peer upsert），其余变体（`Ack` / `Error` /
/// `BatterySnapshot` / `NonceHello` 等）会被转给本回调。
///
/// # 为什么不盖盖 `AnnounceReply`
/// AnnounceReply 是 comm 内部发现机制的一部分，反馈到业务只会造成误解；
/// 如果业务真的关心 peer 入库事件，应直接听 [`crate::PeerRegistry`] 快照。
pub type ResponseHandler = fn(&CommandResponse);

// ============================================================
// NotifierError
// ============================================================

/// Notifier 相关错误
///
/// # 不完备枚举
/// 未来可能新增链路下行/耐性错误变体，标 `#[non_exhaustive]` 以保留演进空间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NotifierError {
  /// 底层链路失败
  Link,
  /// 无目标 receiver（既没选广播，也没选任何 peer）
  NoTarget,
}

#[cfg(feature = "defmt")]
impl defmt::Format for NotifierError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::Link => defmt::write!(f, "NotifierError::Link"),
      Self::NoTarget => defmt::write!(f, "NotifierError::NoTarget"),
    }
  }
}

// ============================================================
// Notifier
// ============================================================

/// 发送端门面
///
/// # 生命周期
/// 通常放进 `StaticCell` 使其成为 `'static`，然后把后台 task 需要的字段
/// 单独 spawn 出去。参见 [`run_broadcast_loop`] / [`run_receive_loop`]。
///
/// # `dead_code` 说明
/// `replay` / `response_signal` 字段供用户在外层 loop 中读取（他们把这些
/// `&'static` 引用同时传给 `run_receive_loop` / `run_broadcast_loop`）。
/// 本结构体自身没有 `.await` 循环，因此本地读不到它们，字段级 `#[allow(dead_code)]`
/// 显式局部隔离，不影响其他字段与方法的 lint 保护。
pub struct Notifier<L: CommLink> {
  pub(crate) link: L,
  pub(crate) keyring: &'static Keyring,
  pub(crate) peers: &'static PeerRegistry,
  #[allow(dead_code)]
  pub(crate) replay: &'static ReplayGuard,
  pub(crate) selector: &'static Selector,
  pub(crate) frame_signal: &'static FrameSignal,
  pub(crate) command_signal: &'static CommandOutSignal,
  pub(crate) response_signal: &'static ResponseSignal,
  /// 可选 command handler —— 启用后 Notifier 变成"双身份"（既发又收命令）
  #[allow(dead_code)]
  pub(crate) handler_config: Option<CommandHandlerConfig>,
}

impl<L: CommLink> Notifier<L> {
  /// 开始构造 Notifier
  //
  // 无需 `#[must_use]`：`NotifierBuilder` 结构体已在类型层面标了 `#[must_use]`，
  // 函数级注解会触发 `clippy::double_must_use`。
  pub const fn builder() -> NotifierBuilder<L> {
    NotifierBuilder::<L>::new()
  }

  // ---- 无 link 依赖的同步 API：`&self` 就够，可以在主循环里随便调 ----

  /// 广播状态帧（**主循环入口 #1**）
  ///
  /// 内部把 Frame 塞进 [`FrameSignal`]；`broadcast_loop` 后台任务会取出编码后
  /// 通过 [`CommLink::send`] 发出去。
  ///
  /// # 语义
  /// - 覆盖式：若上一帧还没被消费，本次会覆盖它 —— 高频状态流场景**只关心最新**
  /// - 同步：不阻塞主循环
  pub fn send_frame(&self, frame: &Frame) {
    self.frame_signal.signal(*frame);
  }

  /// 主动发起一次 peer 发现（**主循环入口 #2**）
  ///
  /// 广播一条 `CommandBody::Announce`；网内所有 receiver 收到后应回
  /// `ResponseBody::AnnounceReply`，被本 Notifier 的 `receive_loop` 消费。
  pub fn discover(&self) {
    let seq = self.keyring.next_seq();
    let cmd = Command::with_key(seq, self.keyring.active(), CommandBody::Announce);
    // Announce 必须广播：目标恰恰是"尚未发现的" receiver，无 MAC 可单播。
    self
      .command_signal
      .signal(OutboundCommand::broadcast(encode_command(&cmd)));
  }

  /// 发送任意 Command（比如 LedBlink / ShowToast）
  ///
  /// # 参数
  /// - `body`：命令载荷
  ///
  /// # 语义
  /// - 自动分配 seq（从 keyring 当前 active slot 的 tx_counter fetch_add）
  /// - 自动使用 keyring 的 active key_id 计算 HMAC
  pub fn send_command(&self, body: CommandBody) {
    let seq = self.keyring.next_seq();
    let cmd = Command::with_key(seq, self.keyring.active(), body);
    // 广播式 send_command：发给全网所有 receiver（fire-and-forget，无 ACK）。
    self
      .command_signal
      .signal(OutboundCommand::broadcast(encode_command(&cmd)));
  }

  /// 单播一条 Command 给指定 `receiver_id`（**Phase 2：定向命令**）
  ///
  /// 从 [`PeerRegistry`](crate::PeerRegistry) 反查该 id 的 MAC，走
  /// [`CommandDest::Unicast`]：ESP-NOW MAC 层 ACK + `run_broadcast_loop` 有界重试，
  /// 送达可靠性远高于广播。适合 LedBlink / ShowToast / SetSensitivity 等只想发给
  /// **某一台** receiver 的业务命令。
  ///
  /// # Errors
  /// - [`NotifierError::NoTarget`]：`receiver_id` 尚未在 registry 里（未发现 / 已过期）
  ///
  /// # ⚠️ 单目标限制（覆盖式信号）
  /// [`CommandOutSignal`] 是**覆盖式** `Signal`（后写覆盖前写），因此**不要**在紧凑
  /// 循环里对多个目标连发——除最后一条外都会在被 `broadcast_loop` 消费前被覆盖丢弃。
  /// 需要"发给多台"请分帧节流，或等未来把命令出站通道换成有界队列后再支持组播。
  pub fn send_command_to(&self, receiver_id: u8, body: CommandBody) -> Result<(), NotifierError> {
    let mac = self
      .peers
      .lookup_mac_for_id(receiver_id)
      .ok_or(NotifierError::NoTarget)?;
    self.send_command_to_mac(mac, body);
    Ok(())
  }

  /// 单播一条 Command 给指定 MAC（跳过 registry 反查）
  ///
  /// 当调用方**已经**持有目标 MAC（比如来自一次 AnnounceReply 快照）时用它，省一次
  /// 反查。语义与 [`send_command_to`](Self::send_command_to) 相同，见其"单目标限制"。
  pub fn send_command_to_mac(&self, mac: [u8; 6], body: CommandBody) {
    let seq = self.keyring.next_seq();
    let cmd = Command::with_key(seq, self.keyring.active(), body);
    self
      .command_signal
      .signal(OutboundCommand::unicast(mac, encode_command(&cmd)));
  }

  /// 拿 peer 列表快照（用于 UI 渲染 / 选择器候选）
  #[must_use]
  pub fn peers(&self) -> heapless::Vec<crate::PeerInfo, { crate::peer_registry::MAX_PEERS }> {
    self.peers.snapshot()
  }

  /// 直接设置 active dest_mask（跳过 pending 编辑）
  pub fn select_targets(&self, mask: crate::selector::DestMask) {
    self.selector.set_active(mask);
  }

  /// 借用 selector 做交互编辑（`toggle_pending` / `commit` / `cancel`）
  #[must_use]
  pub fn selector(&self) -> &'static Selector {
    self.selector
  }

  /// 切换 active key_id
  ///
  /// # Errors
  /// 见 [`KeyringError`](crate::KeyringError)
  pub fn rotate_key(&self, new_id: KeyId) -> Result<(), crate::keyring::KeyringError> {
    self.keyring.rotate_to(new_id)
  }

  /// 借用内部 [`ResponseSignal`]（供 [`run_nonce_broadcast_loop`] 复用）
  ///
  /// # 用途
  /// nonce 广播任务需要 `&'static ResponseSignal` 才能塞 [`CommandResponse`]；
  /// Notifier 建造时已经持有这份 static 引用，直接暴露出来避免用户再单独维护
  /// 一份别名。
  ///
  /// # 使用示例
  /// ```ignore
  /// #[embassy_executor::task]
  /// async fn nonce_task() -> ! {
  ///     comm::notifier::run_nonce_broadcast_loop(
  ///         NOTIFIER.response_signal(),
  ///         comm::notifier::DEFAULT_NONCE_BROADCAST_INTERVAL,
  ///     ).await
  /// }
  /// ```
  #[must_use]
  pub fn response_signal(&self) -> &'static ResponseSignal {
    self.response_signal
  }

  /// 从 [`EntropySource`] 采样一次并写入 protocol 层的 `SESSION_NONCE`
  ///
  /// 语义等价于顶层 [`init_session`]；挂在 `Notifier` 上是为了让
  /// "一句话完成 comm 初始化"更顺手。
  ///
  /// # 参数
  /// - `entropy`：任意 [`EntropySource`] 实现；采样后可丢弃
  ///
  /// # 返回
  /// 实际写入的 nonce 值（便于日志打印）
  pub fn init_session<E: EntropySource + ?Sized>(&self, entropy: &mut E) -> u32 {
    init_session(entropy)
  }

  /// 借用 link（供 [`spawn_broadcast_loop`] / [`spawn_receive_loop`] 拆用）
  ///
  /// # 谨慎使用
  /// 只应该在后台 task 内部使用；主循环拿到 `&mut L` 会破坏"send/recv 分离在两个
  /// task"的架构。
  pub fn link_mut(&mut self) -> &mut L {
    &mut self.link
  }
}

// ============================================================
// 后台 loop（由用户在自己的 crate 里用 #[embassy_executor::task] 包一层）
// ============================================================

/// **广播 loop** —— 用户在自己的 `#[embassy_executor::task]` 里调用本函数
///
/// # 为什么不是 `#[embassy_executor::task]` 直接标注？
/// embassy 的 `#[task]` 硬性禁止泛型函数（macro 展开时需要具体类型）；
/// 因此本 crate 只提供泛型 async fn，用户在自己 crate 里写：
/// ```ignore
/// #[embassy_executor::task]
/// async fn my_broadcast_task(link: MyLink) -> ! {
///     comm::notifier::run_broadcast_loop(link, &SIG_FRAME, &SIG_CMD, &SIG_RESP).await
/// }
/// ```
///
/// # 三路 select 语义
/// 与手柄原 `esp_now_broadcast_task` 保持一致：
/// 1. `Frame`（高频广播状态）
/// 2. `Response`（低频回执，从命令 handler 侧塞入）
/// 3. `CommandOut`（Announce / 自发 Command）
///
/// 任何一路就绪即取出编码后发出；失败时静默日志（不 panic，避免链路故障
/// 导致整机 crash）。
///
/// # 命令通路的寻址与可靠性（Phase 1）
/// `CommandOut` 载荷为 [`OutboundCommand`]，可选广播或单播：
/// - [`CommandDest::Broadcast`]：与 Frame / Response 相同的 fire-and-forget 广播
/// - [`CommandDest::Unicast`]：把 MAC 经 `L::Addr::from` 转成链路地址后单播；ESP-NOW
///   会给 MAC 层 ACK，`send` 返回 `Err` 才说明彻底失败。此时做 [`MAX_UNICAST_SEND_RETRIES`]
///   次有界补发（次数刻意很小，避免拖累与本 loop 共享的高频 Frame 出站节拍）。
///
/// # `L::Addr: From<[u8; 6]>` 约束
/// 单播目标以 MAC-48 表达；本 loop 需要把它转成具体链路地址。所有真实链路
/// （ESP-NOW / loopback）的 `Addr` 都是 `[u8; 6]`，`From` 为自反实现，零成本；
/// `Addr = ()` 的 `DummyLink` 不会进本 loop，故不受影响。
pub async fn run_broadcast_loop<L: CommLink>(
  mut link: L,
  frame_signal: &'static FrameSignal,
  command_signal: &'static CommandOutSignal,
  response_signal: &'static ResponseSignal,
) -> !
where
  L::Addr: From<[u8; 6]>,
{
  use embassy_futures::select::{Either3, select3};
  loop {
    match select3(
      frame_signal.wait(),
      response_signal.wait(),
      command_signal.wait(),
    )
    .await
    {
      Either3::First(frame) => {
        let bytes = encode_frame(&frame);
        // 忽略 send 错误 —— 链路可能暂时不可用；下一帧再试
        let _ = link.send(L::BROADCAST, &bytes).await;
      }
      Either3::Second(resp) => {
        let bytes = protocol::encode_response(&resp);
        let _ = link.send(L::BROADCAST, &bytes).await;
      }
      Either3::Third(out) => match out.dest {
        CommandDest::Broadcast => {
          let _ = link.send(L::BROADCAST, &out.bytes).await;
        }
        CommandDest::Unicast(mac) => {
          let dst = L::Addr::from(mac);
          let mut attempt = 0_u8;
          loop {
            match link.send(dst, &out.bytes).await {
              Ok(()) => break,
              Err(_) if attempt < MAX_UNICAST_SEND_RETRIES => attempt += 1,
              Err(_) => break, // 放弃：下一轮 AnnounceReply 仍会幂等补发（见 handle_incoming_response）
            }
          }
        }
      },
    }
  }
}

/// 单播命令在 `send` 返回 `Err` 后的应用层补发次数上限。
///
/// ESP-NOW 硬件本身已在 MAC 层重传若干次；这里的补发只覆盖 `add_peer` 时序 /
/// 瞬时队列满等边界。刻意取小值，避免离线单播目标把与本 loop 共享的高频 Frame
/// 出站节拍拖垮。
pub const MAX_UNICAST_SEND_RETRIES: u8 = 2;

/// **接收 loop** —— 从 link 里连续 recv，解析并派发
///
/// # 基本行为（纯 notifier 场景，`handler_config = None`）
/// - `RESPONSE_LEN` 帧 → AnnounceReply → `peers.upsert(...)` → 回 AssignId（每次
///   AnnounceReply 都重发，以自愈覆盖式信号被覆盖 / 射频丢包造成的 AssignId 丢失）
/// - `COMMAND_LEN` 帧 → 静默忽略（自发命令广播回环，无消费者）
///
/// # 启用 "双身份" 后的行为（`handler_config = Some(...)`）
/// - `RESPONSE_LEN` 帧：同上
/// - `COMMAND_LEN` 帧：
///   * `Announce` → 内置回 AnnounceReply（role_tag + my_mac 自配置取）
///   * `AssignId` → 当 mac 匹配时写入 my_id
///   * 其它业务 Command → anti-replay 校验 → handler 派发 → 自动 Ack/Err/Respond/NoReply
///
/// 用户在自己 crate 里包一层 task：
/// ```ignore
/// #[embassy_executor::task]
/// async fn my_recv_task(link: MyLink) -> ! {
///     comm::notifier::run_receive_loop(
///         link, &PEERS, &CMD_SIG, &KEYRING, &REPLAY, &RESP_SIG,
///         Some(comm::notifier::CommandHandlerConfig { .. }),
///     ).await
/// }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn run_receive_loop<L: CommLink>(
  mut link: L,
  peers: &'static PeerRegistry,
  command_signal: &'static CommandOutSignal,
  keyring: &'static Keyring,
  replay: &'static ReplayGuard,
  response_signal: &'static ResponseSignal,
  handler_config: Option<CommandHandlerConfig>,
  response_handler: Option<ResponseHandler>,
) -> ! {
  loop {
    let Ok(packet) = link.recv().await else {
      // 接收错误：继续下一轮；一直失败会自然节流
      continue;
    };
    if packet.data.len() < 2 {
      continue;
    }
    let magic = u16::from_le_bytes([packet.data[0], packet.data[1]]);
    match magic {
      RESPONSE_MAGIC if packet.data.len() == RESPONSE_LEN => {
        if let Ok(resp) = decode_response(packet.data) {
          handle_incoming_response(&resp, peers, command_signal, keyring, response_handler);
        }
      }
      COMMAND_MAGIC if packet.data.len() == COMMAND_LEN => {
        if let Some(cfg) = handler_config.as_ref() {
          dispatch_command_frame(
            packet.data,
            DispatchCtx {
              keyring,
              replay,
              response_signal,
              role_tag: cfg.role_tag,
              my_mac: cfg.my_mac,
              my_id: cfg.my_id,
              handler: cfg.handler,
              src: cfg.src,
              frame_handler: cfg.frame_handler,
            },
          );
        }
        // 无 handler 时静默忽略（自发命令广播回环的情况）
      }
      FRAME_MAGIC if packet.data.len() == FRAME_LEN => {
        // 双身份场景：手柄同时也订阅了 Frame（少见，但调试 / host 监听时需要）。
        // `dispatch_frame` 内部会在 `frame_handler.is_none()` 时 short-circuit，
        // 因此这里无需再嵌套判断——即使 cfg 未启用 frame_handler 也是零成本的。
        if let Some(cfg) = handler_config.as_ref() {
          dispatch_frame(
            packet.data,
            DispatchCtx {
              keyring,
              replay,
              response_signal,
              role_tag: cfg.role_tag,
              my_mac: cfg.my_mac,
              my_id: cfg.my_id,
              handler: cfg.handler,
              src: cfg.src,
              frame_handler: cfg.frame_handler,
            },
          );
        }
        // handler_config = None 时（纯 notifier）静默忽略——本来就不消费自发 Frame 回环
      }
      _ => {
        // 未知 magic 或长度不匹配：静默丢弃
      }
    }
  }
}

/// 处理入站 Response（主要是 AnnounceReply，以及可选的业务 response_handler）
///
/// # 副作用
/// - `AnnounceReply` → `peers.upsert(...)` + 回 AssignId（comm 内部机制；每次
///   AnnounceReply 都重发，幂等自愈，详见函数体注释）
/// - 其他变体（`Ack` / `Error` / `NonceHello` / `BatterySnapshot` 等）：
///   若 `response_handler = Some(fn)` 则回调；否则静默丢弃
fn handle_incoming_response(
  resp: &CommandResponse,
  peers: &'static PeerRegistry,
  command_signal: &'static CommandOutSignal,
  keyring: &'static Keyring,
  response_handler: Option<ResponseHandler>,
) {
  match resp.body {
    ResponseBody::AnnounceReply {
      mac,
      rssi_dbm,
      role_tag,
    } => {
      // 无论首次入库（`Inserted`）还是刷新已有条目（`Updated`），都回一条 AssignId。
      //
      // # Phase 1：AssignId 单播 + MAC 层 ACK + 有界重试
      // AssignId 现在**单播**到该 peer 的 MAC（[`OutboundCommand::unicast`]）：
      // - ESP-NOW 单播带 MAC 层 ACK + 硬件重传，`run_broadcast_loop` 再做少量应用层
      //   补发，送达可靠性远高于旧的广播 fire-and-forget。
      // - 目标 MAC 现成（就是本次 AnnounceReply 的 `mac`），无需反查。
      //
      // # 为什么仍在每次 AnnounceReply（含 `Updated`）都重发？
      // 保留这份**幂等自愈**作为兜底：单播仍可能因 (1) 覆盖式 `CommandOutSignal`
      // 在被 `broadcast_loop` 消费前被后续命令覆盖、(2) peer 离线导致重试耗尽而失败。
      // 若只在 `Inserted` 时发一次，一旦丢失，该 peer 会**永久**停留在
      // [`crate::receiver::UNASSIGNED_ID`]。每次重发是幂等的：同一 MAC 的
      // `receiver_id` 由 registry 稳定保序分配、不会变化。发现流量极低频
      // （仅在 controller 广播 Announce 后出现），成本可忽略。
      let assigned = match peers.upsert(mac, role_tag, rssi_dbm, embassy_time::Instant::now()) {
        UpsertOutcome::Inserted { receiver_id } | UpsertOutcome::Updated { receiver_id } => {
          Some(receiver_id)
        }
        UpsertOutcome::Full => None,
      };
      if let Some(receiver_id) = assigned {
        let seq = keyring.next_seq();
        let cmd = Command::with_key(
          seq,
          keyring.active(),
          CommandBody::AssignId { mac, receiver_id },
        );
        command_signal.signal(OutboundCommand::unicast(mac, encode_command(&cmd)));
      }
    }
    _ => {
      // 其他 Response kind：如果业务注册了 response_handler 就转发，否则静默
      if let Some(handler) = response_handler {
        handler(resp);
      }
    }
  }
}
