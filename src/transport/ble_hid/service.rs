//! GATT 服务定义：HID Gamepad + Battery + Device Information + **Custom Controller**
//!
//! 使用 `#[gatt_service]` / `#[gatt_server]` 派生宏声明，宏会在编译期生成：
//! - 一个 `Server<'a>` 结构体，持有所有 characteristic 句柄
//! - `Server::new_with_config(gap)` 构造函数（自动构造 attribute table）
//! - 每个 characteristic 的强类型访问器（`server.hid.input_report` 等）
//! - `server.set(&handle, &value)` 和 `handle.notify(conn, &value)` API
//!
//! # 服务清单
//! - **HID (0x1812)** —— 手柄核心，Report Map 描述形状，Report 承载数据（6 字节缩放值）
//! - **Battery (0x180F)** —— 电量（当前固定 100%）
//! - **Device Information (0x180A)** —— 厂商名，让手机显示"ESP32 by Rust"
//! - **Custom Controller (自定义 128-bit UUID)** —— 完整 25 字节协议帧，
//!   自研 App / Python 客户端可以拿到 `seq / CRC / 未缩放的 i16 / u16 原始值`
//!
//! # 为什么"双轨并行"？
//! - HID 服务保证**通用兼容**：任何手机 / PC 都能识别为手柄
//! - Custom 服务保证**完整精度**：不做任何缩放、附带 seq/CRC，便于回放/调试/精细控制
//!
//! # 使用同一个 BLE 连接
//! 两个服务共享 GATT server + 广播 + 后台任务；主循环 `Transport::send()`
//! 一次调用，两个 characteristic 都会更新并 notify。

// `#[gatt_service]` 宏展开会产生 &literal 传参，clippy 会认为多余的引用；
// 这是宏本身的实现选择，不是我们能控制的
#![allow(clippy::needless_borrows_for_generic_args)]

use trouble_host::prelude::*;

use crate::protocol::FRAME_LEN;

use super::descriptor::{HID_INFO, PROTOCOL_MODE_REPORT, REPORT_MAP, REPORT_REFERENCE_INPUT};

/// 同时最多支持的 BLE 连接数（手柄只需要 1）
pub const CONNECTIONS_MAX: usize = 1;
/// 同时最多支持的 L2CAP 通道数（HID 只用 ATT 通道）
pub const L2CAP_CHANNELS_MAX: usize = 1;
/// 属性表容量（服务 + characteristics + descriptors + CCCD）
///
/// - GAP + GATT: 6
/// - HID: ~13 (service + 5 char × 2..3 attrs + 1 descriptor)
/// - Battery: 4
/// - Device Info: 3
/// - Custom Controller: ~14 (service + 3 char：FrameStream(notify) + ControlCommand(write) + ControlResponse(notify))
/// - 余量: ~20
const ATTRIBUTE_TABLE_SIZE: usize = 60;

// ============================================================
// HID Service (0x1812)
// ============================================================
/// HID over GATT 服务
///
/// # 必需 characteristic 一览
/// | UUID   | 名字            | 属性                | 说明                              |
/// |--------|-----------------|---------------------|-----------------------------------|
/// | 0x2A4A | HID Information | Read                | HID spec 版本 + 国家 + flags       |
/// | 0x2A4B | Report Map      | Read                | 我们的手柄描述符（[REPORT_MAP]）  |
/// | 0x2A4C | Control Point   | Write w/o response  | Host 用来"暂停/恢复"（我们忽略）  |
/// | 0x2A4D | Report          | Read + Notify       | Input Report（6 字节数据）        |
/// | 0x2A4E | Protocol Mode   | Read + Write w/o R  | 0x01 = Report Mode                |
#[gatt_service(uuid = service::HUMAN_INTERFACE_DEVICE)]
pub struct HidService {
  /// HID Information —— 静态版本信息
  #[characteristic(uuid = characteristic::HID_INFORMATION, read, value = HID_INFO)]
  pub hid_info: [u8; 4],

  /// Report Map —— 描述手柄形状的字节序列（HID Report Descriptor）
  ///
  /// Host 在连接后**必读**此 characteristic 才知道后续 report 字节的含义。
  #[characteristic(uuid = characteristic::REPORT_MAP, read, value = REPORT_MAP)]
  pub report_map: [u8; 63],

  /// HID Control Point —— Host 用来告诉设备"暂停/恢复"，我们只处理写入不做实际动作
  #[characteristic(uuid = characteristic::HID_CONTROL_POINT, write_without_response, value = 0u8)]
  pub control_point: u8,

  /// Input Report —— 手柄状态数据（6 字节）
  ///
  /// - `read`：Host 主动查询当前状态
  /// - `notify`：Host 订阅后，我们主动推送变化
  ///
  /// 附带 Report Reference Descriptor (0x2908)，声明这是 Input Report + Report ID。
  #[descriptor(uuid = descriptors::REPORT_REFERENCE, read, value = REPORT_REFERENCE_INPUT)]
  #[characteristic(uuid = characteristic::REPORT, read, notify, value = [0u8; 6])]
  pub input_report: [u8; 6],

  /// Protocol Mode —— 0x00 = Boot, 0x01 = Report（我们只做 Report）
  #[characteristic(uuid = characteristic::PROTOCOL_MODE, read, write_without_response, value = PROTOCOL_MODE_REPORT)]
  pub protocol_mode: u8,
}

// ============================================================
// Battery Service (0x180F)
// ============================================================
/// 电量服务（初始固定 100%，后续可以接实际电池测量）
#[gatt_service(uuid = service::BATTERY)]
pub struct BatteryService {
  /// Battery Level —— 0..100
  #[characteristic(uuid = characteristic::BATTERY_LEVEL, read, notify, value = 100u8)]
  pub level: u8,
}

// ============================================================
// Device Information Service (0x180A)
// ============================================================
/// 设备信息服务（让手机显示"Manufacturer: Rust Controller"等）
#[gatt_service(uuid = service::DEVICE_INFORMATION)]
pub struct DeviceInformationService {
  /// Manufacturer Name String
  ///
  /// 用固定长度 `[u8; 16]` 而非 String，因为宏对 `heapless::String` 需要 feature 依赖；
  /// 字节内容是 ASCII 字符 "Rust Controller\0"（不足处填 0）。
  #[characteristic(uuid = characteristic::MANUFACTURER_NAME_STRING, read, value = MANUFACTURER_NAME)]
  pub manufacturer: [u8; 16],
}

/// 制造商名（16 字节，ASCII，不足处填 0）
const MANUFACTURER_NAME: [u8; 16] = *b"Rust Controller\0";

// ============================================================
// Custom Controller Service（自定义 128-bit UUID）
// ============================================================
//
// # UUID 生成规则
// 我们采用 UUIDv4 的"品牌化"设计：
// - 前 4 字节 `C7 00 00 0X` 用作"服务 + characteristic 编号"（`0xC7` 是本项目魔数）
// - 中段 `1E 00 - 4000 - 8000` 是 UUIDv4 保留位（version=4, variant=RFC4122）
// - 后 6 字节 `0000 0000 00 CC` 藏了 "CC"（Controller）
//
// 一个 128-bit UUID 在 GATT 里的常规文本表达：
//     c7000001-1e00-4000-8000-0000000000cc
//
// # UUID 分配
// - Service:          c7000001-... —— 主服务
// - FrameStream:      c7000002-... —— 完整帧（25 字节）
// - ControlCommand:   c7000003-... —— Host 反向下发命令（24 字节，含 HMAC）
// - ControlResponse:  c7000004-... —— 手柄→Host 命令回执 / NonceHello（24 字节，N 选项）
//
// # 字节数组表示
// trouble-host 的 `Uuid::new_long` 期望 **大端序** 16 字节。
// UUID 文本 "c7000001-1e00-4000-8000-0000000000cc" 转字节即：
//     [0xC7, 0x00, 0x00, 0x01, 0x1E, 0x00, 0x40, 0x00,
//      0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xCC]

/// Custom Controller Service UUID —— `c7000001-1e00-4000-8000-0000000000cc`
///
/// 用 `u128` 常量而不是字符串，是因为 `#[gatt_service]` 宏要求 `uuid = <expr>`
/// 表达式返回类型实现 `Into<Uuid>`；`u128` 有 `From<u128> for Uuid` 直接实现，
/// 字面量 `&'static str` 则没有。文本 UUID 转成 `u128` 的映射如下：
///
/// ```text
///   c7000001 - 1e00 - 4000 - 8000 - 0000 00000000 cc
///     ▲          ▲       ▲       ▲        ▲              ▲
///     time_low   time_mid ver+   clock+   node (48 bits)
///                        time_hi variant
/// ```
/// 把 32 位 hex 直接排在 u128 里即得下面这个常量。
pub const CUSTOM_SERVICE_UUID: u128 = 0xc700_0001_1e00_4000_8000_0000_0000_00cc;

/// FrameStream Characteristic UUID —— `c7000002-1e00-4000-8000-0000000000cc`
const FRAME_STREAM_UUID: u128 = 0xc700_0002_1e00_4000_8000_0000_0000_00cc;

/// Control Command Characteristic UUID —— `c7000003-1e00-4000-8000-0000000000cc`
///
/// 反向通道：Host 通过 `write` 或 `write_without_response` 下发 24 字节的
/// [`crate::protocol::Command`] 帧，手柄侧解码后执行。
const CONTROL_COMMAND_UUID: u128 = 0xc700_0003_1e00_4000_8000_0000_0000_00cc;

/// Control Response Characteristic UUID —— `c7000004-1e00-4000-8000-0000000000cc`
///
/// 手柄 → Host 的**反馈通道**（N 选项）：
/// - Ack / Error：命令执行结果（Host 可以知道自己发的 seq 是否已正确执行）
/// - BatterySnapshot：主动上报电量（预留，当前未使用）
/// - NonceHello：周期广播 session nonce（K3）
///
/// **与 ESP-NOW 对等**：一条 [`crate::protocol::CommandResponse`] 同时通过
/// BLE notify 与 ESP-NOW 广播发出，Host 任选一条链路接收。
const CONTROL_RESPONSE_UUID: u128 = 0xc700_0004_1e00_4000_8000_0000_0000_00cc;

/// Command 帧长度（24 字节）—— 供 `#[characteristic]` 宏的 `value = [0u8; N]` 使用
use crate::protocol::COMMAND_LEN;

/// Response 帧长度（24 字节）—— 同上；与 Command 同长但方向不同，magic 区分
use crate::protocol::RESPONSE_LEN;

/// 自定义控制器服务 —— 双向：完整 25 字节协议帧 + 24 字节反向控制命令（含 HMAC 认证 + 抗重放 seq）
///
/// # Characteristics
/// - `frame_stream`（`read + notify`）：手柄 → Host，完整 25 字节协议帧（30 Hz）
/// - `control_command`（`write + write_without_response`）：Host → 手柄，24 字节命令帧（含 HMAC + seq 抗重放）
/// - `control_response`（`read + notify`）：手柄 → Host，24 字节响应帧（N 选项：Ack / Error / NonceHello）
///
/// # 特点
/// - **零缩放**：完全保留 `i16 [-1000..+1000]` 和 `u16 [0..1000]` 的原始精度
/// - **含 seq / CRC**：可用于回放调试、丢包检测、二进制比对
/// - **双向控制**：Host 可下发 LED 特效、灵敏度调节、Toast 提示等命令
/// - **双轨反馈**（N 选项）：同一条 Response 同时通过 BLE notify 与 ESP-NOW 广播发出
#[gatt_service(uuid = CUSTOM_SERVICE_UUID)]
pub struct CustomControllerService {
  /// FrameStream —— 完整协议帧（21 字节）
  ///
  /// - `read`：一次性抓取当前帧
  /// - `notify`：订阅后主动推送每次状态更新（30 Hz）
  #[characteristic(uuid = FRAME_STREAM_UUID, read, notify, value = [0u8; FRAME_LEN])]
  pub frame_stream: [u8; FRAME_LEN],

  /// ControlCommand —— Host 反向下发命令（20 字节固定）
  ///
  /// - `write`：需要 response（可靠但较慢）
  /// - `write_without_response`：无响应（低延迟，适合频繁命令）
  ///
  /// 收到写入后，`ble_gamepad_task` 会在 `handle_gatt_event` 里调用
  /// [`crate::transport::control::dispatch_command`] 解码并执行。
  #[characteristic(uuid = CONTROL_COMMAND_UUID, write, write_without_response, value = [0u8; COMMAND_LEN])]
  pub control_command: [u8; COMMAND_LEN],

  /// ControlResponse —— 手柄 → Host 反馈通道（20 字节，N 选项）
  ///
  /// - `read`：一次性拉取最新 Response（健壮性备份，主链路仍为 notify）
  /// - `notify`：订阅后事件驱动推送，与 ESP-NOW 广播对等
  ///
  /// **Response 类型与布局**：参见 [`crate::protocol::response`]（2B magic + 1B ver +
  /// 4B req_seq + 1B kind + 6B payload + 4B hmac + 2B crc）。
  #[characteristic(uuid = CONTROL_RESPONSE_UUID, read, notify, value = [0u8; RESPONSE_LEN])]
  pub control_response: [u8; RESPONSE_LEN],
}

// ============================================================
// GATT Server —— 聚合所有服务
// ============================================================
/// GATT Server 顶层结构（宏生成）
///
/// 提供：
/// - `Server::new_with_config(GapConfig)` 构造
/// - `server.hid.input_report` / `server.battery.level` / ... 强类型 handle
/// - `server.set(&handle, &value)` 修改属性表
#[gatt_server(
    connections_max = CONNECTIONS_MAX,
    mutex_type = embassy_sync::blocking_mutex::raw::NoopRawMutex,
    attribute_table_size = ATTRIBUTE_TABLE_SIZE
)]
pub struct Server {
  /// HID 主服务
  pub hid: HidService,
  /// 电量
  pub battery: BatteryService,
  /// 设备信息
  pub device_info: DeviceInformationService,
  /// 自定义控制器服务（完整 21 字节协议帧）
  pub custom: CustomControllerService,
}
