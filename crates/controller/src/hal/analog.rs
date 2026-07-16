//! 通用模拟量输入（ADC 抽象）
//!
//! 供**旋钮**和**摇杆轴**共享的底层组件。特性：
//! - 环形缓冲区做移动平均滤波（默认窗口 4）
//! - 三种归一化输出：
//!   * `read_raw`：原始 0..=4095（含滤波）
//!   * `read_normalized`：0..=AXIS_RANGE（旋钮用）
//!   * `read_centered`：-AXIS_RANGE..=+AXIS_RANGE，含死区（摇杆轴用）
//! - 上电校准 `calibrate`：把机械居中位置的 ADC 实际读数记为
//!   `zero_offset`，补偿摇杆电位器个体偏差（否则静止值不稳定归零）
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

use defmt::warn;
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcChannel, AdcPin};
use esp_hal::peripherals::ADC1;

use crate::config::tuning::{
  ADC_FILTER_WINDOW, ADC_MAX, ADC_MID, AXIS_RANGE, JOYSTICK_CALIBRATION_MAX_OFFSET,
  JOYSTICK_CALIBRATION_SAMPLES,
};

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
  /// 静止中值（ADC 原始值单位）
  ///
  /// 默认 [`ADC_MID`] = 2048（理论中值）。调用 [`AnalogInput::calibrate`]
  /// 后会被实测均值覆盖，用于补偿摇杆电位器机械中心 ≠ 电气中心的个体偏差。
  /// 旋钮的 `read_normalized` 路径不使用此字段。
  zero_offset: u16,
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
      zero_offset: ADC_MID,
    }
  }

  /// 上电校准：以当前实测均值作为 `zero_offset`（要求摇杆此刻居中）
  ///
  /// 连续采样 [`JOYSTICK_CALIBRATION_SAMPLES`] 次求平均，写入 `zero_offset`。
  /// 若均值偏离 [`ADC_MID`] 超过 [`JOYSTICK_CALIBRATION_MAX_OFFSET`]，视为
  /// 用户上电时误推了摇杆，拒绝校准并回退到 [`ADC_MID`]，同时打 warn 日志。
  ///
  /// # 前置条件
  /// - 调用时摇杆物理上必须居中
  /// - 仅对摇杆轴有意义；旋钮不需要（`read_normalized` 不使用 `zero_offset`）
  ///
  /// # 返回
  /// 最终生效的 `zero_offset`（可能是实测均值或 fallback 的 `ADC_MID`）
  pub fn calibrate(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> u16 {
    // 累加 u32 避免 u16 溢出：4095 * 65535 = 2.68e8 < u32::MAX
    let mut sum: u32 = 0;
    for _ in 0..JOYSTICK_CALIBRATION_SAMPLES {
      // 走 read_raw 而非 read_oneshot，让滤波缓冲同步预热
      sum = sum.saturating_add(u32::from(self.read_raw(adc)));
    }
    let mean = (sum / u32::from(JOYSTICK_CALIBRATION_SAMPLES)) as u16;

    // 与理论中值偏差过大 → 拒绝校准
    let deviation = mean.abs_diff(ADC_MID);
    if deviation > JOYSTICK_CALIBRATION_MAX_OFFSET {
      warn!(
        "[ADC] calibration rejected: mean={} deviates {} from ADC_MID={} (max={}); fallback to ADC_MID",
        mean, deviation, ADC_MID, JOYSTICK_CALIBRATION_MAX_OFFSET
      );
      self.zero_offset = ADC_MID;
    } else {
      self.zero_offset = mean;
    }
    self.zero_offset
  }

  /// 返回当前生效的静止中值（主要用于日志/诊断）
  pub fn zero_offset(&self) -> u16 {
    self.zero_offset
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
  ///
  /// # 中心偏差补偿
  /// 使用运行时的 `zero_offset`（可通过 [`AnalogInput::calibrate`] 校准）
  /// 作为中心，而不是理论 [`ADC_MID`]。这样每颗摇杆的电位器个体偏差在启动
  /// 时被吸收，静止读数能稳定归零。
  ///
  /// # 值域对称性
  /// ADC 满量程是 `ADC_MAX = 4095`（不到 4096），因此原始 `delta` 正侧最大
  /// 只有 `ADC_MAX - zero_offset`、负侧最大有 `-zero_offset`，两侧天然不对称。
  /// 缩放分母取**两侧最小可用范围**：`min(zero_offset, ADC_MAX - zero_offset)
  /// - deadzone`，让两侧都能触达 `AXIS_RANGE`，超出部分 clamp。
  pub fn read_centered(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> i16 {
    let raw = self.read_raw(adc);
    let zero = i32::from(self.zero_offset);
    let delta = i32::from(raw) - zero;

    if delta.unsigned_abs() <= u32::from(self.deadzone) {
      return 0;
    }

    let sign: i32 = if delta > 0 { 1 } else { -1 };
    let effective = (delta.unsigned_abs() as i32).saturating_sub(i32::from(self.deadzone));

    // 分母取两侧可用范围的较小值，保证两侧都能触达 AXIS_RANGE。
    // 例如 zero=2100 时：负侧最大 2100，正侧最大 4095-2100=1995 → 用 1995
    // 作为分母，两侧极值都会 clamp 到 AXIS_RANGE，牺牲少许线性度换对称。
    let neg_range = zero;
    let pos_range = i32::from(ADC_MAX) - zero;
    let usable = neg_range.min(pos_range);
    let range_denom = (usable - i32::from(self.deadzone)).max(1);

    let scaled = (effective * i32::from(AXIS_RANGE) / range_denom).min(i32::from(AXIS_RANGE));
    (sign * scaled) as i16
  }
}
