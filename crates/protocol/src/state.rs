//! 手柄输入状态 —— 一帧的完整快照
//!
//! 这是**语义层**的数据结构：字段与业务对应（哪个摇杆、哪个按钮），
//! 与网络字节层的 [`Frame`](super::Frame) 分离。
//!
//! [`GamepadState`] 可以自由地：
//! - 从 [`InputSampler`](crate::input::InputSampler) 一次采样得到
//! - 转换成 [`Frame`](super::Frame) 通过 [`Transport`](crate::transport::Transport) 发出
//! - 在接收端还原回来做业务判断（按了哪几个键、摇杆偏移多少）

use core::fmt;

/// 按键位图中每个按钮占据的比特位
///
/// 使用位图而非结构体的原因：8 个键塞进 2 字节，扩展空间大，序列化简单。
/// 位分配保留了向前兼容——加新按钮就用下一个未用的比特，不破坏旧接收端。
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonBits {
  /// 通用按钮 1（面板 IO27）
  Btn1 = 0,
  /// 通用按钮 2（面板 IO13）
  Btn2 = 1,
  /// 通用按钮 3（面板 IO25）
  Btn3 = 2,
  /// 通用按钮 4（面板 IO23）
  Btn4 = 3,
  /// 摇杆按下键（IO12）
  JoyBtn = 4,
  /// 拨动开关（IO15）—— 虽然是"开关"，也用位图表达"当前是否开"
  Switch = 5,
  // 位 6-15 预留
}

impl ButtonBits {
  /// 转为掩码（1 << bit）
  pub const fn mask(self) -> u16 {
    1_u16 << (self as u16)
  }
}

/// 手柄输入状态 —— 一帧的完整快照
///
/// 序列化后正好 [`PAYLOAD_LEN`] 字节。字段顺序即字节顺序（little-endian）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GamepadState {
  /// 按键位图：见 [`ButtonBits`]
  pub buttons: u16,
  /// 摇杆 X 轴：-AXIS_RANGE..+AXIS_RANGE（中位=0，符号=方向）
  pub joy_x: i16,
  /// 摇杆 Y 轴：-AXIS_RANGE..+AXIS_RANGE
  pub joy_y: i16,
  /// 旋钮 1：0..=AXIS_RANGE（单向刻度）
  pub knob_1: u16,
  /// 旋钮 2：0..=AXIS_RANGE
  pub knob_2: u16,
  /// 保留字段（对齐 + 未来扩展），当前必须为 0
  pub _reserved: u16,
}

/// `GamepadState` 序列化后的固定长度
pub const PAYLOAD_LEN: usize = 12;

impl GamepadState {
  /// 空状态（全部零值），常用作初始值
  pub const EMPTY: Self = Self {
    buttons: 0,
    joy_x: 0,
    joy_y: 0,
    knob_1: 0,
    knob_2: 0,
    _reserved: 0,
  };

  /// 判断某个按钮是否处于按下（位图中对应位为 1）
  pub const fn is_pressed(&self, bit: ButtonBits) -> bool {
    (self.buttons & bit.mask()) != 0
  }

  /// 设置某个按钮的位状态
  pub fn set_button(&mut self, bit: ButtonBits, on: bool) {
    if on {
      self.buttons |= bit.mask();
    } else {
      self.buttons &= !bit.mask();
    }
  }

  /// 序列化到 `PAYLOAD_LEN` 字节数组（little-endian）
  ///
  /// # 字节布局
  /// | offset | size | field     |
  /// |--------|------|-----------|
  /// | 0      | 2    | buttons   |
  /// | 2      | 2    | joy_x     |
  /// | 4      | 2    | joy_y     |
  /// | 6      | 2    | knob_1    |
  /// | 8      | 2    | knob_2    |
  /// | 10     | 2    | _reserved |
  pub fn to_bytes(&self) -> [u8; PAYLOAD_LEN] {
    let mut buf = [0_u8; PAYLOAD_LEN];
    buf[0..2].copy_from_slice(&self.buttons.to_le_bytes());
    buf[2..4].copy_from_slice(&self.joy_x.to_le_bytes());
    buf[4..6].copy_from_slice(&self.joy_y.to_le_bytes());
    buf[6..8].copy_from_slice(&self.knob_1.to_le_bytes());
    buf[8..10].copy_from_slice(&self.knob_2.to_le_bytes());
    buf[10..12].copy_from_slice(&self._reserved.to_le_bytes());
    buf
  }

  /// 从 `PAYLOAD_LEN` 字节数组反序列化
  pub fn from_bytes(buf: &[u8; PAYLOAD_LEN]) -> Self {
    Self {
      buttons: u16::from_le_bytes([buf[0], buf[1]]),
      joy_x: i16::from_le_bytes([buf[2], buf[3]]),
      joy_y: i16::from_le_bytes([buf[4], buf[5]]),
      knob_1: u16::from_le_bytes([buf[6], buf[7]]),
      knob_2: u16::from_le_bytes([buf[8], buf[9]]),
      _reserved: u16::from_le_bytes([buf[10], buf[11]]),
    }
  }
}

// 让 defmt 能直接打印 GamepadState
#[cfg(feature = "defmt")]
impl defmt::Format for GamepadState {
  fn format(&self, f: defmt::Formatter<'_>) {
    defmt::write!(
      f,
      "GamepadState {{ btns=0x{:04x} joy=({},{}) knob=({},{}) }}",
      self.buttons,
      self.joy_x,
      self.joy_y,
      self.knob_1,
      self.knob_2,
    );
  }
}

// 便于日志和调试
impl fmt::Display for GamepadState {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "btns=0x{:04x} joy=({},{}) knob=({},{})",
      self.buttons, self.joy_x, self.joy_y, self.knob_1, self.knob_2
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn roundtrip_default() {
    let s = GamepadState::default();
    let bytes = s.to_bytes();
    assert_eq!(GamepadState::from_bytes(&bytes), s);
  }

  #[test]
  fn roundtrip_full() {
    let mut s = GamepadState {
      buttons: 0,
      joy_x: -500,
      joy_y: 999,
      knob_1: 250,
      knob_2: 750,
      _reserved: 0,
    };
    s.set_button(ButtonBits::Btn1, true);
    s.set_button(ButtonBits::JoyBtn, true);
    s.set_button(ButtonBits::Switch, true);
    assert!(s.is_pressed(ButtonBits::Btn1));
    assert!(!s.is_pressed(ButtonBits::Btn2));

    let bytes = s.to_bytes();
    assert_eq!(GamepadState::from_bytes(&bytes), s);
  }

  #[test]
  fn button_bit_layout() {
    assert_eq!(ButtonBits::Btn1.mask(), 0b0000_0001);
    assert_eq!(ButtonBits::Btn2.mask(), 0b0000_0010);
    assert_eq!(ButtonBits::Btn3.mask(), 0b0000_0100);
    assert_eq!(ButtonBits::Btn4.mask(), 0b0000_1000);
    assert_eq!(ButtonBits::JoyBtn.mask(), 0b0001_0000);
    assert_eq!(ButtonBits::Switch.mask(), 0b0010_0000);
  }
}
