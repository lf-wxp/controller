//! 拨动开关（两态稳态输入）
//!
//! 与按钮的区别：拨动开关不需要边沿事件和精细消抖，只关心"当前是开还是关"。
//! 但为了避免拨动瞬间抖动被读到，仍做一次轻量消抖（默认与按钮相同）。

use embassy_time::{Duration, Instant};
use esp_hal::gpio::Input;

use crate::config::tuning::DEBOUNCE_MS;

/// 拨动开关
pub struct Switch<'d> {
  pin: Input<'d>,
  /// 开时电平：true = 高电平为开，false = 低电平为开
  active_high: bool,
  last_stable: bool,
  last_raw: bool,
  last_change: Instant,
}

impl<'d> Switch<'d> {
  /// 构造开关
  ///
  /// * `pin`         - 已配置好的 Input
  /// * `active_high` - 高电平代表"开"则传 true
  /// * `initial_on`  - 冷启动时的假定初值（真实值会在首次 poll 后立即修正）
  pub fn new(pin: Input<'d>, active_high: bool, initial_on: bool) -> Self {
    Self {
      pin,
      active_high,
      last_stable: initial_on,
      last_raw: initial_on,
      last_change: Instant::now(),
    }
  }

  /// 周期采样，返回当前稳态是否为"开"
  pub fn poll(&mut self) -> bool {
    let raw_high = self.pin.is_high();
    let on_now = if self.active_high {
      raw_high
    } else {
      !raw_high
    };

    if on_now != self.last_raw {
      self.last_raw = on_now;
      self.last_change = Instant::now();
      return self.last_stable;
    }

    let debounced = Instant::now() - self.last_change >= Duration::from_millis(DEBOUNCE_MS);
    if debounced {
      self.last_stable = on_now;
    }
    self.last_stable
  }

  /// 当前稳态（不重新采样）
  pub fn is_on(&self) -> bool {
    self.last_stable
  }

  /// **仅用于诊断**：直接返回 GPIO 引脚的原始电平（未经 active_high 反转、未消抖）
  ///
  /// 用于验证硬件接线极性与 `active_high` 配置是否一致。生产代码请勿依赖此方法。
  pub fn raw_is_high(&self) -> bool {
    self.pin.is_high()
  }
}
