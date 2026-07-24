//! # LED 特效系统
//!
//! ## 职责
//! - 保有 LED1 / LED2 硬件所有权（转移自主循环）
//! - 正常模式下按主循环写入的 [`BUTTON_LED_STATE`]（按键触发）驱动 LED
//! - 收到 [`LedEffect`] 命令时，播放一段闪烁序列覆盖按键状态
//!
//! ## 覆盖模式（Override Mode）
//! ```text
//! 无特效播放中：
//!   led_effects_task  ─── 每 20ms tick ───►
//!     └── 读 BUTTON_LED_STATE            → LED1/LED2 输出
//!
//! 播放特效时：
//!   led_effects_task  ─── 特效序列驱动 ───►
//!     ├── 覆盖 BUTTON_LED_STATE          → LED1/LED2 输出
//!     └── 播完自动回到正常模式
//! ```
//!
//! ## 数据流
//! ```text
//!  main loop     ──update_button_led_state──►  BUTTON_LED_STATE (AtomicU8)
//!  command       ──signal_led_effect────────►  EFFECT_SIGNAL     (LedEffect)
//!  effect task   ◄──────────  Signal + Timer 驱动 ──────────────►  LED HW
//! ```

use core::sync::atomic::{AtomicU8, Ordering};

use defmt::info;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};

use crate::hal::Led;

// ============================================================
// 按键 LED 状态位图（主循环 → effect task）
// ============================================================

/// LED1 位（bit 0）
pub const LED1_BIT: u8 = 0b0000_0001;
/// LED2 位（bit 1）
pub const LED2_BIT: u8 = 0b0000_0010;

/// 主循环写入的 LED 期望状态位图
///
/// - bit 0 = LED1 (1 = 亮, 0 = 灭)
/// - bit 1 = LED2 (1 = 亮, 0 = 灭)
///
/// 由主循环通过 [`crate::input::update_button_led_state`] 定时更新；
/// [`led_effects_task`] 在无特效时读取此值驱动 LED。
pub static BUTTON_LED_STATE: AtomicU8 = AtomicU8::new(0);

/// 便捷 setter：一次性写两个 LED 期望态
pub fn set_button_led_state(led1_on: bool, led2_on: bool) {
  let bits = if led1_on { LED1_BIT } else { 0 } | if led2_on { LED2_BIT } else { 0 };
  BUTTON_LED_STATE.store(bits, Ordering::Relaxed);
}

// ============================================================
// LedEffect —— 一次闪烁特效请求
// ============================================================

/// LED 闪烁特效指令
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedEffect {
  /// 目标 LED 索引：0 = LED1, 1 = LED2
  pub led_idx: u8,
  /// 闪烁次数（1..=255）
  pub count: u8,
  /// 单次周期（毫秒）
  pub period_ms: u16,
}

/// 特效共享通道（Signal = 最后写入者赢）
pub type LedEffectSignal = Signal<CriticalSectionRawMutex, LedEffect>;

/// 全局特效通道（command handler → led_effects_task）
pub static EFFECT_SIGNAL: LedEffectSignal = Signal::new();

/// 便捷入口：请求一次 LED 闪烁特效
///
/// # 参数强制 clamp（M-4 加固）
/// - `count` 至少为 1（<=0 无意义，取 1 保证至少闪一次）
/// - `period_ms` 至少为 50ms —— 避免调用方误传 0 时 `Timer::after(0ms)` 变成
///   紧循环独占 CPU（`fire_low_battery_alert` 等内部调用点绕过了 `execute()`
///   的参数校验，需要在此兜底）
pub fn signal_led_effect(led_idx: u8, count: u8, period_ms: u16) {
  const MIN_PERIOD_MS: u16 = 50;
  EFFECT_SIGNAL.signal(LedEffect {
    led_idx,
    count: count.max(1),
    period_ms: period_ms.max(MIN_PERIOD_MS),
  });
}

// ============================================================
// led_effects_task —— 后台任务
// ============================================================

/// 空闲态刷新周期（毫秒）：主循环写完 BUTTON_LED_STATE 后至少 20ms 内会同步到硬件
const IDLE_REFRESH_MS: u64 = 20;

/// 彩灯（IO15）连续闪烁的半周期（毫秒）：亮 400ms / 灭 400ms ≈ 1.25Hz
const COLOR_BLINK_HALF_MS: u64 = 400;

/// LED 特效后台任务
///
/// # 传参
/// - `led1` / `led2`：LED 硬件所有权（从 main 移交）
/// - `color_led`：彩灯（IO15，4 颗并联 LED）硬件所有权；独立于按键/特效持续闪烁
///
/// # 状态机
/// ```text
/// [Idle]  ── select ──► signal    ──► [Playing]
///           │                          │
///           └── timer (20ms) ──► apply BUTTON_LED_STATE ──► [Idle]
///
/// [Playing] ── for i in 0..count ──► toggle LED, sleep period/2, toggle, sleep period/2
///                                    ──► [Idle]
/// ```
#[embassy_executor::task]
pub async fn led_effects_task(
  mut led1: Led<'static>,
  mut led2: Led<'static>,
  mut color_led: Led<'static>,
) -> ! {
  info!("[LED-FX] Task started");

  loop {
    // 空闲态：等待特效或定时唤醒
    let idle_timer = Timer::after(Duration::from_millis(IDLE_REFRESH_MS));
    match select(EFFECT_SIGNAL.wait(), idle_timer).await {
      Either::First(effect) => {
        // 收到特效：播放
        play_effect(&mut led1, &mut led2, effect).await;
        // 播完立刻同步一次按键态，避免特效期间的按键变化"失踪"
        apply_idle_state(&mut led1, &mut led2);
      }
      Either::Second(()) => {
        // 定时到：把按键期望态同步到 LED
        apply_idle_state(&mut led1, &mut led2);
      }
    }
    // 彩灯（IO15）独立于按键/特效，按固定节拍持续闪烁（特效播放的短暂间隙不更新，无碍）
    apply_color_blink(&mut color_led);
  }
}

/// 彩灯闪烁：用单调时钟算相位，鲁棒于 tick 抖动（亮/灭各 [`COLOR_BLINK_HALF_MS`]）
fn apply_color_blink(color_led: &mut Led<'static>) {
  let phase = (Instant::now().as_millis() / COLOR_BLINK_HALF_MS) % 2;
  color_led.set(phase == 0);
}

/// 空闲态：读 [`BUTTON_LED_STATE`] 应用到 LED
fn apply_idle_state(led1: &mut Led<'static>, led2: &mut Led<'static>) {
  let bits = BUTTON_LED_STATE.load(Ordering::Relaxed);
  led1.set((bits & LED1_BIT) != 0);
  led2.set((bits & LED2_BIT) != 0);
}

/// 播放一段闪烁特效
///
/// # 语义
/// 一次 `count` 表示"完整的亮-灭一个周期"；`period_ms` 是这个完整周期的时长。
/// 内部实现：亮 period/2 → 灭 period/2，重复 count 次。
///
/// # 与主循环的按键状态如何互动？
/// 特效播放期间**完全覆盖**按键状态：即使这段时间用户按了按钮，
/// 也不会立刻反映到对应 LED；播完后自动 `apply_idle_state` 恢复。
async fn play_effect(led1: &mut Led<'static>, led2: &mut Led<'static>, effect: LedEffect) {
  info!(
    "[LED-FX] play: led={} count={} period={}ms",
    effect.led_idx, effect.count, effect.period_ms
  );

  let half_period = Duration::from_millis((effect.period_ms as u64) / 2);

  for _ in 0..effect.count {
    // 亮
    set_target(led1, led2, effect.led_idx, true);
    Timer::after(half_period).await;
    // 灭
    set_target(led1, led2, effect.led_idx, false);
    Timer::after(half_period).await;
  }
}

/// 按 led_idx 选择性写 LED
fn set_target(led1: &mut Led<'static>, led2: &mut Led<'static>, led_idx: u8, on: bool) {
  match led_idx {
    0 => led1.set(on),
    1 => led2.set(on),
    _ => {} // 参数校验在 control::execute_command 内已做，理论不会到这里
  }
}
