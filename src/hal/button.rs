//! 通用数字按钮 + 软件消抖
//!
//! 特性：
//! - 支持"按下拉低"（active_low）和"按下拉高"（active_high）两种电气模式
//! - 时间戳消抖：不阻塞，异步安全
//! - 提供边沿检测（just_pressed / just_released）

use embassy_time::{Duration, Instant};
use esp_hal::gpio::Input;

use crate::config::tuning::DEBOUNCE_MS;

/// 按钮实时状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonState {
  /// 未按下（稳态）
  #[default]
  Released,
  /// 已按下（稳态）
  Pressed,
  /// 本次采样刚刚从 Released → Pressed（边沿）
  JustPressed,
  /// 本次采样刚刚从 Pressed → Released（边沿）
  JustReleased,
}

impl ButtonState {
  /// 是否处于按下状态（Pressed / JustPressed）
  pub const fn is_down(self) -> bool {
    matches!(self, Self::Pressed | Self::JustPressed)
  }
}

#[derive(Debug, Clone, Copy)]
enum Edge {
  Rising,
  Falling,
}

/// 通用数字按钮
///
/// 通过 `poll()` 周期性采样（推荐 10ms/次）。内部使用时间戳法消抖，
/// 不会阻塞异步执行器。
pub struct Button<'d> {
  pin: Input<'d>,
  /// 按下时电平：true = 高电平按下，false = 低电平按下
  active_high: bool,
  /// 消抖后的稳定状态
  last_stable: bool,
  /// 上次原始采样电平
  last_raw: bool,
  /// 上次原始电平变化的时间戳
  last_change: Instant,
  /// 本轮 poll 检测到的边沿
  edge: Option<Edge>,
}

impl<'d> Button<'d> {
  /// 构造按钮
  ///
  /// * `pin`         - 已配置好的 Input（应设好合适的 Pull）
  /// * `active_high` - 按下时电平为高则传 true；按下拉低则传 false
  pub fn new(pin: Input<'d>, active_high: bool) -> Self {
    Self {
      pin,
      active_high,
      last_stable: false,
      last_raw: false,
      last_change: Instant::now(),
      edge: None,
    }
  }

  /// 采样并更新内部状态，返回本次状态
  ///
  /// 需要周期性调用（推荐 10ms/次）。
  pub fn poll(&mut self) -> ButtonState {
    let raw_high = self.pin.is_high();
    let pressed_now = if self.active_high {
      raw_high
    } else {
      !raw_high
    };

    // 原始电平变化 → 记录时间戳，不改变稳态
    if pressed_now != self.last_raw {
      self.last_raw = pressed_now;
      self.last_change = Instant::now();
      self.edge = None;
      return self.state();
    }

    // 电平稳定超过消抖时长 → 更新稳态
    let debounced = Instant::now() - self.last_change >= Duration::from_millis(DEBOUNCE_MS);
    if debounced && pressed_now != self.last_stable {
      self.edge = Some(if pressed_now {
        Edge::Rising
      } else {
        Edge::Falling
      });
      self.last_stable = pressed_now;
    } else {
      self.edge = None;
    }

    self.state()
  }

  /// 当前状态（不重新采样）
  pub fn state(&self) -> ButtonState {
    match (self.last_stable, self.edge) {
      (_, Some(Edge::Rising)) => ButtonState::JustPressed,
      (_, Some(Edge::Falling)) => ButtonState::JustReleased,
      (true, None) => ButtonState::Pressed,
      (false, None) => ButtonState::Released,
    }
  }

  /// 便捷方法：当前是否处于按下稳态
  pub fn is_pressed(&self) -> bool {
    self.last_stable
  }
}
