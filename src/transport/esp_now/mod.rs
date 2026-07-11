//! # ESP-NOW 双向传输
//!
//! 使用 Wi-Fi 硬件的 ESP-NOW 私有协议，把协议帧广播到空中，
//! 同时接收 Host 下发的 Command（含 HMAC + 抗重放 seq），
//! 回复 CommandResponse。
//! **任意 ESP32 设备**（ESP32 / C3 / S3 / C6...）都能通过标准 ESP-NOW
//! 接收 API 收到，无需事先配对、无需 SSID/密码、无需路由器。
//!
//! ## 特性
//! - **广播地址** `FF:FF:FF:FF:FF:FF` —— 一个发多个收，零配置
//! - **低延迟**：典型 <5 ms（对比 BLE HID ≈15..30 ms）
//! - **无连接**：不需要配对/绑定，接收端上电即收
//! - **三种消息类型混跑**：Frame (25B) / Command (24B) / Response (24B)；magic 不冲突
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
use embassy_futures::select::{Either3, select3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_radio::esp_now::{BROADCAST_ADDRESS, EspNowReceiver, EspNowSender};

use crate::config::auth::NONCE_BROADCAST_INTERVAL_MS;
use crate::peer_registry;
use crate::protocol::{
  COMMAND_LEN, COMMAND_MAGIC, Command, CommandBody, CommandResponse, Frame, KeyId, RESPONSE_LEN,
  RESPONSE_MAGIC, ResponseBody, decode_response, encode_command, encode_frame, encode_response,
  session_nonce,
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

/// ESP-NOW 出站 Command 通道（controller → receiver 方向）
///
/// 携带**已编码好**的 [`COMMAND_LEN`] 字节，避免 Signal 内存放巨大 enum。
/// 广播任务在 select 里读取并出站，目标地址由 payload 决定（当前仅广播）。
pub type EspNowCommandOutSignal = Signal<CriticalSectionRawMutex, [u8; COMMAND_LEN]>;

/// 全局 Response 通道（handler → broadcast task）
///
/// Command handler 通过 [`signal_response`] 写入；[`esp_now_broadcast_task`]
/// 在 select 里读取并出站。
pub static RESPONSE_SIGNAL: EspNowResponseSignal = Signal::new();

/// 全局出站 Command 通道（Announce/AssignId 走这里）
pub static COMMAND_OUT_SIGNAL: EspNowCommandOutSignal = Signal::new();

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

/// 便捷入口：把一条 Announce/AssignId 命令交给广播任务出站
///
/// # 参数
/// - `wire_bytes`：调用方已经完成 [`encode_command`] 的定长字节
///
/// # 覆盖语义
/// 与 Response 通道相同：Signal 是"最后写入者赢"；若前一条 Announce
/// 尚未被 tx task 消费即被覆盖，属于**可容忍**的行为（下一次 Selecting
/// 进入时会重新广播，Announce 本身是幂等发现请求）。
pub fn signal_command_out(wire_bytes: [u8; COMMAND_LEN]) {
  COMMAND_OUT_SIGNAL.signal(wire_bytes);
}

/// 便捷入口：构造 + 编码 + 送出一条 Announce 广播
///
/// 调用方（selector 首次进入 Selecting、UI 手动刷新按钮）用。所有 receiver
/// 收到后应回 [`ResponseBody::AnnounceReply`]。
///
/// # 参数
/// - `seq`：反重放 seq；调用方保证单调递增（每次 Announce 至少 +1）
pub fn broadcast_announce(seq: u32) {
  let cmd = Command::new(seq, CommandBody::Announce);
  signal_command_out(encode_command(&cmd));
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
    // Frame 是 Copy，值语义写入 Signal；开销 25 字节，忽略不计
    self.signal.signal(*frame);
    Ok(())
  }
}

// ============================================================
// 后台任务：从两个 Signal 取消息并 ESP-NOW 广播
// ============================================================

/// ESP-NOW 广播后台任务
///
/// # 三路 signal select
/// - `frame_signal`：主循环推手柄状态帧
/// - [`RESPONSE_SIGNAL`]：命令 handler 推响应帧
/// - [`COMMAND_OUT_SIGNAL`]：Announce/AssignId 出站
///
/// `select` 优先返回**先就绪**的分支；三条链互不阻塞。
#[embassy_executor::task]
pub async fn esp_now_broadcast_task(
  mut sender: EspNowSender<'static>,
  frame_signal: &'static EspNowFrameSignal,
) -> ! {
  info!("[ESP-NOW] Broadcast task started (target = FF:FF:FF:FF:FF:FF)");
  set_esp_now_ready(true);

  loop {
    // 三路 select：Frame / Response / CommandOut
    //
    // 优先级：Response > CommandOut > Frame（Frame 是持续高频流，容易饥饿其它两路）
    match select3(
      frame_signal.wait(),
      RESPONSE_SIGNAL.wait(),
      COMMAND_OUT_SIGNAL.wait(),
    )
    .await
    {
      Either3::First(frame) => {
        let bytes = encode_frame(&frame);
        if sender.send_async(&BROADCAST_ADDRESS, &bytes).await.is_err() {
          warn!("[ESP-NOW] send frame failed, dropping");
        }
      }
      Either3::Second(resp) => {
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
      Either3::Third(cmd_bytes) => {
        if sender
          .send_async(&BROADCAST_ADDRESS, &cmd_bytes)
          .await
          .is_err()
        {
          warn!("[ESP-NOW] send command failed, dropping");
        } else {
          info!("[ESP-NOW] out-command sent ({} B)", COMMAND_LEN);
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
/// 三种可能长度：
///
/// | 长度 | 前 2 字节 magic | 处理路径                                        |
/// |------|-----------------|-------------------------------------------------|
/// | FRAME_LEN  | (任意)    | 自己发的 Frame 广播回环 —— 静默忽略             |
/// | COMMAND_LEN | `0xCB01` | Command 帧 → [`dispatch_command`]              |
/// | RESPONSE_LEN | `0xCB02`| Response 帧 →                                  |
/// |     |               |    - AnnounceReply → upsert peer_registry     |
/// |     |               |    - 其它 → 通常是自己回环，静默忽略           |
/// | 其它 | (任意)          | 空气里的杂帧 —— 静默忽略                        |
#[embassy_executor::task]
pub async fn esp_now_receive_task(mut receiver: EspNowReceiver<'static>) -> ! {
  // 编译期断言：接收任务按长度分派时假设 Command 与 Response 等长，用 magic 区分。
  // 若未来升级让二者长度不同，此处会编译失败，提醒开发者更新分派逻辑。
  const _: () = assert!(
    COMMAND_LEN == RESPONSE_LEN,
    "esp_now_receive_task assumes COMMAND_LEN == RESPONSE_LEN; update dispatch logic if changed"
  );

  info!("[ESP-NOW] Receive task started (listening for commands + AnnounceReply)");
  loop {
    let pkt = receiver.receive_async().await;
    let src_mac = pkt.info.src_address;
    let data = pkt.data();

    // 按长度分派：Command 与 Response 等长（由上方 const assert 保证），用 magic 区分
    match data.len() {
      len if len == COMMAND_LEN => {
        let magic = u16::from_le_bytes([data[0], data[1]]);
        if magic == COMMAND_MAGIC {
          dispatch_command(CommandSource::EspNow, data);
        } else if magic == RESPONSE_MAGIC {
          handle_incoming_response(data, src_mac);
        }
        // 其它 magic：静默忽略
      }
      _ => {
        // 其它长度（含 FRAME_LEN 自发回环、空气里的杂帧）静默忽略
      }
    }
  }
}

/// 处理入站 Response —— 主要是 AnnounceReply，让 peer_registry 学习新 peer
///
/// # 副作用
/// - `AnnounceReply` → [`peer_registry::upsert`] + 若首次入库则回发 AssignId
/// - `Ack` / `Error` / `NonceHello` / `BatterySnapshot`：通常是自身回环
///   （广播地址天然会让自己也收到自己发的），静默忽略
fn handle_incoming_response(bytes: &[u8], src_mac: [u8; 6]) {
  let Ok(resp) = decode_response(bytes) else {
    // 解码失败：可能是空气里的杂帧或伪造帧；静默忽略
    return;
  };
  match resp.body {
    ResponseBody::AnnounceReply {
      mac,
      rssi_dbm,
      role_tag,
    } => {
      // 优先使用 payload 里的 mac（receiver 自报），忽略无线层的 src_mac
      // —— 因为 ESP-NOW 无线层 mac 可能被中继/桥接改写
      let _ = src_mac; // 保留字段用于未来 RSSI 补齐（当前 RxControlInfo API 待接入）
      let outcome = peer_registry::upsert(mac, role_tag, rssi_dbm, embassy_time::Instant::now());
      match outcome {
        peer_registry::UpsertOutcome::Inserted { receiver_id } => {
          info!(
            "[ANNOUNCE] new peer mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} \
             role={:?} rssi={}dBm → assigned id={}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], role_tag, rssi_dbm, receiver_id
          );
          // 首次入库：单播 AssignId 让 receiver 记住自己的逻辑 id
          send_assign_id(mac, receiver_id);
        }
        peer_registry::UpsertOutcome::Updated { receiver_id } => {
          info!(
            "[ANNOUNCE] updated peer id={} rssi={}dBm",
            receiver_id, rssi_dbm
          );
        }
        peer_registry::UpsertOutcome::Full => {
          warn!("[ANNOUNCE] peer registry full; ignoring new peer");
        }
      }
    }
    _ => {
      // 其它 Response kind：通常是自身回环，忽略
    }
  }
}

/// 向指定 mac 发一条 AssignId 命令，让 receiver 记住自己的逻辑 id
///
/// # 目前实现
/// 由于 [`esp_now_broadcast_task`] 使用广播地址（无法单播到任意 mac），
/// AssignId 也走广播；receiver 端根据 payload 里的 mac 对比自身，
/// 不匹配的 receiver 会静默忽略。这样单一广播任务就能同时处理广播 + "单播"。
fn send_assign_id(mac: [u8; 6], receiver_id: u8) {
  // seq 从 assign_id 独立计数器获取；简单起见用 static AtomicU32 递增。
  // 安全性：当前唯一调用者是 `esp_now_receive_task`（单任务），因此
  // `Relaxed` 足够保证单调递增。若未来有多任务调用，需升级为 `AcqRel`。
  use core::sync::atomic::{AtomicU32, Ordering};
  static ASSIGN_SEQ: AtomicU32 = AtomicU32::new(1);
  let seq = ASSIGN_SEQ.fetch_add(1, Ordering::Relaxed);

  let cmd = Command::with_key(
    seq,
    KeyId::DEFAULT,
    CommandBody::AssignId { mac, receiver_id },
  );
  signal_command_out(encode_command(&cmd));
  info!(
    "[ANNOUNCE] sent AssignId seq={} id={} to {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
    seq, receiver_id, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
  );
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
