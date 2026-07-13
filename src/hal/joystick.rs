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
  /// X 轴：-1000..=+1000（右 = 正，左 = 负）
  pub x: i16,
  /// Y 轴：-1000..=+1000（上 = 正，下 = 负，数学坐标系）
  ///
  /// 注：`Joystick::read` 内部已对硬件读数取反，补偿外壳装反 180°。
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

  /// 上电校准：把当前静止读数作为 X/Y 轴的 `zero_offset`
  ///
  /// # 前置条件
  /// 调用时摇杆必须**物理居中**（用户没有按住或推歪）。
  ///
  /// # 返回
  /// `(x_zero, y_zero)` 生效后的两轴零点，主要用于日志/诊断。
  ///
  /// 参见 [`AnalogInput::calibrate`] 了解拒绝策略与 fallback 行为。
  pub fn calibrate(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> (u16, u16) {
    let x_zero = self.x.calibrate(adc);
    let y_zero = self.y.calibrate(adc);
    (x_zero, y_zero)
  }

  /// 采样一次，返回完整读数
  ///
  /// 需要传入共享的 ADC1 引用（同时轮询多路 ADC 时用得到）
  ///
  /// # 方向约定（数学坐标系）
  /// - X 轴：右 = 正，左 = 负
  /// - Y 轴：上 = 正，下 = 负
  ///
  /// 由于手柄外壳把屏幕/摇杆整体装反了 180°，Y 轴在此处取反补偿，
  /// 保证下游（BLE HID / ESP-NOW / OLED / dashboard）看到的语义一致。
  pub fn read(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> JoystickReading {
    let x = self.x.read_centered(adc);
    // Y 轴取反：补偿硬件安装方向，让"物理向上"= 正值。
    // 使用 saturating_neg 防御 i16::MIN 溢出（read_centered 已 clamp
    // 到 [-AXIS_RANGE, +AXIS_RANGE]，理论不会触发，仍保留防御）。
    let y = self.y.read_centered(adc).saturating_neg();
    let button = self.button.poll();
    JoystickReading { x, y, button }
  }
}
