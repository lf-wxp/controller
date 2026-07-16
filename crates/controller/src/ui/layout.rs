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

use super::selector::{BROADCAST_MASK, PeerInfo, SelectorSnapshot, VISIBLE_ROWS};
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

  // ---- Selecting 模式：整屏面板接管，不画稳态内容 ----
  //
  // 不与稳态元素（摇杆值、Btn、Seq）共存——避免面板中看到摇杆读数
  // 在变化带来的认知干扰；底部仍保留电量告警边框 & Toast 叠加。
  if state.mode.is_selecting() {
    draw_selector_panel(target, style, &state.selector)?;

    if state.battery_level.is_alert() {
      draw_low_battery_border(target, state.battery_level)?;
    }
    if let Some(toast) = state.toast.as_ref() {
      draw_toast(target, style, toast)?;
    }
    return Ok(());
  }

  // ---- 第 0 行：标题 + 连接状态 + 目标 + 电量 ----
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

/// 顶部标题栏：`Ctl B●N●H● >#03 99%`
///
/// 布局分区（x 坐标）：
/// - `Ctl`（3 char，0..18）
/// - `B● N● H●`（三个状态灯，22..57）
/// - 目标指示器 `>#XX` / `>ALL` / `>---`（4 char，右对齐到电量前）
/// - `99%`（电量，右对齐到 128px）
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
  let mut battery_buf = LineBuf::new();
  let _ = write!(&mut battery_buf, "{:3}%", state.battery);
  let battery_w = battery_buf.len() as i32 * 6;
  let battery_x = OLED_WIDTH as i32 - battery_w;
  draw_text(target, style, &battery_buf, battery_x, 0)?;

  // 目标指示器：介于 H 灯（x=54, 宽 6px → 60）与电量之间
  // 提前 2px 预留间隙；可用宽度 = battery_x - 62
  let indicator_x = base_x + 38;
  let indicator_max_w = battery_x - indicator_x - 2;
  if indicator_max_w >= 24 {
    draw_target_indicator(target, style, state.active_dest_mask, indicator_x)?;
  }

  Ok(())
}

/// 在标题栏绘制目标指示器——根据 `dest_mask` 选择显示样式。
///
/// 样式规则（四档，均固定宽度 ≤ 24px）：
/// - `mask == BROADCAST_MASK`  → `>ALL`
/// - `mask == 0`               → `>---`
/// - `mask.count_ones() == 1`  → `>#03`（唯一位置的 id）
/// - 其它                     → `>#*N`（N = 选中数量）
fn draw_target_indicator<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
  mask: u32,
  x: i32,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  let mut buf = LineBuf::new();
  match mask {
    BROADCAST_MASK => {
      let _ = write!(&mut buf, ">ALL");
    }
    0 => {
      let _ = write!(&mut buf, ">---");
    }
    _ => {
      let count = mask.count_ones();
      if count == 1 {
        // trailing_zeros 上限为 31（count_ones == 1 不可能全 0），不会返回 32
        let id = mask.trailing_zeros();
        let _ = write!(&mut buf, ">#{:02}", id);
      } else {
        // 多选 → `>#*N`；N 最大 32，两位十进制足够
        let n = count.min(99);
        let _ = write!(&mut buf, ">#*{:1}", n);
      }
    }
  }
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

// ============================================================
// Target Selector 面板（长按 Switch 进入的选择模式）
// ============================================================

/// 面板每行高度（含 1px 行间距）
const SELECTOR_ROW_H: i32 = LINE_H + 2;

/// 光标标识字符宽度（`>` 一字符 = 6px + 1px 间距）
const SELECTOR_MARK_W: i32 = 7;

/// 面板 y 起点（跳过顶部标题 + 分割线）
const SELECTOR_HEADER_Y: i32 = 0;
const SELECTOR_LIST_TOP_Y: i32 = LINE_H + 3;

/// 绘制 Selecting 模式下的选择器面板（全屏覆盖 Normal 内容）
///
/// # 布局（128×64）
/// ```text
///  y=0  : Select Target        1/5
///  y=11 : ─────────────────────────
///  y=13 : > #01 motor  -42dBm  *      ← 光标行反色
///  y=25 :   #03 led    -55dBm
///  y=37 :   #05 servo  -60dBm  *
///  y=49 : ─────────────────────────
///  y=51 : Jy^v B1+ B2-  Sw exit
/// ```
///
/// - `>` = 当前光标
/// - `*` = pending_mask 已选中
/// - 列表按 [`VISIBLE_ROWS`] 分页，光标始终在可见窗口内
///
/// # Errors
/// 传递底层 [`DrawTarget`] 的错误。
fn draw_selector_panel<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
  snap: &SelectorSnapshot,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  // ---- 顶部标题：Select Target + N/M 计数 ----
  draw_text(target, style, "Select Target", 0, SELECTOR_HEADER_Y)?;
  draw_selector_header_counter(target, style, snap)?;

  // ---- 分割线 ----
  let sep_y = LINE_H + 1;
  Line::new(
    Point::new(0, sep_y),
    Point::new(OLED_WIDTH as i32 - 1, sep_y),
  )
  .into_styled(PrimitiveStyle::with_stroke(BinaryColor::On, 1))
  .draw(target)?;

  // ---- 候选行 ----
  if snap.candidates.is_empty() {
    draw_selector_empty_state(target, style)?;
  } else {
    draw_selector_rows(target, style, snap)?;
  }

  // ---- 底部操作提示（贴屏幕底部）----
  let footer_y = OLED_HEIGHT as i32 - LINE_H;
  draw_text(target, style, "Jy^v B1+ B2- SwExit", 0, footer_y)?;

  Ok(())
}

/// 面板右上角：`光标索引/总数`（例如 `2/5`）
fn draw_selector_header_counter<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
  snap: &SelectorSnapshot,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  if snap.candidates.is_empty() {
    return Ok(());
  }
  let mut buf = LineBuf::new();
  // cursor 1-based 显示更符合用户直觉（"第几个"）
  let cur_1based = usize::from(snap.cursor).saturating_add(1);
  let _ = write!(&mut buf, "{}/{}", cur_1based, snap.candidates.len());
  let w = buf.len() as i32 * 6;
  let x = OLED_WIDTH as i32 - w;
  draw_text(target, style, &buf, x, SELECTOR_HEADER_Y)
}

/// 候选列表为空时的引导文案（居中显示）
fn draw_selector_empty_state<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  // 三行文案，从 y=SELECTOR_LIST_TOP_Y 起
  let lines: [&str; 3] = ["No receivers found.", "Waiting for peers", "to announce..."];
  for (idx, text) in lines.iter().enumerate() {
    let y = SELECTOR_LIST_TOP_Y + SELECTOR_ROW_H * idx as i32;
    draw_text(target, style, text, 0, y)?;
  }
  Ok(())
}

/// 绘制可见窗口内的候选行 + 高亮光标行
fn draw_selector_rows<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
  snap: &SelectorSnapshot,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  let total = snap.candidates.len();
  let cursor = usize::from(snap.cursor).min(total.saturating_sub(1));

  // 计算可见窗口起点：让光标始终落在 [top, top + VISIBLE_ROWS) 内；
  // 优先"光标居中"，光标靠边时贴边显示。
  let visible = VISIBLE_ROWS.min(total);
  let half = visible / 2;
  let top = cursor
    .saturating_sub(half)
    .min(total.saturating_sub(visible));

  for row_idx in 0..visible {
    let idx = top + row_idx;
    let Some(peer) = snap.candidates.get(idx) else {
      break;
    };
    let y = SELECTOR_LIST_TOP_Y + SELECTOR_ROW_H * row_idx as i32;
    let is_cursor = idx == cursor;
    draw_selector_row(target, style, peer, y, is_cursor, snap.pending_mask)?;
  }
  Ok(())
}

/// 绘制一行候选：`> #ID role  RSSIdBm  *`
fn draw_selector_row<D>(
  target: &mut D,
  style: MonoTextStyle<'_, BinaryColor>,
  peer: &PeerInfo,
  y: i32,
  is_cursor: bool,
  pending_mask: u32,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = BinaryColor>,
{
  // 光标行画反色背景（整行 128×10 反色矩形）
  if is_cursor {
    Rectangle::new(
      Point::new(0, y - 1),
      Size::new(OLED_WIDTH as u32, LINE_H as u32 + 1),
    )
    .into_styled(PrimitiveStyle::with_fill(BinaryColor::On))
    .draw(target)?;
  }

  let effective_style = if is_cursor {
    MonoTextStyle::new(&FONT_6X10, BinaryColor::Off)
  } else {
    style
  };

  // 光标标记
  if is_cursor {
    draw_text(target, effective_style, ">", 0, y)?;
  }

  // `#ID` —— 2 位十进制
  let mut id_buf = LineBuf::new();
  let _ = write!(&mut id_buf, "#{:02}", peer.receiver_id);
  draw_text(target, effective_style, &id_buf, SELECTOR_MARK_W, y)?;

  // role 名称 —— UTF-8 反解；非法字节整段跳过
  let role_slice = peer.role_bytes();
  if let Ok(role_str) = core::str::from_utf8(role_slice) {
    draw_text(target, effective_style, role_str, SELECTOR_MARK_W + 20, y)?;
  }

  // RSSI（可选：<-127 表示"未知"，跳过绘制）
  if peer.rssi_dbm > i8::MIN {
    let mut rssi_buf = LineBuf::new();
    let _ = write!(&mut rssi_buf, "{}dBm", peer.rssi_dbm);
    // 右对齐到 118px（给 `*` 留 10px）
    let text_w = rssi_buf.len() as i32 * 6;
    let x = OLED_WIDTH as i32 - text_w - 10;
    draw_text(target, effective_style, &rssi_buf, x, y)?;
  }

  // 已选标记 `*`
  let mask_bit = 1u32.wrapping_shl(u32::from(peer.receiver_id));
  if pending_mask & mask_bit != 0 {
    let star_x = OLED_WIDTH as i32 - 6;
    draw_text(target, effective_style, "*", star_x, y)?;
  }

  Ok(())
}
