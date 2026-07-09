//! 通用模拟量输入（ADC 抽象）
//!
//! 供**旋钮**和**摇杆轴**共享的底层组件。特性：
//! - 环形缓冲区做移动平均滤波（默认窗口 4）
//! - 三种归一化输出：
//!   * `read_raw`：原始 0..=4095（含滤波）
//!   * `read_normalized`：0..=AXIS_RANGE（旋钮用）
//!   * `read_centered`：-AXIS_RANGE..=+AXIS_RANGE，含死区（摇杆轴用）
//!
//! # 使用
//! ```ignore
//! let mut adc_cfg = AdcConfig::new();
//! let knob_pin = adc_cfg.enable_pin(peripherals.GPIO36, Attenuation::_11dB);
//! let mut adc = Adc::new(peripherals.ADC1, adc_cfg);
//! let mut knob = AnalogInput::new(knob_pin, 0);
//!
//! let n = knob.read_normalized(&mut adc);   // 0..=1000
//! ```

use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcChannel, AdcPin};
use esp_hal::peripherals::ADC1;

use crate::config::tuning::{ADC_FILTER_WINDOW, ADC_MAX, ADC_MID, AXIS_RANGE};

/// 通用模拟量输入（绑定到 ADC1）
///
/// 注：ESP32 上 ADC2 与 Wi-Fi/BLE 冲突，本控制器所有 ADC 引脚都在 ADC1 上。
pub struct AnalogInput<'d, PIN>
where
  PIN: AdcChannel,
{
  pin: AdcPin<PIN, ADC1<'d>>,
  /// 移动平均滤波环形缓冲
  filter_buf: [u16; ADC_FILTER_WINDOW],
  filter_idx: usize,
  /// 缓冲是否已填满（未填满时按已有样本数求平均）
  filter_filled: bool,
  /// 死区（ADC 原始值单位）；旋钮传 0，摇杆传合适值
  deadzone: u16,
}

impl<'d, PIN> AnalogInput<'d, PIN>
where
  PIN: AdcChannel,
{
  /// 构造模拟量输入
  ///
  /// * `pin`      - 已通过 `AdcConfig::enable_pin` 得到的 AdcPin
  /// * `deadzone` - 死区大小（ADC 原始值单位）；旋钮用可传 0
  pub fn new(pin: AdcPin<PIN, ADC1<'d>>, deadzone: u16) -> Self {
    Self {
      pin,
      filter_buf: [ADC_MID; ADC_FILTER_WINDOW],
      filter_idx: 0,
      filter_filled: false,
      deadzone,
    }
  }

  /// 读取原始 ADC 值（含滤波），范围 0..=ADC_MAX
  pub fn read_raw(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> u16 {
    // nb::block! 会一直轮询直到转换完成；ESP32 单次转换约几十微秒
    let sample: u16 = nb::block!(adc.read_oneshot(&mut self.pin)).unwrap_or(ADC_MID);

    // 写入环形缓冲
    self.filter_buf[self.filter_idx] = sample;
    self.filter_idx = (self.filter_idx + 1) % ADC_FILTER_WINDOW;
    if self.filter_idx == 0 {
      self.filter_filled = true;
    }

    // 求平均
    let n = if self.filter_filled {
      ADC_FILTER_WINDOW
    } else {
      self.filter_idx.max(1)
    };
    let sum: u32 = self.filter_buf.iter().take(n).map(|v| u32::from(*v)).sum();
    (sum / n as u32) as u16
  }

  /// 归一化为 0..=AXIS_RANGE（适合旋钮：单向连续量）
  pub fn read_normalized(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> u16 {
    let raw = self.read_raw(adc);
    let scaled = u32::from(raw) * AXIS_RANGE as u32 / u32::from(ADC_MAX);
    scaled.min(AXIS_RANGE as u32) as u16
  }

  /// 归一化为中心化 -AXIS_RANGE..=+AXIS_RANGE，含死区（适合摇杆轴）
  ///
  /// - 偏离中心不到 `deadzone` 时返回 0
  /// - 死区外线性映射到满量程，避免死区边缘的跳变
  pub fn read_centered(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> i16 {
    let raw = self.read_raw(adc);
    let mid = i32::from(ADC_MID);
    let delta = i32::from(raw) - mid;

    if delta.unsigned_abs() <= u32::from(self.deadzone) {
      return 0;
    }

    let sign = if delta > 0 { 1_i32 } else { -1_i32 };
    let effective = delta.unsigned_abs() as i32 - i32::from(self.deadzone);
    let range = mid - i32::from(self.deadzone);
    if range <= 0 {
      return 0;
    }
    let scaled = effective * i32::from(AXIS_RANGE) / range;
    (sign * scaled.min(i32::from(AXIS_RANGE))) as i16
  }
}
