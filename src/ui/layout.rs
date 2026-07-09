//! # UI 布局与绘制
//!
//! 纯函数式绘制：给定 [`UiState`] 与可绘制目标（`&mut DrawTarget`），
//! 计算所有元素位置并画出来。不涉及硬件、不涉及异步。
//!
//! ## 屏幕布局（128×64，字体 6×10）
//! ```text
//! 行 0 (y=0)  : ESP32-Ctrl        BLE:● NOW:● 99%
//! 行 1 (y=10) : ─────────────────────────
//! 行 2 (y=20) : Joy X=+1000  Y=-1000
//! 行 3 (y=30) : Kn  K1=1000 K2=1000
//! 行 4 (y=40) : Btn 1234 J S    (点亮的位打成实心方块)
//! 行 5 (y=50) : Seq 0x0000ABCD 30Hz
//! ```

use core::fmt::Write as _;

use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Line, PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};
use heapless::String;

use crate::config::display::{LINE_H, OLED_HEIGHT, OLED_WIDTH};
use crate::hal::battery::BatteryLevel;
use crate::protocol::state::ButtonBits;

use super::{Toast, UiState};

/// 单行最大字符缓冲：留一些余量避免 `write!` 溢出
type LineBuf = String<32>;

/// 把 [`UiState`] 完整绘制到 target（先清屏再逐行绘制）
///
/// # 返回值
/// - `Ok(())`：所有绘制操作成功
/// - `Err(D::Error)`：底层 `DrawTarget` 报错（I²C 断线等）
pub fn render<D>(target: &mut D, state: &UiState) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  // ---- 清屏 ----
  target.clear(BinaryColor::Off)?;

  let style = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);

  // ---- 第 0 行：标题 + 连接状态 + 电量 ----
  draw_header(target, style, state)?;

  // ---- 第 1 行：分割线 ----
  let sep_y = LINE_H + 1;
  Line::new(
    Point::new(0, sep_y),
    Point::new(OLED_WIDTH as i32 - 1, sep_y),
  )
  .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
  .draw(target)?;

  // ---- 第 2 行：Joy ----
  let mut buf = LineBuf::new();
  let _ = write!(
    &mut buf,
    "Joy X={:+05} Y={:+05}",
    state.frame.payload.joy_x, state.frame.payload.joy_y,
  );
  draw_text(target, style, &buf, 0, LINE_H * 2 + 2)?;

  // ---- 第 3 行：Knob ----
  buf.clear();
  let _ = write!(
    &mut buf,
    "Kn  K1={:04} K2={:04}",
    state.frame.payload.knob_1, state.frame.payload.knob_2,
  );
  draw_text(target, style, &buf, 0, LINE_H * 3 + 2)?;

  // ---- 第 4 行：Buttons ----
  draw_buttons(target, style, state, LINE_H * 4 + 2)?;

  // ---- 第 5 行：Seq + TX 频率 ----
  buf.clear();
  let _ = write!(&mut buf, "Seq 0x{:08X}  30Hz", state.frame.header.seq);
  draw_text(target, style, &buf, 0, LINE_H * 5 + 2)?;

  // ---- L 选项：低电量告警边框（Toast 之前绘制，让 Toast 覆盖在边框之上）----
  if state.battery_level.is_alert() {
    draw_low_battery_border(target, state.battery_level)?;
  }

  // ---- Toast 覆盖层（若存在则遮住底部两行）----
  if let Some(toast) = state.toast.as_ref() {
    draw_toast(target, style, toast)?;
  }

  Ok(())
}

/// 顶部标题栏：`ESP32-Ctrl BLE:● NOW:● 99%`
fn draw_header<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
  state: &UiState,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  // 左半：缩短的设备名（3 chars = 18 px），为 H 指示位腾出空间
  draw_text(target, style, "Ctl", 0, 0)?;

  // 中间：BLE / NOW / Heartbeat 三个状态灯
  // 每组 = label(6px) + dot(5px) + 间隙(2px) = 13px；三组 39px
  let base_x = 22;

  draw_text(target, style, "B", base_x, 0)?;
  draw_status_dot(target, base_x + 6, 3, state.ble_connected)?;

  draw_text(target, style, "N", base_x + 13, 0)?;
  draw_status_dot(target, base_x + 19, 3, state.esp_now_ready)?;

  draw_text(target, style, "H", base_x + 26, 0)?;
  draw_status_dot(target, base_x + 32, 3, state.host_heartbeat_alive)?;

  // 电量文字（右对齐到 128px 边缘）
  let mut buf = LineBuf::new();
  let _ = write!(&mut buf, "{:3}%", state.battery);
  let text_w = buf.len() as i32 * 6;
  let x = OLED_WIDTH as i32 - text_w;
  draw_text(target, style, &buf, x, 0)?;

  Ok(())
}

/// 画一个 4×4 的状态方块：实心 = 连接、空心 = 未连接
fn draw_status_dot<D>(target: &mut D, x: i32, y: i32, filled: bool) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  let style = if filled {
    PrimitiveStyle::with_fill(BinaryColor::On)
  } else {
    PrimitiveStyle::with_stroke(BinaryColor::On, 1)
  };
  Rectangle::new(Point::new(x, y), Size::new(5, 5))
    .into_styled(style)
    .draw(target)?;
  Ok(())
}

/// 按钮行：`Btn 1 2 3 4 J S`（按下的按钮加方框）
fn draw_buttons<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
  state: &UiState,
  y: i32,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  draw_text(target, style, "Btn", 0, y)?;

  // 6 个按钮：Btn1..Btn4 + JoyBtn + Switch
  // 每个占 12 像素宽（字符 + 间距）
  let labels: [(ButtonBits, &str); 6] = [
    (ButtonBits::Btn1, "1"),
    (ButtonBits::Btn2, "2"),
    (ButtonBits::Btn3, "3"),
    (ButtonBits::Btn4, "4"),
    (ButtonBits::JoyBtn, "J"),
    (ButtonBits::Switch, "S"),
  ];

  let start_x: i32 = 24;
  let step_x: i32 = 15;

  for (i, (bit, label)) in labels.iter().enumerate() {
    let x = start_x + step_x * i as i32;
    let pressed = state.frame.payload.is_pressed(*bit);

    if pressed {
      // 按下：反色方块背景 + 字符
      Rectangle::new(Point::new(x - 1, y - 1), Size::new(8, 10))
        .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
        .draw(target)?;
      let inv_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);
      draw_text(target, inv_style, label, x, y)?;
    } else {
      draw_text(target, style, label, x, y)?;
    }
  }

  Ok(())
}

/// 便捷：在 (x, y) 画一行文本（Top baseline）
fn draw_text<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
  text: &str,
  x: i32,
  y: i32,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  Text::with_baseline(text, Point::new(x, y), style, Baseline::Top).draw(target)?;
  Ok(())
}

/// Toast 覆盖层：在屏幕底部两行画反色横条 + 居中显示提示文字
///
/// 布局：
/// ```text
/// y = OLED_HEIGHT - 20 .. OLED_HEIGHT   （反色横条，20 px 高）
/// 文字水平居中，采用反色字体（背景填充，字符透空）
/// ```
fn draw_toast<D>(
  target: &mut D,
  _base_style: MonoTextStyle<'_, BinaryColor>,
  toast: &Toast,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  // 底部横条 20 px 高
  let bar_h: i32 = 20;
  let bar_y: i32 = OLED_HEIGHT as i32 - bar_h;
  Rectangle::new(
    Point::new(0, bar_y),
    Size::new(OLED_WIDTH as u32, bar_h as u32),
  )
  .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
  .draw(target)?;

  // 反色字体（背景已填白，字符要透空）
  let inv_style = MonoTextStyle::new(&FONT_6X10, BinaryColor::Off);

  // 有效字节 → &str（非 ASCII 直接跳过；防御性处理）
  let len = toast.len as usize;
  let msg = core::str::from_utf8(&toast.bytes[..len]).unwrap_or("???");

  // 水平居中：文字宽度 = len * 6，屏宽 128
  let text_w = msg.len() as i32 * 6;
  let x = ((OLED_WIDTH as i32 - text_w) / 2).max(0);
  // 竖直居中：横条高 20，字符高 10 → 顶部留 5
  let y = bar_y + 5;

  draw_text(target, inv_style, msg, x, y)?;
  Ok(())
}

// ============================================================
// L 选项：低电量告警边框
// ============================================================

/// 边框闪烁周期（毫秒）—— 与电量分级对应
///
/// 亮/灭各占半周期。例如 `Low` 周期 500ms → 亮 250ms → 灭 250ms → 亮…
const BORDER_PERIOD_LOW_MS: u64 = 500;
const BORDER_PERIOD_CRITICAL_MS: u64 = 300;
const BORDER_PERIOD_EMPTY_MS: u64 = 200;

/// 边框粗细（px）—— Empty/Critical 用 2px，Low 用 1px
///
/// 让越紧急的告警视觉冲击越强。
const BORDER_THICKNESS_LOW: u32 = 1;
const BORDER_THICKNESS_CRITICAL: u32 = 2;
const BORDER_THICKNESS_EMPTY: u32 = 2;

/// 在屏幕四周画一个闪烁边框，用于低电量告警（L 选项）
///
/// # 闪烁相位
/// 从 [`embassy_time::Instant::now`] 拿到当前毫秒数，除以周期取整数商的奇偶，
/// 决定当前半周期是"亮"还是"灭"。这样每次刷屏（20 Hz）都会自动更新相位，
/// 无需在任务侧维护状态。
///
/// # 参数
/// - `level`：电量分级；必须是 [`BatteryLevel::is_alert`] = true 的值，
///   否则本函数不应该被调用（调用方 [`render`] 已负责判断）
fn draw_low_battery_border<D>(target: &mut D, level: BatteryLevel) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  // 按分级选择周期 + 边框粗细
  let (period_ms, thickness) = match level {
    BatteryLevel::Low => (BORDER_PERIOD_LOW_MS, BORDER_THICKNESS_LOW),
    BatteryLevel::Critical => (BORDER_PERIOD_CRITICAL_MS, BORDER_THICKNESS_CRITICAL),
    BatteryLevel::Empty => (BORDER_PERIOD_EMPTY_MS, BORDER_THICKNESS_EMPTY),
    BatteryLevel::Normal => return Ok(()), // 防御性早返
  };

  // 相位：now_ms / half_period 的奇偶决定"亮/灭"
  let now_ms = embassy_time::Instant::now().as_millis();
  let half_period = period_ms / 2;
  if half_period == 0 {
    return Ok(()); // 防御性：period_ms=0 时不做任何事
  }
  let phase_index = now_ms / half_period;
  let is_lit = phase_index.is_multiple_of(2);
  if !is_lit {
    return Ok(()); // 灭半周期：不画边框
  }

  // 亮半周期：画粗边框（等价于两个嵌套 Rectangle 的差集，但更简单是画四条边）
  let stroke = PrimitiveStyle::with_stroke(BinaryColor::On, thickness);
  Rectangle::new(
    Point::new(0, 0),
    Size::new(OLED_WIDTH as u32, OLED_HEIGHT as u32),
  )
  .into_styled(stroke)
  .draw(target)?;

  Ok(())
}
