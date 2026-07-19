//! # `NotifierBuilder` —— 简化 builder
//!
//! ## 设计选择
//! 早期版本尝试过完整的 typestate（每个必填字段占一个泛型状态位），但 8 个必填
//! 字段会产生 `2^8` 组组合级别的 impl 展开，可读性和文档噪音成本远高于收益。
//! 现在采用「所有字段 `Option` 存储、`build()` 内 `expect` 校验」的实用主义
//! 方案 —— 生产 crate 中 `notifier::builder()...build()` 会在启动最早期就
//! 触发 panic，配合明确的错误消息 & 单元测试完全能替代编译期强制。
//!
//! 这是 rust-skills [`api-builder-pattern`] 明确接受的实用主义妥协。
//!
//! ## 使用示例
//! ```ignore
//! Notifier::builder()
//!   .link(link)                   // 必填
//!   .keyring(&KEYRING)             // 必填
//!   .peers(&PEERS)                 // 必填
//!   .replay(&REPLAY)               // 必填
//!   .selector(&SELECTOR)           // 必填
//!   .frame_signal(&SIG_FRAME)      // 必填
//!   .command_signal(&SIG_CMD)      // 必填
//!   .response_signal(&SIG_RESP)    // 必填
//!   .with_command_handler(...)     // 可选：双身份场景
//!   .build();
//! ```
//!
//! ## 为什么用 `&'static` 引用而不是内置 `Signal`？
//! embassy 的 `Signal` 是 `Sync + !Unpin`；放进 `Notifier` 里再 spawn 到 task 会遇到
//! 生命周期难题。让用户在自己 crate 里 `static SIG: Signal<...> = Signal::new();`
//! 是最省事、最贴 embassy 风格的方案。

use core::sync::atomic::AtomicU8;

use crate::keyring::Keyring;
use crate::link::CommLink;
use crate::peer_registry::PeerRegistry;
use crate::receiver::{CommandHandler, CommandSource, FrameHandler};
use crate::replay::ReplayGuard;
use crate::selector::Selector;

use super::Notifier;
use super::signals::{CommandOutSignal, FrameSignal, ResponseSignal};

/// Notifier 的简化 builder
///
/// 所有必填字段以 `Option` 形式存储，`build()` 内 `expect` 校验。
#[must_use]
pub struct NotifierBuilder<L> {
  pub(super) link: Option<L>,
  pub(super) keyring: Option<&'static Keyring>,
  pub(super) peers: Option<&'static PeerRegistry>,
  pub(super) replay: Option<&'static ReplayGuard>,
  pub(super) selector: Option<&'static Selector>,
  pub(super) frame_signal: Option<&'static FrameSignal>,
  pub(super) command_signal: Option<&'static CommandOutSignal>,
  pub(super) response_signal: Option<&'static ResponseSignal>,
  /// 可选：开启"双身份"能力后，Notifier 会在 run_receive_loop 里同时处理 Command 帧
  pub(super) handler_config: Option<CommandHandlerConfig>,
}

/// "双身份" Notifier 启用 command handler 时需要的参数集
///
/// # 使用场景
/// - 手柄设备：既发 Frame（Notifier 本来就有），又需要处理下行 Command（LedBlink /
///   SetSensitivity / QueryReceivers 等）——就把本配置塞给 [`Notifier`]
/// - 纯 notifier 设备（只发不收命令）：不设置本配置即可，默认行为只处理 Response 帧
#[derive(Clone, Copy)]
pub struct CommandHandlerConfig {
  /// 命令处理闭包指针（签名见 [`CommandHandler`]）
  pub handler: CommandHandler,
  /// 本机 role tag（回 AnnounceReply 时用）
  pub role_tag: [u8; 3],
  /// 本机 MAC（回 AnnounceReply 时用，判断 AssignId 目标时也用）
  pub my_mac: [u8; 6],
  /// 本机 receiver_id 存储（当前 Notifier 既发又收时也会接受 AssignId）
  pub my_id: &'static AtomicU8,
  /// 命令来源标识（BLE / ESP-NOW / Local）
  pub src: CommandSource,
  /// 可选：Frame handler（双身份场景下同时开启 Frame 消费，例如 host 监听工具）
  ///
  /// - `None`：入站 Frame 帧静默丢弃（notifier 默认行为，避免处理自发广播回环）
  /// - `Some(fn)`：会先做 `dest_mask` 过滤，命中本机 `my_id` 时才回调
  pub frame_handler: Option<FrameHandler>,
}

impl<L> Default for NotifierBuilder<L> {
  fn default() -> Self {
    Self::new()
  }
}

impl<L> NotifierBuilder<L> {
  /// 创建一个"什么都没设置"的初始 builder
  pub const fn new() -> Self {
    Self {
      link: None,
      keyring: None,
      peers: None,
      replay: None,
      selector: None,
      frame_signal: None,
      command_signal: None,
      response_signal: None,
      handler_config: None,
    }
  }
}

impl<L: CommLink> NotifierBuilder<L> {
  /// 设置物理链路（必填）
  pub fn link(mut self, link: L) -> Self {
    self.link = Some(link);
    self
  }

  /// 设置 keyring（必填）
  pub fn keyring(mut self, keyring: &'static Keyring) -> Self {
    self.keyring = Some(keyring);
    self
  }

  /// 设置 peer registry（必填）
  pub fn peers(mut self, peers: &'static PeerRegistry) -> Self {
    self.peers = Some(peers);
    self
  }

  /// 设置 replay guard（必填）
  pub fn replay(mut self, replay: &'static ReplayGuard) -> Self {
    self.replay = Some(replay);
    self
  }

  /// 设置 receiver 选择器（必填）
  pub fn selector(mut self, selector: &'static Selector) -> Self {
    self.selector = Some(selector);
    self
  }

  /// 设置 frame 出站 Signal（必填）
  pub fn frame_signal(mut self, sig: &'static FrameSignal) -> Self {
    self.frame_signal = Some(sig);
    self
  }

  /// 设置 command 出站 Signal（必填）
  pub fn command_signal(mut self, sig: &'static CommandOutSignal) -> Self {
    self.command_signal = Some(sig);
    self
  }

  /// 设置 response 入站 Signal（必填）
  pub fn response_signal(mut self, sig: &'static ResponseSignal) -> Self {
    self.response_signal = Some(sig);
    self
  }

  /// **可选**：启用"双身份"能力——同一 CommLink 上同时处理 Command 帧
  ///
  /// # 使用场景
  /// 为开启这个能力，[`super::run_receive_loop`] 除了会把 Response 帧送到 peers upsert /
  /// AssignId 的现有逻辑，还会把 Command 帧——包含内置的 Announce / AssignId 与
  /// 用户业务命令——派发给你提供的 handler。
  ///
  /// # ⚠️ 自帧回环约定
  /// 双身份 `Notifier` 会**同时收发** Command / Announce。若底层 [`CommLink`] 存在
  /// 自回环（`recv` 会交回本机 `send` 出去的帧），会触发自发现 / 自执行问题。
  /// 详见 [`CommLink`](crate::CommLink) 文档的"自帧回环"章节——实现方需保证
  /// 不回环本机帧（ESP-NOW 默认满足）。
  ///
  /// # 参数
  /// - `handler`：用户业务命令处理函数
  /// - `role_tag`：本机角色（3 字节 ASCII，回 AnnounceReply 时携带）
  /// - `my_mac`：本机 MAC
  /// - `my_id`：接收 AssignId 后写入的 `AtomicU8`
  /// - `src`：命令来源标识（需要同时接 BLE 和 ESP-NOW 时，请分别搭 2 套 Notifier）
  pub fn with_command_handler(
    mut self,
    handler: CommandHandler,
    role_tag: [u8; 3],
    my_mac: [u8; 6],
    my_id: &'static AtomicU8,
    src: CommandSource,
  ) -> Self {
    self.handler_config = Some(CommandHandlerConfig {
      handler,
      role_tag,
      my_mac,
      my_id,
      src,
      frame_handler: None,
    });
    self
  }

  /// **可选**：在双身份场景下额外订阅 Frame
  ///
  /// # 前置条件
  /// 必须先调用 [`Self::with_command_handler`] 启用双身份；否则本方法会 panic
  /// （frame_handler 不可能独立存在、不依附一个 command handler）。
  ///
  /// # 语义
  /// 开启后 [`super::run_receive_loop`] 会将入站 `Frame`（命中 dest_mask 后）
  /// 交给本闭包；适用于 host 监听工具 / 调试探针等少见场景。
  ///
  /// # Panics
  /// `with_command_handler` 未先调用时。
  pub fn with_frame_handler(mut self, frame_handler: FrameHandler) -> Self {
    let cfg = self
      .handler_config
      .as_mut()
      .expect("NotifierBuilder: call `with_command_handler` before `with_frame_handler`");
    cfg.frame_handler = Some(frame_handler);
    self
  }

  /// 构造 [`Notifier`]
  ///
  /// # Panics
  /// 任一必填字段未设置时 panic —— 因为这属于**程序员错误**（typo / 忘配置），
  /// 生产环境不允许发生。请在开发阶段就跑一遍 `notifier::builder()...build()`
  /// 触发 panic 补齐字段。
  pub fn build(self) -> Notifier<L> {
    Notifier {
      link: self.link.expect("Notifier: `link` is required"),
      keyring: self.keyring.expect("Notifier: `keyring` is required"),
      peers: self.peers.expect("Notifier: `peers` is required"),
      replay: self.replay.expect("Notifier: `replay` is required"),
      selector: self.selector.expect("Notifier: `selector` is required"),
      frame_signal: self
        .frame_signal
        .expect("Notifier: `frame_signal` is required"),
      command_signal: self
        .command_signal
        .expect("Notifier: `command_signal` is required"),
      response_signal: self
        .response_signal
        .expect("Notifier: `response_signal` is required"),
      handler_config: self.handler_config,
    }
  }
}
