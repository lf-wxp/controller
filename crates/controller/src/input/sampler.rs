//! [`InputSampler`] —— 打包所有硬件输入组件，一次 [`poll`] 得到完整 [`GamepadState`]

use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcChannel};
use esp_hal::peripherals::ADC1;

use crate::hal::{AnalogInput, Button, ButtonState, Joystick, JoystickReading};
use crate::protocol::state::{ButtonBits, GamepadState};

/// 一次采样的**完整**产出
///
/// 除了协议要发出的 [`GamepadState`]，还包含**本地反馈需要的**边沿信息：
/// - `edges.*` 让主循环可以判断"要不要打日志/驱动 LED/切换页面"
///
/// 这样 `InputSampler` 既是"帧生产者"也是"UI 事件生产者"，避免上层重复调用 poll。
#[derive(Debug, Clone, Copy, Default)]
pub struct SampleOutput {
  /// 打包好的手柄状态（可以直接送编码器）
  pub state: GamepadState,
  /// 4 个通用按钮的边沿状态
  pub buttons: [ButtonState; 4],
  /// 摇杆按下键的边沿状态
  pub joy_button: ButtonState,
  /// 摇杆连续量读数（含 x/y）
  pub joystick: JoystickReading,
}

/// 输入采样器
///
/// 类型参数：X/Y = 摇杆两个轴的 GPIO 类型；K1/K2 = 两个旋钮的 GPIO 类型。
/// 用泛型是因为 [`AnalogInput`] 需要在类型系统里带上 ADC 通道 —— 但**你不需要手写**：
/// 用 [`InputSampler::new`] 时借助类型推导即可。
pub struct InputSampler<'d, X, Y, K1, K2>
where
  X: AdcChannel,
  Y: AdcChannel,
  K1: AdcChannel,
  K2: AdcChannel,
{
  button_1: Button<'d>,
  button_2: Button<'d>,
  button_3: Button<'d>,
  button_4: Button<'d>,
  joystick: Joystick<'d, X, Y>,
  knob_1: AnalogInput<'d, K1>,
  knob_2: AnalogInput<'d, K2>,
}

impl<'d, X, Y, K1, K2> InputSampler<'d, X, Y, K1, K2>
where
  X: AdcChannel,
  Y: AdcChannel,
  K1: AdcChannel,
  K2: AdcChannel,
{
  /// 构造采样器
  ///
  /// 硬件组件的**电气配置**（active_high、pull、死区）由调用方在构造 hal 组件时就已确定，
  /// 这里只负责聚合，不做二次配置。
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    button_1: Button<'d>,
    button_2: Button<'d>,
    button_3: Button<'d>,
    button_4: Button<'d>,
    joystick: Joystick<'d, X, Y>,
    knob_1: AnalogInput<'d, K1>,
    knob_2: AnalogInput<'d, K2>,
  ) -> Self {
    Self {
      button_1,
      button_2,
      button_3,
      button_4,
      joystick,
      knob_1,
      knob_2,
    }
  }

  /// 采样一次全部硬件，返回 [`SampleOutput`]
  ///
  /// 需要传入共享的 ADC1 引用（4 个 ADC 通道都在 ADC1 上）。
  pub fn poll(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> SampleOutput {
    // 数字输入 —— 一次 poll 拿到当前边沿/稳态
    let b1 = self.button_1.poll();
    let b2 = self.button_2.poll();
    let b3 = self.button_3.poll();
    let b4 = self.button_4.poll();

    // 摇杆（读 X/Y + 按下键）
    let joy = self.joystick.read(adc);

    // 旋钮
    let k1 = self.knob_1.read_normalized(adc);
    let k2 = self.knob_2.read_normalized(adc);

    // 打包成协议状态
    let mut state = GamepadState {
      joy_x: joy.x,
      joy_y: joy.y,
      knob_1: k1,
      knob_2: k2,
      ..GamepadState::EMPTY
    };
    state.set_button(ButtonBits::Btn1, b1.is_down());
    state.set_button(ButtonBits::Btn2, b2.is_down());
    state.set_button(ButtonBits::Btn3, b3.is_down());
    state.set_button(ButtonBits::Btn4, b4.is_down());
    state.set_button(ButtonBits::JoyBtn, joy.is_button_down());

    SampleOutput {
      state,
      buttons: [b1, b2, b3, b4],
      joy_button: joy.button,
      joystick: joy,
    }
  }

  /// **仅用于诊断**：摇杆按下键引脚的原始电平（未经 `active_high` 反转、未消抖）
  pub fn joy_btn_raw_is_high(&self) -> bool {
    self.joystick.button_raw_is_high()
  }
}

/// 本地反馈助手：把按钮状态写入 [`crate::hal::led_effects::BUTTON_LED_STATE`]
///
/// LED 硬件所有权已经转移到 [`crate::hal::led_effects::led_effects_task`]，
/// 主循环不再直接控制 LED。此函数把"按钮 1/2 → LED 1/2"的默认反馈规则
/// 转换成 AtomicU8 位图更新，effect task 会在空闲态定时应用到硬件。
pub fn update_button_led_state(sample: &SampleOutput) {
  use crate::hal::led_effects::set_button_led_state;
  set_button_led_state(sample.buttons[0].is_down(), sample.buttons[1].is_down());
}
