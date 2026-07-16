//! # Notifier 内部 Signal 类型别名
//!
//! 用户在自己的 crate 里如下声明这几个 `static`：
//! ```ignore
//! use comm::notifier::signals::*;
//!
//! static FRAME_SIG: FrameSignal = FrameSignal::new();
//! static CMD_OUT_SIG: CommandOutSignal = CommandOutSignal::new();
//! static RESP_SIG: ResponseSignal = ResponseSignal::new();
//! ```
//! 然后传给 [`Notifier::builder()`](super::Notifier::builder)。
//!
//! ## 为什么用 `Signal` 而不是 `Channel`
//! - Frame 是高频状态流，只关心**最新**；覆盖式语义（`Signal` = "最后写入者赢"）
//!   避免堆积
//! - Command/Response 频率低，覆盖式偶发丢失也是可容忍的（下一次 Announce 会重发）

use controller_protocol::{COMMAND_LEN, CommandResponse, Frame};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

/// Frame 出站 Signal（主循环 → broadcast task）
pub type FrameSignal = Signal<CriticalSectionRawMutex, Frame>;

/// Command 出站 Signal（发现流程 / send_command → broadcast task）
///
/// 值是**已编码**的 [`COMMAND_LEN`] 字节；避免在 Signal 里放巨大 enum。
pub type CommandOutSignal = Signal<CriticalSectionRawMutex, [u8; COMMAND_LEN]>;

/// Response 出站 Signal（命令 handler → broadcast task）
pub type ResponseSignal = Signal<CriticalSectionRawMutex, CommandResponse>;
