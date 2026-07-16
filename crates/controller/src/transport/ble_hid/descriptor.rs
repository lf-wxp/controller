//! HID Report Descriptor —— 手柄"契约"
//!
//! # 这是什么
//! Report Descriptor 是 HID over GATT 的核心：一段"字节魔法"，告诉 Host
//! （手机/PC/iPad）我这个设备**长什么样**：多少个按钮、多少个轴、每个字段的
//! 范围是多少、字节顺序是什么。Host 拿到它以后就能把后续的 Input Report
//! 字节流"翻译"成"按下了 A 键 + 摇杆偏移 X=-30" 这样的语义事件。
//!
//! # 我们要描述的手柄
//! - Usage Page: 0x01 (Generic Desktop)
//! - Usage:      0x05 (Gamepad)
//! - Collection: Application
//!     - 6 个按钮 (Button 1..6)：Btn1..Btn4 + JoyBtn + Switch
//!     - 2 bits padding（凑成完整字节）
//!     - X, Y (i8, -127..127)：摇杆两轴
//!     - Z, Rz (u8, 0..255)：两个旋钮映射为第 3、4 轴
//!
//! # Report 布局（必须与 [`super::report::encode_report`] 精确一致）
//! ```text
//!  offset | size | field
//!  -------+------+----------------------------
//!    0    |  1B  | buttons (bits 0..5 有效)
//!    1    |  1B  | X  (i8)
//!    2    |  1B  | Y  (i8)
//!    3    |  1B  | Z  (u8)   ← knob_1
//!    4    |  1B  | Rz (u8)   ← knob_2
//!    5    |  1B  | reserved (=0)
//! ```
//!
//! # 参考
//! - USB HID Usage Tables 1.4, Section 5 (Game Controls Page)
//! - Bluetooth HID over GATT Profile 1.0

/// HID Report ID —— 本手柄只有一个 report，用 ID = 1
///
/// **Report ID 存在的意义**：允许一个 HID 设备有多种 report（比如键盘+鼠标合一）。
/// 我们只有一种 report，但很多 Host（尤其是 iOS）**要求** report ID 非零；
/// 所以我们用 1，并在每次 Input Report 首字节前添加 report ID。
///
/// **但是**：BLE HID Report characteristic 天然按 handle 区分，不需要 report ID 前缀；
/// 因此 [`REPORT_MAP`] 里也**不**声明 report ID —— 让 characteristic handle
/// 自己做 report 分发（这是 BLE HID 相对 USB HID 的简化）。
pub const REPORT_ID: u8 = 0;

/// HID 输入 report 的字节长度（不含 report ID）
pub const REPORT_LEN: usize = 6;

/// HID Report Descriptor 字节数
pub const REPORT_MAP_LEN: usize = 63;

/// HID Report Descriptor 的完整字节序列
///
/// 用 `[u8; N]` 定长数组（而非 `&[u8]`），是因为 `#[gatt_service]` 宏要求
/// characteristic 字段是 `AsGatt` 实现类型；`[u8; N]` 在 trouble-host 里有实现，
/// 但 `&[u8]` 没有。
///
/// # 逐字节含义
/// ```text
/// 05 01     Usage Page (Generic Desktop)
/// 09 05     Usage (Gamepad)
/// A1 01     Collection (Application)
///   A1 00       Collection (Physical)
///     05 09         Usage Page (Button)
///     19 01         Usage Minimum (Button 1)
///     29 06         Usage Maximum (Button 6)
///     15 00         Logical Minimum (0)
///     25 01         Logical Maximum (1)
///     75 01         Report Size (1)
///     95 06         Report Count (6)
///     81 02         Input (Data, Var, Abs)
///     75 01         Report Size (1)
///     95 02         Report Count (2)
///     81 03         Input (Const, Var, Abs)         ← 2 bit padding
///     05 01         Usage Page (Generic Desktop)
///     09 30         Usage (X)
///     09 31         Usage (Y)
///     15 81         Logical Minimum (-127)
///     25 7F         Logical Maximum (127)
///     75 08         Report Size (8)
///     95 02         Report Count (2)
///     81 02         Input (Data, Var, Abs)
///     09 32         Usage (Z)
///     09 35         Usage (Rz)
///     15 00         Logical Minimum (0)
///     26 FF 00      Logical Maximum (255)           ← 2-byte value
///     75 08         Report Size (8)
///     95 02         Report Count (2)
///     81 02         Input (Data, Var, Abs)
///   C0          End Collection (Physical)
/// C0        End Collection (Application)
/// ```
#[rustfmt::skip]
pub const REPORT_MAP: [u8; REPORT_MAP_LEN] = [
    0x05, 0x01,       // Usage Page (Generic Desktop)
    0x09, 0x05,       // Usage (Gamepad)
    0xA1, 0x01,       // Collection (Application)
    0xA1, 0x00,       //   Collection (Physical)

    // ---- Buttons: 6 bits + 2 bits padding = 1 byte ----
    0x05, 0x09,       //     Usage Page (Button)
    0x19, 0x01,       //     Usage Minimum (Button 1)
    0x29, 0x06,       //     Usage Maximum (Button 6)
    0x15, 0x00,       //     Logical Minimum (0)
    0x25, 0x01,       //     Logical Maximum (1)
    0x75, 0x01,       //     Report Size (1)
    0x95, 0x06,       //     Report Count (6)
    0x81, 0x02,       //     Input (Data, Var, Abs)
    0x75, 0x01,       //     Report Size (1)
    0x95, 0x02,       //     Report Count (2)
    0x81, 0x03,       //     Input (Const, Var, Abs) ← padding

    // ---- Sticks: X, Y as signed 8-bit ----
    0x05, 0x01,       //     Usage Page (Generic Desktop)
    0x09, 0x30,       //     Usage (X)
    0x09, 0x31,       //     Usage (Y)
    0x15, 0x81,       //     Logical Minimum (-127)
    0x25, 0x7F,       //     Logical Maximum (127)
    0x75, 0x08,       //     Report Size (8)
    0x95, 0x02,       //     Report Count (2)
    0x81, 0x02,       //     Input (Data, Var, Abs)

    // ---- Knobs: Z, Rz as unsigned 8-bit ----
    0x09, 0x32,       //     Usage (Z)
    0x09, 0x35,       //     Usage (Rz)
    0x15, 0x00,       //     Logical Minimum (0)
    0x26, 0xFF, 0x00, //     Logical Maximum (255)
    0x75, 0x08,       //     Report Size (8)
    0x95, 0x02,       //     Report Count (2)
    0x81, 0x02,       //     Input (Data, Var, Abs)

    0xC0,             //   End Collection
    0xC0,             // End Collection
];

/// HID Information characteristic 值（4 字节，固定不变）
///
/// 布局（bt_hci 未提供便利结构，手工构造）：
/// ```text
///  offset | size | field
///  -------+------+---------------------
///    0    |  2B  | bcdHID = 0x0111  (HID spec 1.11, little-endian)
///    2    |  1B  | bCountryCode = 0 (not localized)
///    3    |  1B  | flags = 0x02     (bit1 = NormallyConnectable)
/// ```
#[rustfmt::skip]
pub const HID_INFO: [u8; 4] = [
    0x11, 0x01, // bcdHID 1.11
    0x00,       // country code (not localized)
    0x02,       // flags: NormallyConnectable
];

/// Protocol Mode 初始值：`0x01` = Report Protocol（HID over GATT 默认）
pub const PROTOCOL_MODE_REPORT: u8 = 0x01;

/// Report Reference Descriptor（Client Characteristic Configuration 里描述 report 类型）
///
/// 布局：`[report_id, report_type]`，我们只有一个 Input Report。
#[rustfmt::skip]
pub const REPORT_REFERENCE_INPUT: [u8; 2] = [
    REPORT_ID,      // Report ID
    0x01,           // Report Type = Input
];
