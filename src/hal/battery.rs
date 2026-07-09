//! # 电池电量监测
//!
//! ## 职责
//! - 定期采样电池电压（可以是真实 ADC 读取或模拟递减）
//! - 电压 → 百分比换算
//! - 滑动平均滤波（避免瞬时电流波动误报）
//! - 结果写入 [`crate::ui::BATTERY_LEVEL`]，UI 与 BLE 同时看到
//!
//! ## 硬件接线（真实模式）
//! ```text
//!  VBAT ──[R1=100kΩ]──┬── GPIO34  (ADC1_CH6)
//!                     │
//!                    [R2=100kΩ]
//!                     │
//!                    GND
//! ```
//! 分压比 1/2：
//! - 电池 4.20V (100%) → GPIO34 = 2.10V
//! - 电池 3.30V (  0%) → GPIO34 = 1.65V
//!
//! ADC 配 `Attenuation::_11dB` 时满量程约 3.3V，测量范围足够覆盖锂电池全程。
//!
//! ## 模拟模式
//! 当 [`config::battery::SIMULATE`] = true 时，无需连接分压硬件，每次调用
//! `sample()` 电量 -1%，到 0 后回到 100 —— 用于**验收 UI 与 BLE 电量通路**。
//!
//! ## 换算算法
//! 采用**分段线性查表**贴合锂电池 (1S 3.7V) 实际放电曲线。
//! 曲线数据见 [`LI_ION_DISCHARGE_CURVE`]，共 15 个数据点覆盖 0..=100%。
//! 相邻两点之间采用线性插值，兼顾精度与实现简洁。
//!
//! 相比早期的"线性映射 3.30V..4.20V → 0..100%"，分段查表在中段电压
//! （3.7~4.0V，锂电池的**平台期**）给出更贴近真实电量的读数，避免用户
//! "感觉一直满电、突然没电"的心理落差。

use core::sync::atomic::{AtomicU8, Ordering};

use defmt::{debug, info};
use embassy_time::{Duration, Timer};
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcChannel, AdcPin};
use esp_hal::peripherals::ADC1;

use crate::config::battery::{
  ADC_VREF_V, BATTERY_MAX_V, BATTERY_MIN_V, DIVIDER_RATIO, FILTER_WINDOW, SAMPLE_INTERVAL_MS,
  SIMULATE,
};
use crate::config::tuning::ADC_MAX;
use crate::ui::set_battery_level;

// ============================================================
// BatteryLevel —— 电量分级（L 选项：低电量告警）
// ============================================================

/// 电量分级
///
/// # 阈值（可由 [`classify_battery`] 反向查表）
/// | 分级       | 百分比范围 | UI 表现              | LED 表现        |
/// |------------|-----------|---------------------|-----------------|
/// | `Normal`   | >= 20%    | 常规显示            | 正常（按键反馈）|
/// | `Low`      | 10..20%   | 电量图标闪烁 (1Hz)  | LED2 慢闪 (2Hz) |
/// | `Critical` | 5..10%    | 全屏边框闪烁 (2Hz)  | 双 LED 交替闪   |
/// | `Empty`    | < 5%      | 强制 Toast + 边框    | 双 LED 快闪     |
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryLevel {
  /// 正常电量（>= 20%）
  Normal = 0,
  /// 低电量（10..20%）
  Low = 1,
  /// 危急电量（5..10%）
  Critical = 2,
  /// 空电量（< 5%）
  Empty = 3,
}

impl BatteryLevel {
  /// 判断当前分级是否属于"需要告警"
  #[inline]
  pub const fn is_alert(self) -> bool {
    !matches!(self, Self::Normal)
  }

  /// 从 wire 字节反查（用于持久化或跨模块传值）
  ///
  /// 未知字节返回 [`BatteryLevel::Normal`]（安全默认值）。
  pub const fn from_u8(byte: u8) -> Self {
    match byte {
      1 => Self::Low,
      2 => Self::Critical,
      3 => Self::Empty,
      _ => Self::Normal,
    }
  }
}

impl defmt::Format for BatteryLevel {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::Normal => defmt::write!(f, "BatteryLevel::Normal"),
      Self::Low => defmt::write!(f, "BatteryLevel::Low"),
      Self::Critical => defmt::write!(f, "BatteryLevel::Critical"),
      Self::Empty => defmt::write!(f, "BatteryLevel::Empty"),
    }
  }
}

/// 百分比 → 电量分级
///
/// # 算法
/// 单向阈值查表：
/// - `< 5`   → `Empty`
/// - `< 10`  → `Critical`
/// - `< 20`  → `Low`
/// - 其它    → `Normal`
///
/// **不做迟滞（hysteresis）**：从 Low 恢复到 Normal 时立即切换。若未来发现
/// 在阈值附近抖动（比如 19% ↔ 20% 反复告警），可以在此加入 ±2% 迟滞窗口。
pub const fn classify_battery(percent: u8) -> BatteryLevel {
  if percent < 5 {
    BatteryLevel::Empty
  } else if percent < 10 {
    BatteryLevel::Critical
  } else if percent < 20 {
    BatteryLevel::Low
  } else {
    BatteryLevel::Normal
  }
}

/// 电量分级的全局共享状态（UI / LED / 主循环都读它）
///
/// 由 [`battery_monitor_simulated_task`] 每次采样后更新。使用 `AtomicU8`
/// 便于跨任务无锁读取；实际值只会取自 [`BatteryLevel`] 的 `#[repr(u8)]`
/// discriminant，读取端用 [`BatteryLevel::from_u8`] 反查。
pub static BATTERY_LEVEL_STATE: AtomicU8 = AtomicU8::new(BatteryLevel::Normal as u8);

/// 便捷 getter：读当前电量分级
#[inline]
pub fn battery_level_state() -> BatteryLevel {
  BatteryLevel::from_u8(BATTERY_LEVEL_STATE.load(Ordering::Relaxed))
}

/// 内部：写当前电量分级（返回值指示是否**新变化**）
///
/// 返回 `Some(new_level)` 表示分级发生了变化——调用方可据此触发 Toast/LED；
/// 返回 `None` 表示分级不变，不必产生副作用。
fn update_battery_level_state(new_level: BatteryLevel) -> Option<BatteryLevel> {
  let old = BATTERY_LEVEL_STATE.swap(new_level as u8, Ordering::Relaxed);
  if old == new_level as u8 {
    None
  } else {
    Some(new_level)
  }
}

// ============================================================
// BatteryMonitor —— 抽象接口
// ============================================================

/// 电池监测统一接口
///
/// 两个实现：[`RealBatteryMonitor`]（读 ADC）与 [`SimulatedBatteryMonitor`]（模拟递减）。
/// 通过 [`sample_and_update`] 便捷函数即可屏蔽 monomorphization 差异。
pub trait BatteryMonitor {
  /// 采样一次，返回百分比 0..=100
  fn sample(&mut self) -> u8;
}

// ============================================================
// RealBatteryMonitor —— 真实 ADC 读取
// ============================================================

/// 通过 ADC1 分压电路测量电池电压
///
/// # 泛型参数
/// `PIN` 必须是 ADC1 上的一个引脚（`GPIO34` 是推荐位置，未被 Wi-Fi 冲突）。
pub struct RealBatteryMonitor<'d, PIN>
where
  PIN: AdcChannel,
{
  pin: AdcPin<PIN, ADC1<'d>>,
  /// 环形缓冲：最近 N 次采样电压
  filter_buf: [f32; FILTER_WINDOW],
  filter_idx: usize,
  filter_filled: bool,
}

impl<'d, PIN> RealBatteryMonitor<'d, PIN>
where
  PIN: AdcChannel,
{
  /// 构造真实电池监测器
  ///
  /// * `pin` —— 已通过 `AdcConfig::enable_pin` 得到的 AdcPin（推荐 GPIO34）
  pub fn new(pin: AdcPin<PIN, ADC1<'d>>) -> Self {
    Self {
      pin,
      filter_buf: [BATTERY_MAX_V; FILTER_WINDOW],
      filter_idx: 0,
      filter_filled: false,
    }
  }

  /// 采样一次并返回百分比；需要传入 ADC 句柄（同 `AnalogInput` 一致）
  pub fn sample(&mut self, adc: &mut Adc<'d, ADC1<'d>, Blocking>) -> u8 {
    // 1) 读 ADC 原始值
    let raw: u16 = nb::block!(adc.read_oneshot(&mut self.pin)).unwrap_or(0);

    // 2) 原始值 → ADC 引脚电压
    let pin_voltage = f32::from(raw) * ADC_VREF_V / f32::from(ADC_MAX);

    // 3) ADC 引脚电压 → 实际电池电压（乘分压系数）
    let vbat = pin_voltage * DIVIDER_RATIO;

    // 4) 写入环形缓冲
    self.filter_buf[self.filter_idx] = vbat;
    self.filter_idx = (self.filter_idx + 1) % FILTER_WINDOW;
    if self.filter_idx == 0 {
      self.filter_filled = true;
    }

    // 5) 求平均
    let n = if self.filter_filled {
      FILTER_WINDOW
    } else {
      self.filter_idx.max(1)
    };
    let sum: f32 = self.filter_buf.iter().take(n).sum();
    let avg_v = sum / n as f32;

    // 6) 电压 → 百分比
    voltage_to_percent(avg_v)
  }
}

// ============================================================
// SimulatedBatteryMonitor —— 无硬件时的模拟递减
// ============================================================

/// 模拟电池：每次采样电量 -1%，到 0 后回到 100
///
/// 用于**无 VBAT 分压硬件**时验收 UI / BLE 电量通路。
pub struct SimulatedBatteryMonitor {
  level: u8,
}

impl Default for SimulatedBatteryMonitor {
  fn default() -> Self {
    Self { level: 100 }
  }
}

impl SimulatedBatteryMonitor {
  /// 从满电 100% 开始
  pub const fn new() -> Self {
    Self { level: 100 }
  }
}

impl BatteryMonitor for SimulatedBatteryMonitor {
  fn sample(&mut self) -> u8 {
    let current = self.level;
    self.level = if current == 0 { 100 } else { current - 1 };
    current
  }
}

// ============================================================
// 电压 → 百分比（分段线性查表）
// ============================================================

/// 放电曲线上的一个数据点：某个电压对应的 SoC 百分比
///
/// 曲线数据存放在 [`LI_ION_DISCHARGE_CURVE`] 里，必须**按 voltage 从高到低排列**。
#[derive(Debug, Clone, Copy)]
pub struct SocPoint {
  /// 电池电压（伏特）
  pub voltage: f32,
  /// 对应的 State-of-Charge 百分比（0..=100）
  pub percent: u8,
}

/// 单节锂离子电池 (1S 3.7V) 典型放电曲线
///
/// # 数据来源
/// 综合业界（Battery University / 常见 18650 数据手册）在**室温、~0.2C 放电**
/// 条件下的经验数据。不同厂家/化学配方（LiPo vs LiFePO4）曲线略有差异，若
/// 需要更换电池请同步替换此数组。
///
/// # 曲线特征
/// - **顶部（4.20~4.10V）陡峭**：满电后电压快速下降到平台
/// - **中段（4.00~3.85V）平缓**：进入平台期，电压变化不明显但耗电大量
/// - **底部（3.70~3.30V）加速**：临近截止电压时快速掉压
///
/// # 排序约束
/// 数组必须按 `voltage` **从高到低**严格递减；`percent` 相应从 100 单调递减到 0。
const LI_ION_DISCHARGE_CURVE: &[SocPoint] = &[
  SocPoint {
    voltage: 4.20,
    percent: 100,
  },
  SocPoint {
    voltage: 4.15,
    percent: 95,
  },
  SocPoint {
    voltage: 4.11,
    percent: 90,
  },
  SocPoint {
    voltage: 4.08,
    percent: 85,
  },
  SocPoint {
    voltage: 4.02,
    percent: 80,
  },
  SocPoint {
    voltage: 3.98,
    percent: 70,
  },
  SocPoint {
    voltage: 3.95,
    percent: 60,
  },
  SocPoint {
    voltage: 3.91,
    percent: 50,
  },
  SocPoint {
    voltage: 3.87,
    percent: 40,
  },
  SocPoint {
    voltage: 3.85,
    percent: 30,
  },
  SocPoint {
    voltage: 3.84,
    percent: 20,
  },
  SocPoint {
    voltage: 3.82,
    percent: 17,
  },
  SocPoint {
    voltage: 3.80,
    percent: 14,
  },
  SocPoint {
    voltage: 3.73,
    percent: 10,
  },
  SocPoint {
    voltage: 3.60,
    percent: 5,
  },
  SocPoint {
    voltage: BATTERY_MIN_V,
    percent: 0,
  }, // 3.30V
];

/// 电池电压（V）→ 百分比（0..=100）
///
/// # 算法
/// 分段线性插值：在 [`LI_ION_DISCHARGE_CURVE`] 相邻两点之间线性插值。
/// - 电压 ≥ 曲线首点 → 100%
/// - 电压 ≤ 曲线末点 → 0%
/// - 落在两点之间 → 线性插值
///
/// # 复杂度
/// O(n)，n = 曲线点数（当前 16 个），单次调用微秒级；电池采样本身是 5 秒
/// 一次，性能完全无压力。
///
/// # 精度
/// 输出 `u8` 精度即 1%；分段插值在锂电池平台期 (3.85~3.98V) 能显著改善
/// 早期线性映射"平台期读数虚高"的问题。
pub fn voltage_to_percent(voltage: f32) -> u8 {
  // 边界快速返回：曲线首点/末点之外
  let first = LI_ION_DISCHARGE_CURVE[0];
  if voltage >= first.voltage {
    return first.percent;
  }
  let last = LI_ION_DISCHARGE_CURVE[LI_ION_DISCHARGE_CURVE.len() - 1];
  if voltage <= last.voltage {
    return last.percent;
  }

  // 逐段查找：曲线按 voltage 递减，找到第一个包含 voltage 的区间 [lo.voltage, hi.voltage]
  for pair in LI_ION_DISCHARGE_CURVE.windows(2) {
    let hi = pair[0]; // 电压更高的一端
    let lo = pair[1]; // 电压更低的一端
    if voltage <= hi.voltage && voltage >= lo.voltage {
      // 线性插值：percent = lo.percent + ratio * (hi.percent - lo.percent)
      let dv = hi.voltage - lo.voltage;
      // dv 不会为 0：曲线设计上 voltage 严格递减；防御性判断防止未来手误
      if dv <= 0.0 {
        return lo.percent;
      }
      let ratio = (voltage - lo.voltage) / dv;
      let dp = f32::from(hi.percent) - f32::from(lo.percent);
      let pct = f32::from(lo.percent) + ratio * dp;
      // C-4 加固：任何浮点异常（NaN / ±Inf）都回退到区间下界 lo.percent，
      // 避免 `NaN as u8 → 0` 导致误报低电量告警。
      if !pct.is_finite() {
        return lo.percent;
      }
      return pct.clamp(0.0, 100.0) as u8;
    }
  }

  // 不可达：所有电压都应命中上面的分支之一
  0
}

// ============================================================
// battery_monitor_task —— 后台任务（模拟版本）
// ============================================================

/// 模拟版电池监测后台任务
///
/// 用于当前**无实际测量硬件**的场景：每 [`SAMPLE_INTERVAL_MS`] 递减 1%，
/// 到 0 后回到 100。结果写入 [`crate::ui::BATTERY_LEVEL`]，UI 与 BLE 同时看到。
///
/// # 为什么单独做一个 task 而不是塞进主循环？
/// - 主循环频率 100 Hz，电量采样只需要 0.2 Hz，两者分开
/// - 未来接入真实 ADC 后不影响主循环时序
///
/// # 真实硬件模式
/// 接入 GPIO34 分压后：
/// 1. 把 [`SIMULATE`] 改为 `false`
/// 2. 改 spawn 语句用 [`battery_monitor_real_task`]（下面）并传入 ADC + AdcPin
///
/// # L 选项：低电量告警
/// 每次采样后会调用 [`classify_battery`] 得到分级；若分级发生变化则通过
/// [`fire_low_battery_alert`] 触发 UI Toast + LED 慢闪。
#[embassy_executor::task]
pub async fn battery_monitor_simulated_task() -> ! {
  info!("[BAT] Simulated battery monitor started");
  let mut monitor = SimulatedBatteryMonitor::new();
  loop {
    let level = monitor.sample();
    // Ordering::Relaxed 即可：UI/BLE 只是展示用
    debug!("[BAT] simulated level = {}%", level);
    set_battery_level_relaxed(level);

    // L 选项：分级 + 告警触发
    let new_level = classify_battery(level);
    if let Some(changed) = update_battery_level_state(new_level) {
      fire_low_battery_alert(level, changed);
    }

    Timer::after(Duration::from_millis(SAMPLE_INTERVAL_MS)).await;
  }
}

/// 内部包装：写入 [`crate::ui::BATTERY_LEVEL`]
///
/// 使用 `Relaxed` 因为电量只是展示用途，读到旧值最多晚一次刷新，无关正确性。
fn set_battery_level_relaxed(level: u8) {
  // set_battery_level 内部已经用 Relaxed
  set_battery_level(level);
  // silence dead_code warning for Ordering import if not used elsewhere
  let _ = Ordering::Relaxed;
}

/// 当电量分级发生变化时触发告警副作用
///
/// # 参数
/// - `percent`：当前电量百分比（0..=100）——用于 Toast 文本
/// - `level`：新的电量分级
///
/// # 副作用
/// - **Toast**：底部弹出短提示（"LOW"/"CRIT"/"DEAD" 之一）
/// - **LED 特效**：LED2 慢闪 / 双闪，具体见分级说明
///
/// # 不告警的情况
/// `BatteryLevel::Normal` 时**只**发一次"BAT OK"提示（比如从 Low 恢复到 Normal
/// 时的正反馈），不触发 LED 特效。
fn fire_low_battery_alert(percent: u8, level: BatteryLevel) {
  use crate::hal::led_effects::signal_led_effect;
  use crate::ui::signal_toast;

  info!("[BAT] level changed: {}% -> {}", percent, level);

  match level {
    BatteryLevel::Normal => {
      // 恢复到正常：给用户一个正反馈
      signal_toast(b"BAT+");
    }
    BatteryLevel::Low => {
      // 20% 以下：Toast + LED2 慢闪 2 次（周期 500ms）
      signal_toast(b"LOW");
      signal_led_effect(
        /* led_idx = LED2 */ 1, /* count */ 2, /* period_ms */ 500,
      );
    }
    BatteryLevel::Critical => {
      // 10% 以下：Toast + LED2 中速闪 4 次（周期 300ms）
      signal_toast(b"CRIT");
      signal_led_effect(1, 4, 300);
    }
    BatteryLevel::Empty => {
      // 5% 以下：Toast + LED1+LED2 都快闪（每 200ms 交替）
      signal_toast(b"DEAD");
      signal_led_effect(0, 6, 200);
      signal_led_effect(1, 6, 200);
    }
  }
}

/// 编译期表明当前是否走模拟模式（供 main.rs 判断 spawn 哪个 task）
///
/// 单纯 re-export 便于调用方 `use controller::hal::battery::IS_SIMULATED`。
pub const IS_SIMULATED: bool = SIMULATE;

#[cfg(test)]
mod tests {
  use super::*;

  /// 允许 ±1% 的舍入误差
  fn assert_close(actual: u8, expected: u8) {
    let diff = actual.abs_diff(expected);
    assert!(diff <= 1, "expected {}% (±1), got {}%", expected, actual);
  }

  #[test]
  fn endpoint_full() {
    assert_eq!(voltage_to_percent(4.20), 100);
    assert_eq!(voltage_to_percent(4.50), 100); // 超上限也 clamp 到 100
  }

  #[test]
  fn endpoint_empty() {
    assert_eq!(voltage_to_percent(BATTERY_MIN_V), 0);
    assert_eq!(voltage_to_percent(2.50), 0); // 低于保护电压也 clamp 到 0
  }

  #[test]
  fn exact_curve_points() {
    // 曲线上的原始点应精确返回自身
    for point in LI_ION_DISCHARGE_CURVE {
      assert_close(voltage_to_percent(point.voltage), point.percent);
    }
  }

  #[test]
  fn plateau_is_nonlinear() {
    // 平台期：4.00V 应落在 70~80% 之间，符合真实电池表现
    // （旧线性映射会给出 (4.00-3.30)/0.90 * 100 ≈ 77%，看似接近但曲线含义不同）
    let pct = voltage_to_percent(4.00);
    assert!(
      pct >= 70 && pct <= 85,
      "4.00V should map to 70~85%, got {}",
      pct
    );
  }

  #[test]
  fn interpolation_between_points() {
    // 4.15V (95%) 与 4.11V (90%) 之间的中点约 4.13V 应约 92~93%
    let pct = voltage_to_percent(4.13);
    assert!(
      pct >= 91 && pct <= 94,
      "4.13V should interpolate to ~92%, got {}",
      pct
    );
  }

  #[test]
  fn monotonic_decreasing() {
    // 电压递减时百分比也应单调不增
    let mut prev = 101_u8;
    let voltages = [4.20, 4.10, 4.00, 3.90, 3.85, 3.80, 3.70, 3.60, 3.40, 3.30];
    for v in voltages {
      let pct = voltage_to_percent(v);
      assert!(
        pct <= prev,
        "non-monotonic at {}V: prev={}, now={}",
        v,
        prev,
        pct
      );
      prev = pct;
    }
  }

  // ---- L 选项：电量分级测试 ----

  #[test]
  fn classify_battery_ranges() {
    // Empty: < 5
    assert_eq!(classify_battery(0), BatteryLevel::Empty);
    assert_eq!(classify_battery(4), BatteryLevel::Empty);
    // Critical: 5..10
    assert_eq!(classify_battery(5), BatteryLevel::Critical);
    assert_eq!(classify_battery(9), BatteryLevel::Critical);
    // Low: 10..20
    assert_eq!(classify_battery(10), BatteryLevel::Low);
    assert_eq!(classify_battery(19), BatteryLevel::Low);
    // Normal: >= 20
    assert_eq!(classify_battery(20), BatteryLevel::Normal);
    assert_eq!(classify_battery(50), BatteryLevel::Normal);
    assert_eq!(classify_battery(100), BatteryLevel::Normal);
  }

  #[test]
  fn battery_level_is_alert() {
    assert!(!BatteryLevel::Normal.is_alert());
    assert!(BatteryLevel::Low.is_alert());
    assert!(BatteryLevel::Critical.is_alert());
    assert!(BatteryLevel::Empty.is_alert());
  }

  #[test]
  fn battery_level_from_u8_safe_default() {
    // 未知字节应回退到 Normal（安全默认值，避免误告警）
    assert_eq!(BatteryLevel::from_u8(0), BatteryLevel::Normal);
    assert_eq!(BatteryLevel::from_u8(1), BatteryLevel::Low);
    assert_eq!(BatteryLevel::from_u8(2), BatteryLevel::Critical);
    assert_eq!(BatteryLevel::from_u8(3), BatteryLevel::Empty);
    assert_eq!(BatteryLevel::from_u8(255), BatteryLevel::Normal);
  }

  #[test]
  fn update_battery_level_state_returns_none_when_unchanged() {
    // 手动重置到 Normal，然后再次 update 到 Normal → 应返回 None
    BATTERY_LEVEL_STATE.store(BatteryLevel::Normal as u8, Ordering::Relaxed);
    let changed = update_battery_level_state(BatteryLevel::Normal);
    assert!(changed.is_none());
  }

  #[test]
  fn update_battery_level_state_returns_some_on_change() {
    BATTERY_LEVEL_STATE.store(BatteryLevel::Normal as u8, Ordering::Relaxed);
    let changed = update_battery_level_state(BatteryLevel::Low);
    assert_eq!(changed, Some(BatteryLevel::Low));
    // 二次调用同值不再返回 Some
    let unchanged = update_battery_level_state(BatteryLevel::Low);
    assert!(unchanged.is_none());
  }
}
