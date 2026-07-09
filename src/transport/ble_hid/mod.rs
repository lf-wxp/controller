//! # BLE HID Gamepad + Custom Controller Transport
//!
//! 让本设备被手机 / PC / iPad 直接识别为**标准游戏手柄**的同时，
//! 通过一个自定义 GATT 服务向自研 App / Python 客户端暴露**完整 21 字节协议帧**。
//!
//! ## 架构（生产者-消费者，双消费点）
//! ```text
//!                        Signal<Frame>
//!  ┌──────────────┐   overwrite-on-write   ┌────────────────────────────────┐
//!  │  main loop   │─────────────────────► │ ble_gamepad_task               │
//!  │  transport   │  <=next value wins=>   │  ├─ advertise                   │
//!  │  .send()     │                        │  ├─ accept connection           │
//!  └──────────────┘                        │  ├─ notify(HID input_report)    │
//!                                          │  └─ notify(Custom frame_stream) │
//!                                          └────────────────────────────────┘
//! ```
//!
//! ## 特性
//! - **覆盖式 Signal**：主循环发得快时旧值直接被丢弃 —— 手柄场景**只关心最新状态**
//! - **静默丢弃**：BLE 未连接时 `send()` 返回 Ok(())，不影响主循环
//! - **自动重连**：断线后任务回到广播状态，Host 重新配对即可
//! - **双轨输出**：同一次 send() 既更新 HID（缩放后的 6 字节），又更新
//!   自定义服务（未缩放的完整 21 字节，含 seq/CRC）

pub mod descriptor;
pub mod report;
pub mod service;

use bt_hci::controller::ExternalController;
use bt_hci::uuid::service as ble_service;
use core::convert::Infallible;
use defmt::{error, info, warn};
use embassy_futures::select::{Either3, select3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use esp_radio::ble::controller::BleConnector;
use trouble_host::prelude::*;

use self::report::encode_report;
use self::service::{CONNECTIONS_MAX, L2CAP_CHANNELS_MAX, Server};
use crate::protocol::{CommandResponse, Frame, encode_frame, encode_response};
use crate::transport::Transport;
use crate::transport::control::{CommandSource, dispatch_command};
use crate::ui::{BATTERY_LEVEL, set_ble_connected};

use core::sync::atomic::{AtomicU8, Ordering};
/// 广播时展示给 Host 的设备名（≤ 22 字节）
pub const DEVICE_NAME: &str = "ESP32-Controller";

/// esp-radio BLE 通道最大值，esp-hal 官方模板固定 1
const ESP_RADIO_BLE_SLOTS: usize = 1;

/// 底层 BLE controller 类型别名（`ExternalController<BleConnector, 1>`）
///
/// esp-radio 的 `BleConnector` 通过 HCI 命令与硬件 controller 交互。
pub type EspBleController<'d> = ExternalController<BleConnector<'d>, ESP_RADIO_BLE_SLOTS>;

/// 帧共享通道（Signal = "最后写入者赢"）
///
/// 主循环调用 [`BleHidTransport::send`] 会把当前 [`Frame`] 塞进来，
/// BLE 任务在 `wait()` 里取出后**同时**推送：
/// - HID Input Report（本地缩放到 6 字节）
/// - Custom FrameStream（原样 21 字节）
///
/// 连续两次 `send` 之间 BLE 若未消费，第二次会覆盖第一次 —— 这是我们
/// 想要的语义（只发最新状态）。
///
/// 使用 `CriticalSectionRawMutex` 保证跨中断/任务的原子访问；
/// esp-hal + esp-rtos 已提供 `critical-section` 实现。
pub type FrameSignal = Signal<CriticalSectionRawMutex, Frame>;

/// 命令响应共享通道（N 选项）
///
/// 当 [`crate::transport::control::handle_command`] 产生 Ack / Error /
/// NonceHello 等 [`CommandResponse`] 时，会同时写入本 Signal 与
/// [`crate::transport::esp_now::RESPONSE_SIGNAL`]，实现两链路对等的反馈。
///
/// # 为什么采用覆盖式？
/// - 高频命令场景下若 Response 堆积会造成流量放大
/// - 覆盖式保证每次最多 notify 一条最新 Response —— 心跳/连接性场景足够
pub type ResponseSignal = Signal<CriticalSectionRawMutex, CommandResponse>;

/// 全局 Response 通道（handler → BLE broadcast task）
///
/// 与 [`crate::transport::esp_now::RESPONSE_SIGNAL`] 平行存在：
/// - Command handler 同时写入两个 signal（[`crate::transport::control::broadcast_response`]）
/// - BLE task 仅订阅本 signal；ESP-NOW task 仅订阅自己那一个
/// - 互不影响，双链路各自可靠发送
pub static RESPONSE_SIGNAL: ResponseSignal = Signal::new();

/// 便捷入口：把 [`CommandResponse`] 交给 BLE 后台任务出站
///
/// # 内存与开销
/// [`CommandResponse`] 实现了 `Copy`，写入 Signal 时是纯值语义，无堆分配。
///
/// # M-3 观测（Ack 覆盖）
/// 若 [`RESPONSE_SIGNAL`] 已有一份等待被 tx task 取走的 Response，本次
/// `signal()` 会**静默覆盖**它 —— 我们通过 [`crate::metrics::record_response_overwrite`]
/// 埋点计数，便于 dashboard 观察实际发生频率。
pub fn signal_response(resp: CommandResponse) {
  if RESPONSE_SIGNAL.signaled() {
    crate::metrics::record_response_overwrite();
  }
  RESPONSE_SIGNAL.signal(resp);
}

// ============================================================
// Transport 实现
// ============================================================

/// BLE HID + Custom Controller Transport（handle 侧）
///
/// 只持有对 `'static` Signal 的引用，`send()` 是**同步、非阻塞**的：
/// 把当前 `Frame` 写进 Signal，让 BLE 任务在其自己的时序里推送。
///
/// # 为什么 Signal 里存 `Frame` 而不是编码后的字节？
/// - `Frame` 只有 21 字节负载 + `Copy`，开销可忽略
/// - BLE 任务侧才知道每个 characteristic 需要什么样的编码
/// - main 侧不需要感知底层协议，保持传输层职责纯净
pub struct BleHidTransport {
  signal: &'static FrameSignal,
}

impl BleHidTransport {
  /// 构造 handle。真正的 BLE 栈由 [`ble_gamepad_task`] 在后台运行；
  /// 二者通过 `signal` 通信。
  pub const fn new(signal: &'static FrameSignal) -> Self {
    Self { signal }
  }
}

impl Transport for BleHidTransport {
  type Error = Infallible;

  fn send(&mut self, frame: &Frame) -> Result<(), Self::Error> {
    // Signal::signal 会覆盖旧值（如果尚未被消费）
    // Frame 实现了 Copy，这里是纯值语义、无堆分配
    self.signal.signal(*frame);
    Ok(())
  }
}

// ============================================================
// BLE 后台任务
// ============================================================

/// BLE HID Gamepad 后台任务
///
/// # 传参
/// - `controller`：esp-radio 提供的 HCI controller
/// - `signal`：与主循环共享的 frame 通道
///
/// # 生命周期
/// - **永久运行**：即使 BLE 报错也会回到广播状态重试
/// - **单连接**：一次只服务一个 Host，断开后回到广播
///
/// # embassy_executor 任务需求
/// 参数必须 `'static`。调用方需要用 `static_cell::StaticCell::init`
/// 或 `mk_static!` 宏把 controller / signal 延长到 `'static`。
#[embassy_executor::task]
pub async fn ble_gamepad_task(
  controller: EspBleController<'static>,
  signal: &'static FrameSignal,
) -> ! {
  let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
    HostResources::new();
  let stack = trouble_host::new(controller, &mut resources);
  let Host {
    mut peripheral,
    mut runner,
    ..
  } = stack.build();

  let gap = GapConfig::Peripheral(PeripheralConfig {
    name: DEVICE_NAME,
    appearance: &appearance::human_interface_device::GAMEPAD,
  });
  let server = match Server::new_with_config(gap) {
    Ok(s) => s,
    Err(_e) => {
      error!("[BLE] Failed to build GATT server (attribute table too small?)");
      core::future::pending::<()>().await;
      unreachable!()
    }
  };

  info!("[BLE] Server ready, entering runner + advertise loop");

  // `runner.run()` 与广播/连接循环并发；任何一个返回都视作 BLE 栈不可恢复
  //
  // 注：runner 任务可能先返回，此时另一支 select 那边已经进入多重嵌套
  // 的循环——只能靠 "error 后长睡" 避免 task 退出。
  match embassy_futures::select::select(
    async {
      let _ = runner.run().await;
    },
    advertise_and_serve(&mut peripheral, &server, signal),
  )
  .await
  {
    embassy_futures::select::Either::First(()) => error!("[BLE] runner exited"),
    embassy_futures::select::Either::Second(()) => error!("[BLE] advertise loop exited"),
  }

  warn!("[BLE] entering permanent halt after fatal error");
  loop {
    embassy_time::Timer::after(embassy_time::Duration::from_secs(60)).await;
  }
}

/// 广播 + 处理连接的主循环
///
/// # 流程
/// 1. 构造广播数据（Flags + Name + HID Service UUID）
/// 2. `peripheral.advertise()` 阻塞等待连接
/// 3. 拿到 [`GattConnection`] 后进入内层循环：
///    - `select(conn.next(), signal.wait())` 谁先来处理谁
///    - 前者：处理 read/write 事件（Host 读 Report Map、订阅 CCCD 等）
///    - 后者：把新的 Frame 分别编码成 HID 6 字节 + Custom 21 字节，各自 `notify`
///
/// # 错误处理
/// 任何底层 BLE 错误都通过 log 汇报，然后**继续重试广播**——避免一次抖动导致
/// 整个手柄"失联"。
///
/// # 关于广播中的 UUID
/// 只广播 HID service UUID（0x1812，2 字节）。自定义 128-bit UUID 太长，
/// 塞进 31 字节广播包会挤掉设备名；自研 App 应通过设备名 `ESP32-Controller`
/// 发现设备后，用 Service Discovery 找到自定义服务。
async fn advertise_and_serve(
  peripheral: &mut Peripheral<'_, EspBleController<'static>, DefaultPacketPool>,
  server: &Server<'_>,
  signal: &'static FrameSignal,
) {
  // ---- 广播数据（≤ 31 字节，紧凑打包）----
  let mut adv_data = [0_u8; 31];
  let adv_len = match AdStructure::encode_slice(
    &[
      AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
      AdStructure::ServiceUuids16(&[ble_service::HUMAN_INTERFACE_DEVICE.into()]),
      AdStructure::CompleteLocalName(DEVICE_NAME.as_bytes()),
    ],
    &mut adv_data,
  ) {
    Ok(n) => n,
    Err(_) => {
      error!("[BLE] Failed to encode advertisement data");
      return;
    }
  };

  loop {
    info!("[BLE] Advertising as '{=str}'", DEVICE_NAME);
    let acceptor = match peripheral
      .advertise(
        &Default::default(),
        Advertisement::ConnectableScannableUndirected {
          adv_data: &adv_data[..adv_len],
          scan_data: &[],
        },
      )
      .await
    {
      Ok(a) => a,
      Err(_) => {
        error!("[BLE] advertise() failed, retrying...");
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
        continue;
      }
    };

    let conn = match acceptor.accept().await {
      Ok(c) => c,
      Err(_) => {
        error!("[BLE] accept() failed");
        continue;
      }
    };
    let conn = match conn.with_attribute_server(server) {
      Ok(c) => c,
      Err(_) => {
        error!("[BLE] Failed to attach attribute server");
        continue;
      }
    };
    info!("[BLE] Connected!");
    set_ble_connected(true);

    // 每次新连接重置 last-notified → 首帧强制 notify 一次电量
    LAST_NOTIFIED_BATTERY.store(u8::MAX, Ordering::Relaxed);

    // 每次新连接清一次 signal，避免把旧状态"追发"给新 Host
    signal.reset();
    // Response signal 同样重置：避免把先前会话遗留的 Ack / NonceHello 发给新 Host
    RESPONSE_SIGNAL.reset();

    // ---- 已连接：交替处理 GATT 事件 + 主动推送 report + 主动推送 response ----
    loop {
      match select3(conn.next(), signal.wait(), RESPONSE_SIGNAL.wait()).await {
        Either3::First(event) => {
          if !handle_gatt_event(server, event).await {
            // false = 断开
            break;
          }
        }
        Either3::Second(frame) => {
          push_frame_to_peers(server, &conn, &frame).await;
          maybe_notify_battery(server, &conn).await;
        }
        Either3::Third(resp) => {
          push_response_to_peer(server, &conn, &resp).await;
        }
      }
    }

    info!("[BLE] Disconnected, restart advertising");
    set_ble_connected(false);
  }
}

/// 把一帧 [`Frame`] 分别推送到 HID 与 Custom 两个 characteristic
///
/// 每个 characteristic 独立编码 + 独立 `notify`；任一失败仅打印 warning，
/// 不影响另一个 characteristic —— 保证"至少通过其中一个通道到达"。
async fn push_frame_to_peers<P: PacketPool>(
  server: &Server<'_>,
  conn: &GattConnection<'_, '_, P>,
  frame: &Frame,
) {
  // ---- HID Input Report（缩放后的 6 字节，供标准手柄 API 使用）----
  let hid_bytes = encode_report(&frame.payload);
  if server.set(&server.hid.input_report, &hid_bytes).is_err() {
    warn!("[BLE] set input_report failed");
  }
  if server
    .hid
    .input_report
    .notify(conn, &hid_bytes)
    .await
    .is_err()
  {
    // 一次 notify 失败通常是 MTU/流量控制问题，忽略即可
    warn!("[BLE] notify HID failed");
  }

  // ---- Custom FrameStream（原始 21 字节，含 seq / CRC，供自研 App 使用）----
  let custom_bytes = encode_frame(frame);
  if server
    .set(&server.custom.frame_stream, &custom_bytes)
    .is_err()
  {
    warn!("[BLE] set custom frame_stream failed");
  }
  if server
    .custom
    .frame_stream
    .notify(conn, &custom_bytes)
    .await
    .is_err()
  {
    warn!("[BLE] notify custom failed");
  }
}

/// 把一条 [`CommandResponse`] notify 给已连接的 Host（N 选项）
///
/// # 流程
/// 1. `encode_response()` 将强类型 Response 序列化为 20 字节（含 CRC + HMAC）
/// 2. `server.set()` 更新属性表，保证 Host 下次 read 能拿到最新值
/// 3. `.notify()` 主动推送给订阅了 CCCD 的 Host
///
/// # 失败策略
/// set / notify 任一失败只 warn，不影响下一次尝试（MTU / 流控抖动常见）。
async fn push_response_to_peer<P: PacketPool>(
  server: &Server<'_>,
  conn: &GattConnection<'_, '_, P>,
  resp: &CommandResponse,
) {
  let bytes = encode_response(resp);
  if server.set(&server.custom.control_response, &bytes).is_err() {
    warn!("[BLE] set control_response failed");
  }
  if server
    .custom
    .control_response
    .notify(conn, &bytes)
    .await
    .is_err()
  {
    warn!("[BLE] notify control_response failed");
    return;
  }
  info!(
    "[BLE] response notified: req_seq={} kind=0x{:02x}",
    resp.req_seq,
    resp.body.kind() as u8
  );
}

/// 处理单个 GATT 事件；返回 `false` 表示对端断开
///
/// # 命令通道处理
/// 若事件是 `Write` 到 `server.custom.control_command`，先解码字节调用
/// [`crate::transport::control::dispatch_command`]，然后 `accept()` 让 stack 回复 Host。
async fn handle_gatt_event<P: PacketPool>(
  server: &Server<'_>,
  event: GattConnectionEvent<'_, '_, P>,
) -> bool {
  match event {
    GattConnectionEvent::Disconnected { reason: _ } => {
      info!("[BLE] Peer disconnected");
      false
    }
    GattConnectionEvent::Gatt { event } => {
      // 先看是不是写入 control_command
      if let GattEvent::Write(ref write_evt) = event {
        let control_handle = server.custom.control_command.handle;
        if write_evt.handle() == control_handle {
          // 命令通道：解码 + 分发（不阻塞，仅内存操作）
          dispatch_command(CommandSource::Ble, write_evt.data());
        }
      }

      // trouble-host 0.6 里 GATT 事件必须 accept/reject 才会真正响应对端
      match event.accept() {
        Ok(reply) => reply.send().await,
        Err(_) => warn!("[BLE] gatt accept failed"),
      }
      true
    }
    _ => true,
  }
}

// ============================================================
// 电量 notify 相关
// ============================================================

/// 上一次真正 notify 出去的电量（0..=100）；`u8::MAX` 表示"从未 notify"
///
/// 每次新连接建立时重置为 `u8::MAX`，强制第一次 notify —— 确保 Host 连上后
/// 立刻拿到当前电量，不用等电量变化。
static LAST_NOTIFIED_BATTERY: AtomicU8 = AtomicU8::new(u8::MAX);

/// 尝试推送 Battery Level notify
///
/// # 推送策略（避免每帧都 notify 造成流量浪费）
/// - 电量与上次 notify 值**不同** → 立刻 notify
/// - 电量与上次 notify 值**相同** → 什么都不做
///
/// 由于 [`crate::hal::battery`] 每 5 秒才采样一次，电量变化本身就是低频事件；
/// 配合本函数的去抖，对 BLE 流量几乎没有影响。
async fn maybe_notify_battery<P: PacketPool>(
  server: &Server<'_>,
  conn: &GattConnection<'_, '_, P>,
) {
  let current = BATTERY_LEVEL.load(Ordering::Relaxed);
  let last = LAST_NOTIFIED_BATTERY.load(Ordering::Relaxed);

  if current == last {
    return;
  }

  // 更新 attribute table，让"Host 后续 Read"能拿到最新值
  if server.set(&server.battery.level, &current).is_err() {
    warn!("[BLE] set battery.level failed");
    return;
  }
  if server.battery.level.notify(conn, &current).await.is_err() {
    warn!("[BLE] notify battery.level failed");
    return;
  }

  LAST_NOTIFIED_BATTERY.store(current, Ordering::Relaxed);
  info!("[BLE] battery notified: {}%", current);
}
