//! # ESP-NOW 双向传输
//!
//! 使用 Wi-Fi 硬件的 ESP-NOW 私有协议，把 21 字节协议帧广播到空中，
//! 同时接收 Host 下发的 20 字节 Command（v3 含 HMAC + 抗重放 seq），
//! 回复 20 字节 CommandResponse。
//! **任意 ESP32 设备**（ESP32 / C3 / S3 / C6...）都能通过标准 ESP-NOW
//! 接收 API 收到，无需事先配对、无需 SSID/密码、无需路由器。
//!
//! ## 特性
//! - **广播地址** `FF:FF:FF:FF:FF:FF` —— 一个发多个收，零配置
//! - **低延迟**：典型 <5 ms（对比 BLE HID ≈15..30 ms）
//! - **无连接**：不需要配对/绑定，接收端上电即收
//! - **三种消息类型混跑**：Frame (21B) / Command (12B) / Response (16B)；magic 不冲突
//!
//! ## 架构
//! ```text
//!                        Signal<Frame>
//!  ┌──────────────┐   overwrite-on-write   ┌────────────────────────┐
//!  │  main loop   │─────────────────────► │ esp_now_broadcast_task  │
//!  │  transport   │                        │  ├─ select(frame,       │
//!  │  .send()     │                        │  │        response)     │
//!  └──────────────┘                        │  ├─ encode              │
//!                                          │  └─ sender.send_async   │
//!                        Signal<Resp>       │                         │
//!  ┌──────────────┐                        │                         │
//!  │ command      │─────────────────────► │                         │
//!  │ handler      │                        └────────────────────────┘
//!  └──────────────┘
//!
//!                        raw bytes
//!  ┌──────────────┐   ◄─────────────────── ┌────────────────────────┐
//!  │ dispatch_    │                        │ esp_now_receive_task    │
//!  │ command()    │                        │  ├─ receive_async()     │
//!  │              │                        │  └─ 按 magic 分派       │
//!  └──────────────┘                        │      Command / Response │
//!                                          └────────────────────────┘
//! ```
//!
//! ## 频道注意事项
//! ESP-NOW 使用 Wi-Fi 频道；默认走站点模式的当前频道，一般为 channel 1。
//! 发送/接收两端必须同频道，否则收不到。

use core::convert::Infallible;
use defmt::{info, warn};
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_radio::esp_now::{BROADCAST_ADDRESS, EspNowReceiver, EspNowSender};

use crate::config::auth::NONCE_BROADCAST_INTERVAL_MS;
use crate::protocol::{
  COMMAND_LEN, COMMAND_MAGIC, CommandResponse, Frame, encode_frame, encode_response, session_nonce,
};
use crate::transport::Transport;
use crate::transport::control::{CommandSource, dispatch_command};
use crate::ui::set_esp_now_ready;

/// ESP-NOW 帧共享通道（Signal = "最后写入者赢"）
///
/// 主循环把 [`Frame`] 写进来，后台任务在自己的节奏里取出、编码、广播。
/// 连续两次 `send` 之间未消费时，第二次会覆盖第一次 —— 手柄场景**只关心最新状态**。
pub type EspNowFrameSignal = Signal<CriticalSectionRawMutex, Frame>;

/// ESP-NOW 响应共享通道（Signal = "最后写入者赢"）
///
/// Command handler 把要回给 Host 的 [`CommandResponse`] 写进来；
/// 广播任务在下一次 wake 时序列化并广播。
///
/// **为什么用覆盖式？** 高频命令场景下如果 Response 堆积会造成流量放大，
/// 覆盖式保证每次最多发一条最新 Response —— 心跳/连接性场景足够。
pub type EspNowResponseSignal = Signal<CriticalSectionRawMutex, CommandResponse>;

/// 全局 Response 通道（handler → broadcast task）
///
/// Command handler 通过 [`signal_response`] 写入；[`esp_now_broadcast_task`]
/// 在 select 里读取并出站。
pub static RESPONSE_SIGNAL: EspNowResponseSignal = Signal::new();

/// 便捷入口：把 [`CommandResponse`] 交给广播任务出站
///
/// # M-3 观测（Ack 覆盖）
/// 若 [`RESPONSE_SIGNAL`] 已有一份等待被 tx task 取走的 Response，本次
/// `signal()` 会**静默覆盖**它 —— 我们通过 [`crate::metrics::record_response_overwrite`]
/// 埋点计数，便于 dashboard 观察实际发生频率，评估是否需要升级到 `Channel<N>`。
pub fn signal_response(resp: CommandResponse) {
  if RESPONSE_SIGNAL.signaled() {
    crate::metrics::record_response_overwrite();
  }
  RESPONSE_SIGNAL.signal(resp);
}

// ============================================================
// Transport 实现
// ============================================================

/// ESP-NOW 广播 Transport（handle 侧）
///
/// 只持有对 `'static` Signal 的引用，`send()` 是**同步、非阻塞**的。
///
/// # 生命周期
/// `EspNowSender<'static>` 由 [`esp_now_broadcast_task`] 独立持有；本结构体
/// **不接触硬件**，只操作 Signal —— 与硬件的解耦让主循环完全不用关心 Wi-Fi 状态。
pub struct EspNowTransport {
  signal: &'static EspNowFrameSignal,
}

impl EspNowTransport {
  /// 构造 handle。真正的 ESP-NOW 发送由 [`esp_now_broadcast_task`] 在后台运行。
  pub const fn new(signal: &'static EspNowFrameSignal) -> Self {
    Self { signal }
  }
}

impl Transport for EspNowTransport {
  type Error = Infallible;

  fn send(&mut self, frame: &Frame) -> Result<(), Self::Error> {
    // Frame 是 Copy，值语义写入 Signal；开销 21 字节，忽略不计
    self.signal.signal(*frame);
    Ok(())
  }
}

// ============================================================
// 后台任务：从两个 Signal 取消息并 ESP-NOW 广播
// ============================================================

/// ESP-NOW 广播后台任务
///
/// # 双 signal select
/// - `frame_signal`：主循环推手柄状态帧（21 字节，30 Hz）
/// - [`RESPONSE_SIGNAL`]：命令 handler 推响应帧（20 字节，事件驱动）
///
/// `select` 优先返回**先就绪**的分支；两条链互不阻塞。
#[embassy_executor::task]
pub async fn esp_now_broadcast_task(
  mut sender: EspNowSender<'static>,
  frame_signal: &'static EspNowFrameSignal,
) -> ! {
  info!("[ESP-NOW] Broadcast task started (target = FF:FF:FF:FF:FF:FF)");
  set_esp_now_ready(true);

  loop {
    // 二路 select：Frame 或 Response
    match select(frame_signal.wait(), RESPONSE_SIGNAL.wait()).await {
      Either::First(frame) => {
        let bytes = encode_frame(&frame);
        if sender.send_async(&BROADCAST_ADDRESS, &bytes).await.is_err() {
          warn!("[ESP-NOW] send frame failed, dropping");
        }
      }
      Either::Second(resp) => {
        let bytes = encode_response(&resp);
        if sender.send_async(&BROADCAST_ADDRESS, &bytes).await.is_err() {
          warn!("[ESP-NOW] send response failed, dropping");
        } else {
          info!(
            "[ESP-NOW] response sent: req_seq={} kind=0x{:02x}",
            resp.req_seq,
            resp.body.kind() as u8
          );
        }
      }
    }
  }
}

// ============================================================
// 后台任务：ESP-NOW 接收 —— Command / Response 反向通道
// ============================================================

/// ESP-NOW 接收后台任务
///
/// # 分派逻辑
/// 由于 v3 起 Command 和 Response 都是 20 字节（长度撞车），需要用 **magic 前缀**
/// 区分：
///
/// | 长度 | 前 2 字节 magic | 处理路径                                        |
/// |------|-----------------|-------------------------------------------------|
/// | 21   | (任意)          | 自己发的 Frame 广播回环 —— 静默忽略             |
/// | 20   | `0xCB01`        | Command 帧（v3）→ [`dispatch_command`]          |
/// | 20   | `0xCB02`        | Response 帧 —— 通常是自己回环，静默忽略         |
/// | 20   | 其它            | 空气里其它 ESP-NOW 设备的 20 字节帧 —— 忽略      |
/// | 其它 | (任意)          | 空气里的杂帧 —— 静默忽略                        |
///
/// # 为什么按长度 + magic 双重过滤？
/// 广播地址 `FF:FF:FF:FF:FF:FF` 会让**自己**也收到自己发出去的帧
/// （硬件层面无法屏蔽）；我们不希望自己 Ack 自己或递归处理。
/// **长度**廉价过滤大部分噪声；**magic** 才能精确区分方向。
#[embassy_executor::task]
pub async fn esp_now_receive_task(mut receiver: EspNowReceiver<'static>) -> ! {
  info!("[ESP-NOW] Receive task started (listening for commands)");
  loop {
    let pkt = receiver.receive_async().await;
    let data = pkt.data();

    // 只关心 COMMAND_LEN 长度的帧；其它长度（含 FRAME_LEN 自发回环）直接丢
    if data.len() != COMMAND_LEN {
      continue;
    }
    // 用 magic 前缀区分 Command / Response（v3 起同长度）
    let magic = u16::from_le_bytes([data[0], data[1]]);
    if magic == COMMAND_MAGIC {
      // 交给 dispatcher（内部做 magic/CRC/HMAC/anti-replay 校验）
      dispatch_command(CommandSource::EspNow, data);
    }
    // magic == RESPONSE_MAGIC 或其它值：静默忽略
  }
}

// ============================================================
// K3: NonceHello 广播任务
// ============================================================

/// 定期广播 [`ResponseKind::NonceHello`] 帧（K3 选项）
///
/// 手柄每 [`NONCE_BROADCAST_INTERVAL_MS`] 通过 ESP-NOW 广播一次当前 session
/// nonce，让 Host 侧无需握手也能被动接收：
///
/// - Host 首次上线：等最多 5 秒收到 NonceHello → 记录 nonce → 开始发命令
/// - 手柄重启：新 nonce → Host 端 HMAC 会拒绝，5 秒内感知到并同步
///
/// # 为什么复用 [`RESPONSE_SIGNAL`] 而不是自己发？
/// - 保持"所有 ESP-NOW 出站流量都走同一个 broadcast task"的架构不变
/// - `Signal` 是覆盖式，若前一条 NonceHello 还没发出去就被新的 NonceHello 覆盖
///   —— 无副作用（nonce 相同）
/// - 若刚好有 Ack 在等发送，也不会导致 NonceHello 阻塞 Ack；Signal 天然让位
///
/// # 与 Response 广播的耦合
/// 会与命令回执共享 [`RESPONSE_SIGNAL`]。极端情况下（Host 高频发命令导致
/// Ack 堆积），最新的 NonceHello 可能被 Ack 覆盖。**这没关系**：下一个 5 秒
/// 周期还会再广播一次；Host 会在下一轮拿到 nonce。
#[embassy_executor::task]
pub async fn nonce_broadcast_task() -> ! {
  info!(
    "[ESP-NOW] Nonce broadcast task started (interval = {} ms)",
    NONCE_BROADCAST_INTERVAL_MS
  );
  // 首次立即发一次（让 Host 尽快拿到 nonce）
  broadcast_current_nonce();
  loop {
    Timer::after(Duration::from_millis(NONCE_BROADCAST_INTERVAL_MS)).await;
    broadcast_current_nonce();
  }
}

/// 读取当前 session nonce 并把 NonceHello 塞入 [`RESPONSE_SIGNAL`]
///
/// 拆分成独立函数便于单元测试；实际生产环境只会由 [`nonce_broadcast_task`] 调用。
fn broadcast_current_nonce() {
  let nonce = session_nonce();
  let resp = CommandResponse::nonce_hello(nonce);
  info!("[ESP-NOW] broadcast NonceHello: nonce=0x{:08x}", nonce);
  signal_response(resp);
}
