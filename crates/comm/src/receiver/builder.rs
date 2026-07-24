//! # `ReceiverBuilder`
//!
//! 与 [`NotifierBuilder`](crate::notifier::NotifierBuilder) 采用同款"简化 typestate"
//! 策略：所有字段 `Option` 存储，`build()` 里 `expect` 检查。

use core::sync::atomic::AtomicU8;

use crate::keyring::Keyring;
use crate::notifier::signals::{CommandOutChannel, FrameSignal, ResponseChannel};
use crate::replay::ReplayGuard;

use super::{CommandHandler, CommandSource, FrameHandler, Receiver};

/// receiver 端 builder
///
/// 不含 `link`：门面 link 无关，link 在跑 loop 时按 send / recv 端传入。
#[must_use]
pub struct ReceiverBuilder {
  keyring: Option<&'static Keyring>,
  replay: Option<&'static ReplayGuard>,
  response_signal: Option<&'static ResponseChannel>,
  frame_signal: Option<&'static FrameSignal>,
  command_signal: Option<&'static CommandOutChannel>,
  role_tag: [u8; 3],
  my_mac: [u8; 6],
  my_id: Option<&'static AtomicU8>,
  handler: Option<CommandHandler>,
  src: CommandSource,
  frame_handler: Option<FrameHandler>,
}

impl ReceiverBuilder {
  /// 空 builder
  //
  // 无需 `#[must_use]`：返回类型 `ReceiverBuilder` 结构体本身已标 `#[must_use]`，
  // 函数级注解会触发 clippy::double_must_use（同一约束标两遍无意义）。
  pub const fn new() -> Self {
    Self {
      keyring: None,
      replay: None,
      response_signal: None,
      frame_signal: None,
      command_signal: None,
      role_tag: [0; 3],
      my_mac: [0; 6],
      my_id: None,
      handler: None,
      src: CommandSource::EspNow,
      frame_handler: None,
    }
  }

  /// 设置 keyring
  pub fn keyring(mut self, keyring: &'static Keyring) -> Self {
    self.keyring = Some(keyring);
    self
  }

  /// 设置 replay guard
  pub fn replay(mut self, replay: &'static ReplayGuard) -> Self {
    self.replay = Some(replay);
    self
  }

  /// 设置回执 Signal
  pub fn response_signal(mut self, sig: &'static ResponseChannel) -> Self {
    self.response_signal = Some(sig);
    self
  }

  /// 设置 Frame 出站 Signal（供 [`Receiver::send_frame`](super::Receiver::send_frame) 使用）
  ///
  /// # 为什么必需
  /// Receiver 端现已支持主动出站（P0+P1），[`run_broadcast_loop`](super::run_broadcast_loop)
  /// 需要固定的三路 signals；Frame Signal 就是其中一路。即使不调 `send_frame`，
  /// 未消费的 signal 也只占一份静态内存，零运行时开销。
  pub fn frame_signal(mut self, sig: &'static FrameSignal) -> Self {
    self.frame_signal = Some(sig);
    self
  }

  /// 设置 Command 出站有界队列（供 `Receiver::send_command` 使用，需
  /// `endpoint-initiated-command` feature；未开启时该队列不被主动写入）
  pub fn command_signal(mut self, sig: &'static CommandOutChannel) -> Self {
    self.command_signal = Some(sig);
    self
  }

  /// 设置 role_tag（3 字节 ASCII）
  pub fn role_tag(mut self, tag: [u8; 3]) -> Self {
    self.role_tag = tag;
    self
  }

  /// 设置本机 MAC
  pub fn mac(mut self, mac: [u8; 6]) -> Self {
    self.my_mac = mac;
    self
  }

  /// 设置 my_id 存储位置（`static AtomicU8`）
  pub fn my_id(mut self, cell: &'static AtomicU8) -> Self {
    self.my_id = Some(cell);
    self
  }

  /// 设置命令处理闭包
  pub fn command_handler(mut self, handler: CommandHandler) -> Self {
    self.handler = Some(handler);
    self
  }

  /// 可选：设置 Frame 处理闭包
  ///
  /// # 语义
  /// - 未调用这个 setter 时：receiver 入站的 `Frame` 帧会被静默丢弃
  ///   （适用于只关心命令、不关心 GamepadState 的设备）。
  /// - 设置后：[`run_receive_loop`](super::run_receive_loop) 会先做 `dest_mask`
  ///   过滤，仅当帧命中本机 `my_id`（或带广播 mask / 本机尚未分配 id）
  ///   时才会将 `Frame` 交给本闭包。
  pub fn frame_handler(mut self, handler: FrameHandler) -> Self {
    self.frame_handler = Some(handler);
    self
  }

  /// 可选：设置入站命令来源标识（默认 [`CommandSource::EspNow`]）
  ///
  /// 透传给 command handler；receiver-only 设备通常保持默认 `EspNow` 即可。
  pub fn src(mut self, src: CommandSource) -> Self {
    self.src = src;
    self
  }

  /// 构造 Receiver
  ///
  /// # Panics
  /// 必填字段缺失时 panic
  ///
  /// # `my_id` 初值
  /// 不主动写入 [`UNASSIGNED_ID`](super::UNASSIGNED_ID)：调用方应在传入的
  /// `static AtomicU8` 上按需初始化（未持久化时使用 `AtomicU8::new(UNASSIGNED_ID)`，
  /// 有持久化 id 时使用 `AtomicU8::new(persisted_id)`）。
  pub fn build(self) -> Receiver {
    Receiver {
      keyring: self.keyring.expect("Receiver: `keyring` is required"),
      replay: self.replay.expect("Receiver: `replay` is required"),
      response_signal: self
        .response_signal
        .expect("Receiver: `response_signal` is required"),
      frame_signal: self
        .frame_signal
        .expect("Receiver: `frame_signal` is required"),
      command_signal: self
        .command_signal
        .expect("Receiver: `command_signal` is required"),
      role_tag: self.role_tag,
      my_mac: self.my_mac,
      my_id: self.my_id.expect("Receiver: `my_id` is required"),
      handler: self
        .handler
        .expect("Receiver: `command_handler` is required"),
      src: self.src,
      frame_handler: self.frame_handler,
    }
  }
}

impl Default for ReceiverBuilder {
  fn default() -> Self {
    Self::new()
  }
}
