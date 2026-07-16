//! `GamepadState` → HID Input Report（6 字节）
//!
//! # 布局
//! 必须与 [`super::descriptor::REPORT_MAP`] 精确一致：
//! ```text
//!  offset | size | field
//!  -------+------+----------------------------
//!    0    |  1B  | buttons  bits 0..5
//!    1    |  1B  | X   (i8, -127..127)
//!    2    |  1B  | Y   (i8, -127..127)
//!    3    |  1B  | Z   (u8, 0..255)  ← knob_1
//!    4    |  1B  | Rz  (u8, 0..255)  ← knob_2
//!    5    |  1B  | reserved (=0)
//! ```
//!
//! # 归一化换算
//! 协议层的量程：
//! - 摇杆：`i16` [-1000..+1000]
//! - 旋钮：`u16` [0..1000]
//!
//! HID 手柄常用量程：
//! - 摇杆：`i8`  [-127..+127]
//! - 旋钮：`u8`  [0..255]
//!
//! 所以要做**范围缩放**（下面 `scale_*` 函数）。

use crate::config::tuning::AXIS_RANGE;
use crate::protocol::state::{ButtonBits, GamepadState};

use super::descriptor::REPORT_LEN;

/// HID Input Report（定长）
pub type Report = [u8; REPORT_LEN];

/// 把 [`GamepadState`] 编码为 HID Input Report
pub fn encode_report(state: &GamepadState) -> Report {
  let mut buf = [0_u8; REPORT_LEN];

  // ---- byte 0: buttons ----
  // 保留最低 6 位（Btn1..Btn4 + JoyBtn + Switch），高 2 位归零对应 padding
  buf[0] = pack_buttons(state);

  // ---- byte 1..2: X / Y ----
  buf[1] = scale_axis_signed(state.joy_x) as u8;
  buf[2] = scale_axis_signed(state.joy_y) as u8;

  // ---- byte 3..4: Z / Rz ----
  buf[3] = scale_knob_unsigned(state.knob_1);
  buf[4] = scale_knob_unsigned(state.knob_2);

  // buf[5] 保留 0
  buf
}

/// 把按钮位图重新打包到 HID 字节（前 6 bit）
fn pack_buttons(state: &GamepadState) -> u8 {
  let mut b = 0_u8;
  if state.is_pressed(ButtonBits::Btn1) {
    b |= 1 << 0;
  }
  if state.is_pressed(ButtonBits::Btn2) {
    b |= 1 << 1;
  }
  if state.is_pressed(ButtonBits::Btn3) {
    b |= 1 << 2;
  }
  if state.is_pressed(ButtonBits::Btn4) {
    b |= 1 << 3;
  }
  if state.is_pressed(ButtonBits::JoyBtn) {
    b |= 1 << 4;
  }
  if state.is_pressed(ButtonBits::Switch) {
    b |= 1 << 5;
  }
  b
}

/// 把 [-AXIS_RANGE..+AXIS_RANGE] 缩放到 [-127..+127]
///
/// 使用 i32 中间量避免溢出，最后 clamp 到 i8 范围。
fn scale_axis_signed(v: i16) -> i8 {
  let scaled = i32::from(v) * 127 / i32::from(AXIS_RANGE);
  scaled.clamp(-127, 127) as i8
}

/// 把 [0..AXIS_RANGE] 缩放到 [0..255]
fn scale_knob_unsigned(v: u16) -> u8 {
  let scaled = u32::from(v) * 255 / AXIS_RANGE as u32;
  scaled.min(255) as u8
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_state_is_all_zero() {
    let r = encode_report(&GamepadState::EMPTY);
    assert_eq!(r, [0, 0, 0, 0, 0, 0]);
  }

  #[test]
  fn axis_scaling_endpoints() {
    // 满量程
    assert_eq!(scale_axis_signed(AXIS_RANGE), 127);
    assert_eq!(scale_axis_signed(-AXIS_RANGE), -127);
    // 中位
    assert_eq!(scale_axis_signed(0), 0);
    // 溢出安全
    assert_eq!(scale_axis_signed(i16::MAX), 127);
    assert_eq!(scale_axis_signed(i16::MIN), -127);
  }

  #[test]
  fn knob_scaling_endpoints() {
    assert_eq!(scale_knob_unsigned(0), 0);
    assert_eq!(scale_knob_unsigned(AXIS_RANGE as u16), 255);
    // 溢出安全
    assert_eq!(scale_knob_unsigned(u16::MAX), 255);
  }

  #[test]
  fn all_buttons_pressed() {
    let mut state = GamepadState::EMPTY;
    state.set_button(ButtonBits::Btn1, true);
    state.set_button(ButtonBits::Btn2, true);
    state.set_button(ButtonBits::Btn3, true);
    state.set_button(ButtonBits::Btn4, true);
    state.set_button(ButtonBits::JoyBtn, true);
    state.set_button(ButtonBits::Switch, true);
    let r = encode_report(&state);
    // bits 0..5 = 0b0011_1111 = 0x3F
    assert_eq!(r[0], 0x3F);
  }

  #[test]
  fn full_report_roundtrip() {
    let mut state = GamepadState {
      joy_x: -1000,
      joy_y: 1000,
      knob_1: 500,
      knob_2: 0,
      ..GamepadState::EMPTY
    };
    state.set_button(ButtonBits::Btn1, true);
    let r = encode_report(&state);
    assert_eq!(r[0], 0x01); // 只按下 Btn1
    assert_eq!(r[1] as i8, -127); // joy_x 满负
    assert_eq!(r[2] as i8, 127); // joy_y 满正
    assert_eq!(r[3], 127); // knob_1 半量程 ≈ 127
    assert_eq!(r[4], 0); // knob_2 零位
    assert_eq!(r[5], 0); // 保留
  }
}
