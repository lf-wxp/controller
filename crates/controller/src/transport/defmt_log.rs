//! defmt 日志传输 —— 开发调试专用
//!
//! 每次 [`send`](DefmtLogTransport::send)：
//! - 通过 `defmt::info!` 输出一行人类可读摘要（每帧都打）
//! - 或者按频率降频打印（默认每 15 帧打一次，避免刷屏）
//!
//! 因为 defmt 只是往 RTT 通道写字节，永远不失败，`Error` 用 [`Infallible`]。

use core::convert::Infallible;

use defmt::info;

use crate::protocol::{Frame, GamepadState};
use crate::transport::Transport;

/// 把帧打到 defmt 日志的"传输"
///
/// # 频率控制
/// 输入采样通常在 100Hz，直接每帧打日志会淹没 RTT。构造时传入 `print_every`
/// 决定"每 N 帧打一次"，同时**每次按键/开关状态变化**（即 payload.buttons 变化）
/// 都会立即打印一次，避免错过重要事件。
pub struct DefmtLogTransport {
  /// 每 N 帧打一次日志（1 = 每帧都打）
  print_every: u32,
  /// 已收到的帧计数
  counter: u32,
  /// 上一帧的按钮位图，用来检测按键状态变化
  last_buttons: u16,
  /// 是否是第一次调用（第一次强制打印，作为"session 开始"信号）
  first: bool,
}

impl DefmtLogTransport {
  /// 构造
  ///
  /// * `print_every` - 每 N 帧打一次；建议 15（≈150ms @ 100Hz）
  pub const fn new(print_every: u32) -> Self {
    Self {
      print_every: if print_every == 0 { 1 } else { print_every },
      counter: 0,
      last_buttons: 0,
      first: true,
    }
  }

  /// 打印一帧的完整摘要
  fn print_frame(&self, tag: &'static str, frame: &Frame) {
    let s: &GamepadState = &frame.payload;
    info!(
      "[TX {=str}] seq={=u32} btns=0x{=u16:04x} joy=({=i16},{=i16}) knob=({=u16},{=u16})",
      tag, frame.header.seq, s.buttons, s.joy_x, s.joy_y, s.knob_1, s.knob_2,
    );
  }
}

impl Default for DefmtLogTransport {
  fn default() -> Self {
    Self::new(15)
  }
}

impl Transport for DefmtLogTransport {
  type Error = Infallible;

  fn send(&mut self, frame: &Frame) -> Result<(), Self::Error> {
    let buttons_changed = frame.payload.buttons != self.last_buttons;

    if self.first {
      self.print_frame("start", frame);
      self.first = false;
    } else if buttons_changed {
      self.print_frame("edge", frame);
    } else if self.counter.is_multiple_of(self.print_every) {
      self.print_frame("tick", frame);
    }

    self.last_buttons = frame.payload.buttons;
    self.counter = self.counter.wrapping_add(1);
    Ok(())
  }
}
