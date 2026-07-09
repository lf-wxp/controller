//! 摇杆：X 轴 + Y 轴 + 按下按键的组合抽象
//!
//! # 硬件电气结构（本项目）
//! ```text
//! 3V3 ── X轴电位器 ── GND     中间抽头 → IO32 (ADC1_CH4)
//! 3V3 ── Y轴电位器 ── GND     中间抽头 → IO33 (ADC1_CH5)
//! GND ── [按钮 SW] ── IO12    (按下拉低；IO12 是 strapping pin，需 Pull::Down)
//! ```
//!
//! # 输出
//! - `read()` 返回 [`JoystickReading`]，X/Y 均已死区处理并归一化到 -1000..+1000
//! - 摇杆按下键使用普通 [`Button`] 抽象，含消抖

use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcChannel};
use esp_hal::peripherals::ADC1;

use super::analog::AnalogInput;
use super::button::{Button, ButtonState};

/// 摇杆一次采样的结果
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JoystickReading {
  /// X 轴：-1000..+1000（左负右正，具体方向取决于硬件走线）
  pub x: i16,
  /// Y 轴：-1000..+1000（前负后正，具体方向取决于硬件走线）
  pub y: i16,
  /// 按下键当前状态
  pub button: ButtonState,
}

impl JoystickReading {
  /// 是否处于归中位置（X 和 Y 都在死区内）
  pub fn is_centered(&self) -> bool {
    self.x == 0 && self.y == 0
  }

  /// 便捷：按键是否被按下（稳态）
  pub fn is_button_down(&self) -> bool {
    self.button.is_down()
  }
}

/// 摇杆组合组件
///
/// 类型参数：
/// - `'d`   ：ADC1 借用生命周期
/// - `XPIN` ：X 轴 GPIO 类型（应为 GPIO32）
/// - `YPIN` ：Y 轴 GPIO 类型（应为 GPIO33）
pub struct Joystick<'d, XPIN, YPIN>
where
  XPIN: AdcChannel,
  YPIN: AdcChannel,
{
  x: AnalogInput<'d, XPIN>,
  y: AnalogInput<'d, YPIN>,
  button: Button<'d>,
}

impl<'d, XPIN, YPIN> Joystick<'d, XPIN, YPIN>
where
  XPIN: AdcChannel,
  YPIN: AdcChannel,
{
  /// 构造摇杆
  pub fn new(x: AnalogInput<'d, XPIN>, y: AnalogInput<'d, YPIN>, button: Button<'d>) -> Self {
    Self { x, y, button }
  }

  /// 采样一次，返回完整读数
  ///
  /// 需要传入共享的 ADC1 引用（同时轮询多路 ADC 时用得到）
  pub fn read(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> JoystickReading {
    let x = self.x.read_centered(adc);
    let y = self.y.read_centered(adc);
    let button = self.button.poll();
    JoystickReading { x, y, button }
  }
}
