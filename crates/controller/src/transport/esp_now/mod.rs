//! # ESP-NOW 双向传输（**直接使用 [`comm`] 类型，无 re-export / 无 wrapper**）
//!
//! ## 分工
//! 本模块**只做三件事**：
//! 1. 提供 [`EspNowTransport`]：主循环 `Transport::send(&frame)` 的 ESP-NOW 实现
//!    （内部经门面 [`comm::Notifier::send_frame`] 把 `Frame` 塞进 [`FRAME_SIG`]）
//! 2. 提供 [`init_notifier`]：把 controller 侧的 3 个 static signal +
//!    [`crate::SESSION_KEYRING`] + [`crate::REGISTRY`] +
//!    [`crate::transport::control::REPLAY`] + 双身份 command handler 收拢成一个
//!    `&'static` [`comm::Notifier`] 门面（不设 selector —— controller 自己在 UI 层
//!    算好 `dest_mask` 直接写进 `Frame`）。
//! 3. 提供 [`esp_now_notifier_broadcast_task`] + [`esp_now_notifier_recv_task`]：
//!    embassy `#[task]` 宏不能吃泛型 async fn，因此在这里包一层，各吃一个 link 端
//!    调门面自带的 [`comm::Notifier::run_broadcast_loop`] /
//!    [`comm::Notifier::run_receive_loop`]。
//!
//! **业务代码要发 Response / Command / Announce**：Announce 直接用
//! [`comm::Notifier::discover`]；Response 走 [`comm::notifier::signals::enqueue_response`]
//! 塞进 [`RESP_SIG`]（丢弃计入 [`comm::metrics`]）。

pub mod link;

use core::convert::Infallible;
use core::sync::atomic::AtomicU8;
use defmt::info;
use embassy_sync::signal::Signal;
use static_cell::StaticCell;

use comm::Notifier;
use comm::notifier::signals::{CommandOutChannel, FrameSignal, ResponseChannel};

use crate::protocol::Frame;
use crate::transport::Transport;
use crate::ui::set_esp_now_ready;

pub use link::{EspNowRecvLink, EspNowSendLink};

// ============================================================
// 三路出站通道：comm 的 `run_broadcast_loop` / `run_receive_loop` 直接消费
//   - Frame：覆盖式 `Signal`（只关心最新手柄状态）
//   - Command / Response：有界 `Channel`（逐条排队，满则丢弃并计入 comm::metrics）
// ============================================================

/// Frame 出站 Signal（主循环 `EspNowTransport::send` → broadcast_loop）
pub static FRAME_SIG: FrameSignal = Signal::new();

/// Command 出站有界队列（Announce / AssignId / SetSensitivity 下发 → broadcast_loop）
///
/// # M-3 丢弃观测
/// 从覆盖式 `Signal` 升级为深度 [`OUTBOUND_QUEUE_DEPTH`](comm::notifier::signals::OUTBOUND_QUEUE_DEPTH)
/// 的有界队列：命令逐条排队出站，不再互相覆盖。生产者应走 comm 的入队助手
/// [`comm::notifier::signals::enqueue_command`]，队列满时的丢弃由
/// [`comm::metrics::dropped_commands`] 集中计数。
pub static CMD_OUT_SIG: CommandOutChannel = CommandOutChannel::new();

/// Response 出站有界队列（Command handler 回 Ack / NonceHello 广播 → broadcast_loop）
pub static RESP_SIG: ResponseChannel = ResponseChannel::new();

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
/// `send()` 是**同步、非阻塞**的：通过门面 [`comm::Notifier::send_frame`] 把 `Frame`
/// 塞进 [`FRAME_SIG`]，由 [`esp_now_notifier_broadcast_task`] 在自己节奏里出站。
///
/// # 为什么走门面而非直接 `FRAME_SIG.signal()`
/// 让"向出站 Frame 通道写"这件事**统一收口到门面 API**：`send_frame` 之后由
/// broadcast loop 依据 `dest_mask` 自动决策广播 / 单播升级（见
/// [`comm::Notifier::send_frame`]）。持有 `&'static Notifier` 引用即可，零额外开销。
pub struct EspNowTransport {
  notifier: &'static Notifier,
}

impl EspNowTransport {
  /// 构造 handle，绑定门面单例（见 [`init_notifier`]）
  #[must_use]
  pub const fn new(notifier: &'static Notifier) -> Self {
    Self { notifier }
  }
}

impl Transport for EspNowTransport {
  type Error = Infallible;

  fn send(&mut self, frame: &Frame) -> Result<(), Self::Error> {
    self.notifier.send_frame(frame);
    Ok(())
  }
}

// ============================================================
// Notifier 门面（双身份）
// ============================================================

/// 门面单例：[`init_notifier`] 里 `builder()...build()` 后 `init` 进来，
/// 两个后台 task 各持一份 `&'static` 共享借用，主循环也用它 `discover()`。
static NOTIFIER: StaticCell<Notifier> = StaticCell::new();

/// 组装 controller 侧的**双身份** Notifier 门面（link 无关）
///
/// 收拢 [`crate::SESSION_KEYRING`] / [`crate::REGISTRY`] /
/// [`crate::transport::control::REPLAY`] + 三路 signal + command handler。
///
/// # `own_mac`
/// AnnounceReply / AssignId 目标判定要用本机 MAC，只有拿到后才能 build，故运行时构造。
///
/// # selector
/// 不设置：controller 在自己的 UI 选择器里算好 `dest_mask` 直接写进 `Frame`，
/// 不经 comm 的 `Selector`（现已是可选字段）。
pub fn init_notifier(own_mac: [u8; 6]) -> &'static Notifier {
  NOTIFIER.init(
    Notifier::builder()
      .keyring(&crate::SESSION_KEYRING)
      .peers(&crate::REGISTRY)
      .replay(&crate::transport::control::REPLAY)
      .frame_signal(&FRAME_SIG)
      .command_signal(&CMD_OUT_SIG)
      .response_signal(&RESP_SIG)
      .with_command_handler(
        crate::transport::control::dispatch_command_from_esp_now,
        *b"joy",
        own_mac,
        &MY_ID,
        comm::CommandSource::EspNow,
      )
      .build(),
  )
}

// ============================================================
// embassy task 包装（宏不能吃泛型 async fn）
// ============================================================

/// 广播 loop —— 三路 select 由 [`comm::Notifier::run_broadcast_loop`] 完成
///
/// 门面持有 `Some(&REGISTRY)`，启用 Frame 自动寻址：单目标帧（dest_mask 恰好选中
/// 一台且其 MAC 已知）会自动升级为单播，其余仍广播。
#[embassy_executor::task]
pub async fn esp_now_notifier_broadcast_task(
  notifier: &'static Notifier,
  link: EspNowSendLink,
) -> ! {
  info!("[ESP-NOW] Notifier broadcast task started (target = FF:FF:FF:FF:FF:FF)");
  set_esp_now_ready(true);
  notifier.run_broadcast_loop(link).await
}

/// 接收 loop —— 解码 / 抗重放 / AnnounceReply 自动 upsert / 自动 AssignId /
/// Command 派发到 [`crate::transport::control::dispatch_command_from_esp_now`] 全部由
/// [`comm::Notifier::run_receive_loop`] 完成。
#[embassy_executor::task]
pub async fn esp_now_notifier_recv_task(notifier: &'static Notifier, link: EspNowRecvLink) -> ! {
  info!("[ESP-NOW] Notifier recv task started (listening for AnnounceReply + Command)");
  notifier.run_receive_loop(link).await
}
