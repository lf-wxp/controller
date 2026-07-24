//! ESP-NOW → [`comm::CommLink`] 适配层（手柄侧）
//!
//! 与 `crates/c6/src/link.rs` 完全同款，只是 esp-radio 版本 / target
//! 不同（controller 是 xtensa）。共同的模式：
//!
//! - [`EspNowSendLink`]：包 `EspNowSender<'static>`，只允许 `send`；`recv` 返回
//!   [`RecvError::SendOnly`]。喂给 [`comm::run_broadcast_loop`] 使用。
//! - [`EspNowRecvLink`]：包 `EspNowReceiver<'static>`，只允许 `recv`；`send` 返回
//!   [`SendOnRecvError::RecvOnly`]。喂给 [`comm::notifier::run_receive_loop`] 使用。
//!
//! 之所以拆两半：`CommLink` trait 的 `send + recv` 都要 `&mut self`，不能被
//! 两个 embassy task 同时借用。参考 [`comm::loopback::LoopbackSendEnd`] 模式。
//!
//! ## 与 c6 的差异
//! - `Packet::src` **填入 `pkt.info.src_address`**（controller 保留了旧 `handle_incoming_response`
//!   曾用过的 src_mac 语义，虽然 comm 内部实际不消费，但语义完整对未来 debug 有价值）
//! - **自帧回环过滤**：[`EspNowRecvLink`] 持有本机 MAC，`recv` 内部丢弃
//!   `src == own_mac` 的帧。这是 [`comm::CommLink`] 文档约定的"self-echo 必须由 link
//!   实现方过滤"的落地点：手柄是**双身份 Notifier**，若底层链路回环了自己广播的
//!   `Announce`，comm 会回 `AnnounceReply` 并把**自己**登记成 peer（自发现回环）。
//!   ESP-NOW 硬件默认不回环，此过滤是低成本的兜底防御。
//! - 错误名保持 controller-local，避免与 c6 端冲突（虽然两个 crate 各自命名空间隔离）

use comm::{CommLink, Packet};
use defmt::Format;
use esp_radio::esp_now::{
  BROADCAST_ADDRESS, EspNowError, EspNowManager, EspNowReceiver, EspNowSender, EspNowWifiInterface,
  PeerInfo,
};

/// ESP-NOW MTU：最大 250 字节 payload；本项目真实帧 Frame/Command/Response ≤ 26B，
/// 该上限只是硬边界保护。
const ESP_NOW_MTU: usize = 250;

// ============================================================
// 错误类型
// ============================================================

/// send-only 端的发送错误
#[derive(Debug, Format)]
pub enum SendError {
  /// 底层 esp-radio 发送失败（可能是 Wi-Fi 未初始化 / 广播队列满）
  Radio,
  /// 单播目标 `add_peer` 失败（多为 ESP-NOW peer 表满，上限约 20）
  PeerFull,
}

/// send-only 端被误 `recv` 时的错误
#[derive(Debug, Format)]
pub enum RecvError {
  /// 该端只支持发送
  SendOnly,
}

/// recv-only 端被误 `send` 时的错误
#[derive(Debug, Format)]
pub enum SendOnRecvError {
  /// 该端只支持接收
  RecvOnly,
}

// ============================================================
// send-only：EspNowSendLink
// ============================================================

/// [`CommLink`] 的 send-only 实现；由 [`comm::run_broadcast_loop`] 拥有。
///
/// # 单播 peer 惰性登记（Phase 1）
/// ESP-NOW 单播要求目标 MAC 先 `add_peer`（广播地址由 esp-radio 初始化时自动登记）。
/// 本 link 持有 [`EspNowManager`] 引用，在首次单播到某未登记 MAC 时惰性 `add_peer`，
/// 从而让 comm 侧的 `CommandDest::Unicast`（当前用于 AssignId）无需感知 peer 表管理。
pub struct EspNowSendLink {
  sender: EspNowSender<'static>,
  manager: &'static EspNowManager<'static>,
}

impl EspNowSendLink {
  /// 用 `esp_now.split()` 拆出的 sender + manager 构造
  #[must_use]
  pub const fn new(
    sender: EspNowSender<'static>,
    manager: &'static EspNowManager<'static>,
  ) -> Self {
    Self { sender, manager }
  }

  /// 确保单播目标已在 peer 表中（幂等）。
  ///
  /// 广播地址由 esp-radio 自动登记，直接跳过。`add_peer` 失败（多为
  /// peer 表满，ESP-NOW 上限约 20）时返回 [`SendError::PeerFull`]，让上层
  /// 决定放弃 / 回退——Phase 1 里由 AnnounceReply 幂等重发兜底。
  fn ensure_peer(&self, dst: &[u8; 6]) -> Result<(), SendError> {
    if *dst == BROADCAST_ADDRESS || self.manager.peer_exists(dst) {
      return Ok(());
    }
    self
      .manager
      .add_peer(PeerInfo {
        interface: EspNowWifiInterface::Station,
        peer_address: *dst,
        lmk: None,
        channel: None,
        encrypt: false,
      })
      .map_err(|_e: EspNowError| SendError::PeerFull)
  }
}

impl CommLink for EspNowSendLink {
  const MAX_FRAME_LEN: usize = ESP_NOW_MTU;
  type SendError = SendError;
  type RecvError = RecvError;
  type Addr = [u8; 6];
  const BROADCAST: Self::Addr = BROADCAST_ADDRESS;

  async fn send(&mut self, dst: Self::Addr, bytes: &[u8]) -> Result<(), Self::SendError> {
    self.ensure_peer(&dst)?;
    self
      .sender
      .send_async(&dst, bytes)
      .await
      .map_err(|_e: EspNowError| SendError::Radio)
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    // send-only：broadcast_loop 不会调用 recv；receiver_loop 若误调则 loop 会
    // 忽略 Err 并 continue（不 panic，不刷屏）
    Err(RecvError::SendOnly)
  }
}

// ============================================================
// recv-only：EspNowRecvLink
// ============================================================

/// [`CommLink`] 的 recv-only 实现；由 [`comm::notifier::run_receive_loop`] 拥有。
///
/// # 内部缓冲
/// `ReceivedData::data()` 返回的切片生命周期与 `pkt` 绑定；而
/// [`CommLink::recv`] 约定返回**借用 impl 内部**的切片，因此把字节先 copy 到
/// `self.scratch`，让借用挂到 `self` 上，这样调用方（`run_receive_loop` 内部）
/// 就能在 `.await` 之间安全持有。
pub struct EspNowRecvLink {
  receiver: EspNowReceiver<'static>,
  scratch: [u8; ESP_NOW_MTU],
  scratch_len: usize,
  scratch_src: [u8; 6],
  /// 本机 MAC —— 用于丢弃自帧回环（见模块文档 & [`comm::CommLink`] self-echo 约定）
  own_mac: [u8; 6],
}

impl EspNowRecvLink {
  /// 用 `esp_now.split()` 拆出的 receiver + 本机 MAC 构造
  ///
  /// `own_mac` 用于在 `recv` 内部过滤掉链路回环的自帧（`src == own_mac`）。
  #[must_use]
  pub const fn new(receiver: EspNowReceiver<'static>, own_mac: [u8; 6]) -> Self {
    Self {
      receiver,
      scratch: [0; ESP_NOW_MTU],
      scratch_len: 0,
      scratch_src: [0; 6],
      own_mac,
    }
  }
}

impl CommLink for EspNowRecvLink {
  const MAX_FRAME_LEN: usize = ESP_NOW_MTU;
  type SendError = SendOnRecvError;
  type RecvError = RecvError;
  type Addr = [u8; 6];
  const BROADCAST: Self::Addr = BROADCAST_ADDRESS;

  async fn send(&mut self, _dst: Self::Addr, _bytes: &[u8]) -> Result<(), Self::SendError> {
    Err(SendOnRecvError::RecvOnly)
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    // 循环直到收到一条**非自帧**：丢弃 src == own_mac 的回环，绝不把它交回给
    // comm（否则双身份 Notifier 会自发现 / 自执行，见模块文档）。ESP-NOW 默认不
    // 回环，此循环在正常部署下永远一次命中。
    loop {
      let pkt = self.receiver.receive_async().await;
      let src = pkt.info.src_address;
      if src == self.own_mac {
        continue;
      }
      let data = pkt.data();
      let len = data.len().min(ESP_NOW_MTU);
      self.scratch[..len].copy_from_slice(&data[..len]);
      self.scratch_len = len;
      self.scratch_src = src;
      return Ok(Packet {
        src: self.scratch_src,
        data: &self.scratch[..self.scratch_len],
      });
    }
  }
}
