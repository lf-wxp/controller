//! ESP-NOW → `comm::CommLink` 适配层
//!
//! `esp-radio` 的 [`esp_now.split()`] 返回 `(_, EspNowSender<'static>, EspNowReceiver<'static>)`
//! 两半 —— 用来分别喂给 comm 的 [`run_broadcast_loop`] 和 [`run_receive_loop`] 两个 embassy task。
//!
//! [`CommLink`] 的 `send + recv` 都要 `&mut self`，无法在两个 task 之间共享同一个实现。
//! 因此这里参照 [`comm::loopback::LoopbackSendEnd`] / [`LoopbackRecvEnd`] 的模式，
//! 提供**两个 `CommLink` 实现**：
//!
//! - [`EspNowSendLink`]：只 `send`；`recv` 直接返回错误（永不会被广播 loop 调用）
//! - [`EspNowRecvLink`]：只 `recv`；`send` 直接返回错误（永不会被接收 loop 调用）
//!
//! ## `Packet::src` 字段
//! comm 内部（`crates/comm/src/dispatch.rs` + `notifier/mod.rs`）**不消费** `Packet::src`，
//! 帧派发用的是 `DispatchCtx::src`（在 builder 里配置为 [`CommandSource::EspNow`]）。
//! 所以本模块 `recv` 一律把 `src` 填成 `[0; 6]`，避免额外解析 esp-radio 的 `RxControlInfo`。
//!
//! [`run_broadcast_loop`]: comm::notifier::run_broadcast_loop
//! [`run_receive_loop`]: comm::notifier::run_receive_loop
//! [`CommLink`]: comm::CommLink
//! [`CommandSource::EspNow`]: comm::CommandSource

use comm::{CommLink, Packet};
use defmt::Format;
use esp_radio::esp_now::{EspNowError, EspNowReceiver, EspNowSender};

use crate::peer::BROADCAST;

/// ESP-NOW MTU：最大 250 字节 payload；comm 侧真实帧长（Frame/Command/Response）
/// 均 ≤ 26 字节，此上限足够。
const ESP_NOW_MTU: usize = 250;

// ============================================================
// 错误类型
// ============================================================

/// 发送侧错误
#[derive(Debug, Format)]
pub enum SendError {
  /// 底层 esp-radio 发送失败
  Radio,
}

/// 接收侧错误
#[derive(Debug, Format)]
pub enum RecvError {
  /// 该端只支持发送，不能接收（`send-only` 端被误 `recv`）
  SendOnly,
}

/// 只允许被 `run_receive_loop` 使用；`send` 会返回该错误
#[derive(Debug, Format)]
pub enum SendOnRecvError {
  /// 该端只支持接收，不能发送
  RecvOnly,
}

// ============================================================
// send-only 端
// ============================================================

/// [`CommLink`] 的 send-only 实现，包住 [`EspNowSender`]。
///
/// 由 [`comm::notifier::run_broadcast_loop`] 拥有并驱动。
pub struct EspNowSendLink {
  sender: EspNowSender<'static>,
}

impl EspNowSendLink {
  /// 用 `esp_now.split()` 拆出来的 sender 构造
  #[must_use]
  pub const fn new(sender: EspNowSender<'static>) -> Self {
    Self { sender }
  }
}

impl CommLink for EspNowSendLink {
  const MAX_FRAME_LEN: usize = ESP_NOW_MTU;
  type SendError = SendError;
  type RecvError = RecvError;
  type Addr = [u8; 6];
  const BROADCAST: Self::Addr = BROADCAST;

  async fn send(&mut self, dst: Self::Addr, bytes: &[u8]) -> Result<(), Self::SendError> {
    // esp-radio 0.18 的 `send_async` 直接吞 `&[u8; 6]` + payload；失败即链路不可用
    self
      .sender
      .send_async(&dst, bytes)
      .await
      .map_err(|_e: EspNowError| SendError::Radio)
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    // send-only：给 recv_loop 用会直接返回错误，让 loop 静默进入下一轮（loop 内 `continue`）
    Err(RecvError::SendOnly)
  }
}

// ============================================================
// recv-only 端
// ============================================================

/// [`CommLink`] 的 recv-only 实现，包住 [`EspNowReceiver`]。
///
/// 由 [`comm::notifier::run_receive_loop`] 拥有并驱动。
///
/// # 内部缓冲
/// esp-radio 的 `ReceivedData::data()` 返回的切片生命周期与 `pkt` 绑定；
/// 但 [`CommLink::recv`] 承诺返回**借用 impl 内部**的切片，
/// 因此必须先把字节 copy 到 `self.scratch`，让借用挂到 `self` 上。
pub struct EspNowRecvLink {
  receiver: EspNowReceiver<'static>,
  scratch: [u8; ESP_NOW_MTU],
  scratch_len: usize,
  scratch_src: [u8; 6],
}

impl EspNowRecvLink {
  /// 用 `esp_now.split()` 拆出来的 receiver 构造
  #[must_use]
  pub const fn new(receiver: EspNowReceiver<'static>) -> Self {
    Self {
      receiver,
      scratch: [0; ESP_NOW_MTU],
      scratch_len: 0,
      scratch_src: [0; 6],
    }
  }
}

impl CommLink for EspNowRecvLink {
  const MAX_FRAME_LEN: usize = ESP_NOW_MTU;
  type SendError = SendOnRecvError;
  type RecvError = RecvError;
  type Addr = [u8; 6];
  const BROADCAST: Self::Addr = BROADCAST;

  async fn send(&mut self, _dst: Self::Addr, _bytes: &[u8]) -> Result<(), Self::SendError> {
    // recv-only：给 broadcast_loop 用会直接返回错误，让 loop 忽略下一轮再试
    Err(SendOnRecvError::RecvOnly)
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    let pkt = self.receiver.receive_async().await;
    let data = pkt.data();
    // ESP-NOW 单帧最大 250B，超出直接截断（协议帧远远短于此，实际不会命中）
    let len = data.len().min(ESP_NOW_MTU);
    self.scratch[..len].copy_from_slice(&data[..len]);
    self.scratch_len = len;
    // src 字段 comm 不消费（见模块级 doc），此处保持全 0 即可
    self.scratch_src = [0; 6];
    Ok(Packet {
      src: self.scratch_src,
      data: &self.scratch[..self.scratch_len],
    })
  }
}
