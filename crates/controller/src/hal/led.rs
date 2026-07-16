//! 通用 LED 输出
//!
//! 支持"高电平点亮"（active_high，共阴，本项目使用）和"低电平点亮"（active_low，共阳）
//! 提供简单的开/关/翻转/闪烁接口。

use esp_hal::gpio::{Level, Output};

/// 通用 LED
pub struct Led<'d> {
  pin: Output<'d>,
  /// 点亮时电平
  active_high: bool,
  /// 当前逻辑状态（true = 点亮）
  on: bool,
}

impl<'d> Led<'d> {
  /// 构造 LED
  ///
  /// * `pin`         - 已配置好的 Output
  /// * `active_high` - 高电平点亮传 true（共阴）；低电平点亮传 false（共阳）
  pub fn new(pin: Output<'d>, active_high: bool) -> Self {
    let mut led = Self {
      pin,
      active_high,
      on: false,
    };
    led.off();
    led
  }

  /// 点亮
  pub fn on(&mut self) {
    self.pin.set_level(if self.active_high {
      Level::High
    } else {
      Level::Low
    });
    self.on = true;
  }

  /// 熄灭
  pub fn off(&mut self) {
    self.pin.set_level(if self.active_high {
      Level::Low
    } else {
      Level::High
    });
    self.on = false;
  }

  /// 翻转
  pub fn toggle(&mut self) {
    if self.on {
      self.off();
    } else {
      self.on();
    }
  }

  /// 按布尔值设定
  pub fn set(&mut self, on: bool) {
    if on {
      self.on();
    } else {
      self.off();
    }
  }

  /// 当前逻辑状态
  pub fn is_on(&self) -> bool {
    self.on
  }
}
