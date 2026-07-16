//! # ESP-NOW 双向传输（**直接使用 [`comm`] 类型，无 re-export / 无 wrapper**）
//!
//! ## 分工
//! 本模块**只做两件事**：
//! 1. 提供 [`EspNowTransport`]：主循环 `Transport::send(&frame)` 的 ESP-NOW 实现
//!    （内部只是把 `Frame` 塞进 [`FRAME_SIG`]）
//! 2. 提供 [`esp_now_notifier_broadcast_task`] + [`esp_now_notifier_recv_task`]：
//!    embassy `#[task]` 宏不能吃泛型 async fn，因此在这里包一层，把
//!    [`comm::notifier::run_broadcast_loop`] / [`comm::notifier::run_receive_loop`]
//!    与 controller 侧的 3 个 static signal + [`crate::SESSION_KEYRING`] +
//!    [`crate::REGISTRY`] + [`crate::transport::control::REPLAY`] 绑起来
//!
//! **业务代码要发 Response / Command / Announce**：直接 `.signal(...)` 到 [`RESP_SIG`] /
//! [`CMD_OUT_SIG`]，或用 [`crate::SESSION_KEYRING`] 生成 seq 后 [`encode_command`]。
//! 不再有 `signal_response` / `broadcast_announce` 这类 wrapper。

pub mod link;

use core::convert::Infallible;
use core::sync::atomic::AtomicU8;
use defmt::info;
use embassy_sync::signal::Signal;

use comm::notifier::signals::{CommandOutSignal, FrameSignal, ResponseSignal};

use crate::protocol::Frame;
use crate::transport::Transport;
use crate::ui::set_esp_now_ready;

pub use link::{EspNowRecvLink, EspNowSendLink};

// ============================================================
// 三路 static Signals：comm 的 `run_broadcast_loop` / `run_receive_loop` 直接消费
// ============================================================

/// Frame 出站 Signal（主循环 `EspNowTransport::send` → broadcast_loop）
pub static FRAME_SIG: FrameSignal = Signal::new();

/// Command 出站 Signal（Announce / AssignId / SetSensitivity 下发 → broadcast_loop）
///
/// # M-3 覆盖观测
/// 此 signal 高频命令场景下可能被覆盖，业务代码在写入前若关心覆盖事件，
/// 应先检查 `signaled()` 并调用 [`crate::metrics::record_response_overwrite`]
/// —— 见 [`crate::transport::control::broadcast_response`] 的实现。
pub static CMD_OUT_SIG: CommandOutSignal = Signal::new();

/// Response 出站 Signal（Command handler 回 Ack / NonceHello 广播 → broadcast_loop）
pub static RESP_SIG: ResponseSignal = Signal::new();

/// 手柄本机作为 Notifier 双身份角色时的占位 `receiver_id`。
///
/// Notifier（Coordinator）不会收到发给自己的 `AssignId`；此字段仅供
/// `comm::CommandHandlerConfig::my_id` 填参数用。值恒为 `UNASSIGNED_ID`。
pub static MY_ID: AtomicU8 = AtomicU8::new(comm::receiver::UNASSIGNED_ID);

// ============================================================
// Transport 实现（主循环 send(&frame) 入口）
// ============================================================

/// ESP-NOW 广播 Transport
///
/// `send()` 是**同步、非阻塞**的：只把 `Frame` 塞进 [`FRAME_SIG`]，
/// 由 [`esp_now_notifier_broadcast_task`] 在自己节奏里出站。
///
/// # 无字段
/// 直接引用模块内 static [`FRAME_SIG`]，无需在构造时传引用。
#[derive(Default)]
pub struct EspNowTransport;

impl EspNowTransport {
  /// 构造 handle
  #[must_use]
  pub const fn new() -> Self {
    Self
  }
}

impl Transport for EspNowTransport {
  type Error = Infallible;

  fn send(&mut self, frame: &Frame) -> Result<(), Self::Error> {
    FRAME_SIG.signal(*frame);
    Ok(())
  }
}

// ============================================================
// embassy task 包装（宏不能吃泛型 async fn）
// ============================================================

/// 广播 loop —— 三路 select 由 [`comm::notifier::run_broadcast_loop`] 完成
#[embassy_executor::task]
pub async fn esp_now_notifier_broadcast_task(link: EspNowSendLink) -> ! {
  info!("[ESP-NOW] Notifier broadcast task started (target = FF:FF:FF:FF:FF:FF)");
  set_esp_now_ready(true);
  comm::notifier::run_broadcast_loop(link, &FRAME_SIG, &CMD_OUT_SIG, &RESP_SIG).await
}

/// 接收 loop —— 解码 / 抗重放 / AnnounceReply 自动 upsert / 自动 AssignId /
/// Command 派发到 [`crate::transport::control::dispatch_command_from_esp_now`] 全部由
/// [`comm::notifier::run_receive_loop`] 完成。
#[embassy_executor::task]
pub async fn esp_now_notifier_recv_task(link: EspNowRecvLink, own_mac: [u8; 6]) -> ! {
  info!("[ESP-NOW] Notifier recv task started (listening for AnnounceReply + Command)");
  comm::notifier::run_receive_loop(
    link,
    &crate::REGISTRY,
    &CMD_OUT_SIG,
    &crate::SESSION_KEYRING,
    &crate::transport::control::REPLAY,
    &RESP_SIG,
    Some(comm::notifier::CommandHandlerConfig {
      role_tag: *b"joy",
      my_mac: own_mac,
      my_id: &MY_ID,
      handler: crate::transport::control::dispatch_command_from_esp_now,
      src: comm::CommandSource::EspNow,
      frame_handler: None,
    }),
    None,
  )
  .await
}
