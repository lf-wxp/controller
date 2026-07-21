//! LCD 显示模块：1.3 寸 240x240 ST7789 SPI 屏幕
//!
//! 提供两个能力：
//! 1. [`init_display`]：完成 SPI + DC/RST/BL 的初始化，返回 `Display` 句柄
//! 2. [`ViewModel`] + [`render`]：把 `GamepadState` + 元数据画到屏上
//!
//! 布局（240x240）：
//! ```text
//!   ┌──────────────────────────┐
//!   │ RECV  seq=12345  gap=2   │  顶部状态行
//!   ├──────────────────────────┤
//!   │ [B1][B2][B3][B4]         │  4 键方块
//!   │                          │
//!   │ Joy ( +12000, -50 )      │  摇杆 + 十字准星
//!   │                          │
//!   │ Knob1 32768  Knob2 16384 │  两个旋钮进度条
//!   │ id=0.  filt=0  rep=0     │  底部 peer 状态行
//!   └──────────────────────────┘
//! ```

use embedded_graphics::{
  mono_font::{
    MonoTextStyleBuilder,
    ascii::{FONT_6X10, FONT_8X13_BOLD},
  },
  pixelcolor::Rgb565,
  prelude::*,
  primitives::{PrimitiveStyle, PrimitiveStyleBuilder, Rectangle},
  text::{Baseline, Text, TextStyleBuilder},
};
use protocol::{ButtonBits, GamepadState};

use crate::self_test::{ALL_ITEMS, SelfTestReport, SelfTestStatus};

/// 屏幕分辨率
pub const SCREEN_W: u16 = 240;
pub const SCREEN_H: u16 = 240;

// —— 主题配色 ——
const BG: Rgb565 = Rgb565::BLACK;
const FG: Rgb565 = Rgb565::WHITE;
const ACCENT: Rgb565 = Rgb565::CSS_CYAN;
const WARN: Rgb565 = Rgb565::CSS_ORANGE;
const OK: Rgb565 = Rgb565::CSS_LIME;
const OFF: Rgb565 = Rgb565::CSS_DIM_GRAY;

/// 渲染时需要的一切上下文
#[derive(Debug, Clone, Copy)]
pub struct ViewModel {
  /// 是否已收到过任何帧
  pub have_data: bool,
  /// 最近一帧的 seq
  pub last_seq: u32,
  /// 累计 seq gap 数
  pub gap_count: u32,
  /// 累计成功解码帧数
  pub ok_count: u32,
  /// 累计被 `dest_mask` 过滤掉（收到但不是发给本机）的帧数
  pub filtered_count: u32,
  /// 本机当前 `receiver_id`（由手柄的 `AssignId` 命令下发；未分配时为
  /// [`crate::peer::INITIAL_RECEIVER_ID`] = `UNASSIGNED_ID` / `u8::MAX`）
  pub receiver_id: u8,
  /// 是否已被手柄通过 `AssignId` 分配过 ID
  pub assigned: bool,
  /// 累计发出的 `AnnounceReply` 次数
  pub reply_count: u32,
  /// 最近一帧的手柄状态
  pub state: GamepadState,
}

impl ViewModel {
  pub const fn empty() -> Self {
    Self {
      have_data: false,
      last_seq: 0,
      gap_count: 0,
      ok_count: 0,
      filtered_count: 0,
      receiver_id: crate::peer::INITIAL_RECEIVER_ID,
      assigned: false,
      reply_count: 0,
      state: GamepadState::EMPTY,
    }
  }
}

// ============================================================
// 绘制辅助
// ============================================================

fn fill_screen<D>(display: &mut D, color: Rgb565) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  Rectangle::new(Point::zero(), Size::new(SCREEN_W as u32, SCREEN_H as u32))
    .into_styled(PrimitiveStyle::with_fill(color))
    .draw(display)
}

/// 画文字：`bg` 作为**字形背景色**一起绘制。
///
/// 传背景色是增量重绘的关键：每个字符会连同其背景单元格一次性写入，
/// 因此新文本能**原地覆盖**旧文本，无需先清屏（清屏正是闪烁的根因）。
/// 对会变长/变短的字段，调用方用固定宽度格式化（如 `{:<5}` / `{:+6}`），
/// 让尾随空格的背景把残留字符抹掉。
fn draw_text<D>(
  display: &mut D,
  s: &str,
  pos: Point,
  color: Rgb565,
  bg: Rgb565,
  bold: bool,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  let text_style = TextStyleBuilder::new().baseline(Baseline::Top).build();
  if bold {
    let style = MonoTextStyleBuilder::new()
      .font(&FONT_8X13_BOLD)
      .text_color(color)
      .background_color(bg)
      .build();
    Text::with_text_style(s, pos, style, text_style).draw(display)?;
  } else {
    let style = MonoTextStyleBuilder::new()
      .font(&FONT_6X10)
      .text_color(color)
      .background_color(bg)
      .build();
    Text::with_text_style(s, pos, style, text_style).draw(display)?;
  }
  Ok(())
}

/// 画一个按钮方块：8x13 字号 + 边框；`width` 允许调用方按不同布局自适应
fn draw_button<D>(
  display: &mut D,
  label: &str,
  pos: Point,
  width: u32,
  pressed: bool,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  let size = Size::new(width, 32);
  let (bg, fg, border) = if pressed {
    (OK, BG, ACCENT)
  } else {
    (BG, OFF, OFF)
  };

  let block_style = PrimitiveStyleBuilder::new()
    .fill_color(bg)
    .stroke_color(border)
    .stroke_width(2)
    .build();
  Rectangle::new(pos, size)
    .into_styled(block_style)
    .draw(display)?;

  // 居中文字（8x13 字号，label 长度 1-4 char）；背景传按钮底色 `bg`，
  // 让字形单元格与方块底色一致（而非强制黑底）。
  let text_x = pos.x + (size.width as i32 - (label.len() as i32) * 8) / 2;
  let text_y = pos.y + (size.height as i32 - 13) / 2;
  draw_text(display, label, Point::new(text_x, text_y), fg, bg, true)?;
  Ok(())
}

/// 画一个水平进度条，val 归一化到 0..=AXIS_RANGE（旋钮量程）
fn draw_bar<D>(
  display: &mut D,
  pos: Point,
  width: u32,
  height: u32,
  val: u16,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  // 外框
  Rectangle::new(pos, Size::new(width, height))
    .into_styled(
      PrimitiveStyleBuilder::new()
        .stroke_color(OFF)
        .stroke_width(1)
        .fill_color(BG)
        .build(),
    )
    .draw(display)?;

  // 填充（val 量程 0..=AXIS_RANGE，clamp 防止越界填出外框）
  let filled_w = ((u32::from(val) * (width - 2)) / AXIS_RANGE as u32).min(width - 2);
  if filled_w > 0 {
    Rectangle::new(
      Point::new(pos.x + 1, pos.y + 1),
      Size::new(filled_w, height - 2),
    )
    .into_styled(PrimitiveStyle::with_fill(ACCENT))
    .draw(display)?;
  }
  Ok(())
}

/// 摇杆区域边长（正方形）。
const JOY_REGION: i32 = 80;
/// 摇杆光点半边长（光点是 `2*JOY_DOT+? ` 的小方块）。
const JOY_DOT: i32 = 3;

/// 摇杆/旋钮坐标满量程（与手柄端 `config::tuning::AXIS_RANGE` 对齐）。
///
/// ⚠️ 手柄归一化后摇杆是 `-AXIS_RANGE..=+AXIS_RANGE`、旋钮是 `0..=AXIS_RANGE`
/// （见 `protocol::GamepadState` 字段注释），**不是 i16/u16 满量程**。
/// 早期这里误用 `i16::MAX`/`u16::MAX` 做映射，导致满偏时光点只移动约 ±1px、
/// 旋钮条几乎不填充。dashboard 端 (`gamepad_visual.rs`) 也用这个值。
const AXIS_RANGE: i32 = 1000;

/// 旋钮显示滞回阈值（0..=AXIS_RANGE 量程内的点数）。
///
/// 旋钮电位器的 ADC 采样即便在手柄端做过 4 点移动平均，中位读数仍会在低位
/// 持续 ±数点抖动，c6 若原样显示就表现为数字/进度条跳动。这里在**显示层**
/// 加一层滞回吸收抖动，不影响协议传输值，也不改手柄采样逻辑。
const KNOB_DISPLAY_HYSTERESIS: u16 = 8;

/// 对旋钮显示值做滞回：与上次显示值相差不足 [`KNOB_DISPLAY_HYSTERESIS`] 时
/// 沿用旧值，抖动即被吸收。
///
/// 两端极值（`0` / `AXIS_RANGE`，电位器机械到底、读数本就稳定）直接透传，
/// 避免"旋到底却显示不到 0 / 满量程"。
#[must_use]
pub fn stabilize_knob(prev: u16, new: u16) -> u16 {
  if new == 0 || new >= AXIS_RANGE as u16 {
    return new;
  }
  if new.abs_diff(prev) < KNOB_DISPLAY_HYSTERESIS {
    prev
  } else {
    new
  }
}

/// 计算摇杆光点中心像素坐标：joy_x/joy_y 是 i16，量程为 **±AXIS_RANGE**
/// （不是 ±i16::MAX），映射到区域中心 ± `span`。
fn joy_dot_center(pos: Point, joy_x: i16, joy_y: i16) -> Point {
  let cx = pos.x + JOY_REGION / 2;
  let cy = pos.y + JOY_REGION / 2;
  // 留出光点半径 + 边框余量，保证满偏时光点不压到边框
  let span = JOY_REGION / 2 - JOY_DOT - 2;
  let dx = ((joy_x as i32) * span / AXIS_RANGE).clamp(-span, span);
  let dy = ((joy_y as i32) * span / AXIS_RANGE).clamp(-span, span);
  Point::new(cx + dx, cy - dy)
}

/// 画摇杆的十字准星（两条 1px 线）。光点移动后需要补画，避免残留缺口。
fn draw_joy_crosshair<D>(display: &mut D, pos: Point) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  let cx = pos.x + JOY_REGION / 2;
  let cy = pos.y + JOY_REGION / 2;
  Rectangle::new(Point::new(pos.x, cy), Size::new(JOY_REGION as u32, 1))
    .into_styled(PrimitiveStyle::with_fill(OFF))
    .draw(display)?;
  Rectangle::new(Point::new(cx, pos.y), Size::new(1, JOY_REGION as u32))
    .into_styled(PrimitiveStyle::with_fill(OFF))
    .draw(display)?;
  Ok(())
}

/// 画摇杆光点（`fill` 传 [`ACCENT`] 画点、传 [`BG`] 则擦除）。
fn draw_joy_dot<D>(display: &mut D, center: Point, fill: Rgb565) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  Rectangle::new(
    Point::new(center.x - JOY_DOT, center.y - JOY_DOT),
    Size::new((JOY_DOT * 2) as u32, (JOY_DOT * 2) as u32),
  )
  .into_styled(PrimitiveStyle::with_fill(fill))
  .draw(display)
}

/// 摇杆十字准星 + 光点，**增量式**：`prev_joy` 为 `None` 时整块重画（边框 +
/// 十字 + 光点）；为 `Some` 时只擦除旧光点、补画十字、画新光点，避免每帧刷整块
/// 80×80 造成的局部闪烁（摇杆值常因 ADC 噪声逐帧微变）。
fn draw_joystick<D>(
  display: &mut D,
  pos: Point,
  joy_x: i16,
  joy_y: i16,
  prev_joy: Option<(i16, i16)>,
) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  let new_center = joy_dot_center(pos, joy_x, joy_y);

  match prev_joy {
    // 首帧 / 切页：画边框 + 十字 + 光点
    None => {
      Rectangle::new(pos, Size::new(JOY_REGION as u32, JOY_REGION as u32))
        .into_styled(
          PrimitiveStyleBuilder::new()
            .stroke_color(OFF)
            .stroke_width(1)
            .fill_color(BG)
            .build(),
        )
        .draw(display)?;
      draw_joy_crosshair(display, pos)?;
      draw_joy_dot(display, new_center, ACCENT)?;
    }
    // 增量：擦旧点 → 补十字 → 画新点（边框保持不动）
    Some((ox, oy)) => {
      let old_center = joy_dot_center(pos, ox, oy);
      if old_center != new_center {
        draw_joy_dot(display, old_center, BG)?;
        draw_joy_crosshair(display, pos)?;
        draw_joy_dot(display, new_center, ACCENT)?;
      }
    }
  }
  Ok(())
}

// ============================================================
// 主渲染入口
// ============================================================

/// 按钮定义（4 键：Btn1-4）。
///
/// JoyBtn / Switch 不再展示：Switch(IO15) 已改作彩灯输出、不再是输入；
/// JoyBtn 仅用于手柄本机长按开选择器，无需在接收端画面上呈现。
const BTN_DEFS: [(ButtonBits, &str); 4] = [
  (ButtonBits::Btn1, "B1"),
  (ButtonBits::Btn2, "B2"),
  (ButtonBits::Btn3, "B3"),
  (ButtonBits::Btn4, "B4"),
];
//   4 键左对齐：8(左边距) + 4*btn_w + 3*gap = 8 + 136 + 12 = 156，右侧留白
const BTN_W: u32 = 34;
const BTN_GAP: i32 = 4;

/// **增量重绘**：只重画相对 `prev` 发生变化的部件，避免整屏清屏导致的闪烁。
///
/// # 为什么不再整屏 `fill_screen`
/// 原实现每帧 `fill_screen(BG)` 再全量重画：整块 240×240 先刷黑再重绘，
/// 在 20MHz SPI 上单帧要几十毫秒，且"刷黑→重绘"的过程肉眼可见——手柄状态帧
/// 高频到达时就表现为持续闪烁。
///
/// # 策略
/// - `prev == None`：首帧 / 切页，整屏清一次并全量绘制静态元素（K1/K2 标签等）。
/// - `prev == Some(p)`：逐字段比较，只重画变化的部件。
/// - 文本统一带 `BG` 背景色绘制（见 [`draw_text`]），原地覆盖旧值；变长数值用
///   固定宽度格式化，靠尾随空格背景抹除残留。
/// - 方块 / 进度条 / 摇杆本身每次都会用底色填满自己的矩形，天然自清除，只是
///   通过 diff 控制"是否调用"，未变化则完全不动。
pub fn render<D>(display: &mut D, vm: &ViewModel, prev: Option<&ViewModel>) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  let full = prev.is_none();
  if full {
    fill_screen(display, BG)?;
  }

  // —— 顶部状态行 ——
  if full || prev.is_some_and(|p| p.have_data != vm.have_data) {
    let (title, title_color) = if vm.have_data {
      ("RECV", OK)
    } else {
      ("WAIT", WARN)
    };
    draw_text(display, title, Point::new(4, 4), title_color, BG, true)?;
  }

  if full || prev.is_some_and(|p| p.last_seq != vm.last_seq) {
    let mut buf = heapless_str::<24>();
    let _ = core::fmt::write(&mut buf, format_args!("seq={:<10}", vm.last_seq));
    draw_text(display, buf.as_str(), Point::new(50, 6), FG, BG, false)?;
  }

  if full || prev.is_some_and(|p| p.gap_count != vm.gap_count) {
    let mut buf2 = heapless_str::<24>();
    let _ = core::fmt::write(&mut buf2, format_args!("gap={:<4}", vm.gap_count));
    draw_text(
      display,
      buf2.as_str(),
      Point::new(140, 6),
      if vm.gap_count > 0 { WARN } else { OFF },
      BG,
      false,
    )?;
  }

  if full || prev.is_some_and(|p| p.ok_count != vm.ok_count) {
    let mut buf3 = heapless_str::<24>();
    let _ = core::fmt::write(&mut buf3, format_args!("ok={:<5}", vm.ok_count));
    draw_text(display, buf3.as_str(), Point::new(190, 6), OFF, BG, false)?;
  }

  // —— 按钮行 ——：逐键只在按压状态翻转时重画
  for (i, (bit, label)) in BTN_DEFS.iter().enumerate() {
    let pressed = vm.state.is_pressed(*bit);
    if full || prev.is_some_and(|p| p.state.is_pressed(*bit) != pressed) {
      let x = 8 + (i as i32) * (BTN_W as i32 + BTN_GAP);
      draw_button(display, label, Point::new(x, 28), BTN_W, pressed)?;
    }
  }

  // —— 摇杆区域 + 数值 ——：仅在坐标变化时重画（增量移动光点）
  if full
    || prev.is_some_and(|p| p.state.joy_x != vm.state.joy_x || p.state.joy_y != vm.state.joy_y)
  {
    let prev_joy = prev.map(|p| (p.state.joy_x, p.state.joy_y));
    draw_joystick(
      display,
      Point::new(8, 72),
      vm.state.joy_x,
      vm.state.joy_y,
      prev_joy,
    )?;
    let mut jx = heapless_str::<24>();
    let _ = core::fmt::write(&mut jx, format_args!("X:{:+6}", vm.state.joy_x));
    draw_text(display, jx.as_str(), Point::new(96, 80), FG, BG, false)?;
    let mut jy = heapless_str::<24>();
    let _ = core::fmt::write(&mut jy, format_args!("Y:{:+6}", vm.state.joy_y));
    draw_text(display, jy.as_str(), Point::new(96, 96), FG, BG, false)?;
  }

  // —— 旋钮 ——：静态标签只在首帧画；进度条 + 数值仅在旋钮值变化时重画
  if full {
    draw_text(display, "K1", Point::new(8, 164), FG, BG, true)?;
  }
  if full || prev.is_some_and(|p| p.state.knob_1 != vm.state.knob_1) {
    draw_bar(display, Point::new(32, 164), 180, 12, vm.state.knob_1)?;
    let mut k1 = heapless_str::<24>();
    let _ = core::fmt::write(&mut k1, format_args!("{:<5}", vm.state.knob_1));
    draw_text(display, k1.as_str(), Point::new(32, 180), OFF, BG, false)?;
  }

  if full {
    draw_text(display, "K2", Point::new(8, 200), FG, BG, true)?;
  }
  if full || prev.is_some_and(|p| p.state.knob_2 != vm.state.knob_2) {
    draw_bar(display, Point::new(32, 200), 180, 12, vm.state.knob_2)?;
    let mut k2 = heapless_str::<24>();
    let _ = core::fmt::write(&mut k2, format_args!("{:<5}", vm.state.knob_2));
    draw_text(display, k2.as_str(), Point::new(32, 216), OFF, BG, false)?;
  }

  // —— 底部 peer 状态行（y=228~239）——
  //
  // 显示：本机 receiver_id、是否被手柄分配过、被 dest_mask 过滤的帧数、发出的 AnnounceReply 数
  // 格式（示例）：`id= 3*  filt=12    rep=2`；`*` 表示已被 AssignId 分配过（未分配显示 `.`）。
  // 未分配时 receiver_id 是 UNASSIGNED_ID（`u8::MAX`）哨兵，显示 `-` 而非裸 255。
  if full
    || prev.is_some_and(|p| {
      p.receiver_id != vm.receiver_id
        || p.assigned != vm.assigned
        || p.filtered_count != vm.filtered_count
        || p.reply_count != vm.reply_count
    })
  {
    let mut peer = heapless_str::<48>();
    let mark = if vm.assigned { '*' } else { '.' };
    // filt / rep 单调递增，固定宽度 `{:<6}` 的尾随空格足以抹除历史残留；
    // id 右对齐到 2 列，未分配时占位 "-"，两种情况列宽一致。
    let _ = if vm.assigned {
      core::fmt::write(
        &mut peer,
        format_args!(
          "id={:>2}{}  filt={:<6}  rep={:<6}",
          vm.receiver_id, mark, vm.filtered_count, vm.reply_count
        ),
      )
    } else {
      core::fmt::write(
        &mut peer,
        format_args!(
          "id={:>2}{}  filt={:<6}  rep={:<6}",
          "-", mark, vm.filtered_count, vm.reply_count
        ),
      )
    };
    draw_text(
      display,
      peer.as_str(),
      Point::new(4, 228),
      if vm.assigned { OK } else { OFF },
      BG,
      false,
    )?;
  }

  Ok(())
}

// ============================================================
// 自检页渲染
// ============================================================

/// 绘制自检进度页
///
/// 布局（240x240）：
/// ```text
///   ┌──────────────────────────┐
///   │       SELF-TEST          │  标题
///   ├──────────────────────────┤
///   │ HEAP    [ OK ]           │
///   │ LCD     [ OK ]           │
///   │ WIFI    [ .. ]           │
///   │ ESPNOW  [ .. ]           │
///   │ CODEC   [FAIL] alloc<512 │  失败附带原因
///   │ WATCH   [ .. ]           │
///   ├──────────────────────────┤
///   │ Result: OK / FAILED      │  汇总
///   └──────────────────────────┘
/// ```
pub fn render_self_test<D>(display: &mut D, report: &SelfTestReport) -> Result<(), D::Error>
where
  D: DrawTarget<Color = Rgb565>,
{
  fill_screen(display, BG)?;

  // —— 标题 ——
  draw_text(display, "SELF-TEST", Point::new(72, 8), ACCENT, BG, true)?;

  // 分隔线
  Rectangle::new(Point::new(4, 26), Size::new(232, 1))
    .into_styled(PrimitiveStyle::with_fill(OFF))
    .draw(display)?;

  // —— 逐项 ——
  let start_y: i32 = 40;
  let row_h: i32 = 22;
  for (i, item) in ALL_ITEMS.iter().enumerate() {
    let y = start_y + (i as i32) * row_h;

    // 项目名
    draw_text(display, item.label(), Point::new(8, y), FG, BG, true)?;

    // 状态标签
    let status = report.status_of(*item);
    let (tag, color) = match status {
      SelfTestStatus::Pending => ("[ .. ]", OFF),
      SelfTestStatus::Ok => ("[ OK ]", OK),
      SelfTestStatus::Fail(_) => ("[FAIL]", WARN),
    };
    draw_text(display, tag, Point::new(96, y), color, BG, true)?;

    // 失败原因
    if let SelfTestStatus::Fail(reason) = status {
      draw_text(display, reason, Point::new(160, y + 2), WARN, BG, false)?;
    }
  }

  // 底部汇总
  let summary_y = start_y + (ALL_ITEMS.len() as i32) * row_h + 8;
  Rectangle::new(Point::new(4, summary_y), Size::new(232, 1))
    .into_styled(PrimitiveStyle::with_fill(OFF))
    .draw(display)?;

  let (summary, summary_color) = if report.any_fail() {
    ("FAILED - CHECK HW", WARN)
  } else if report.all_ok() {
    ("ALL OK", OK)
  } else {
    ("TESTING...", ACCENT)
  };
  draw_text(
    display,
    summary,
    Point::new(8, summary_y + 6),
    summary_color,
    BG,
    true,
  )?;

  Ok(())
}

// ============================================================

// ============================================================
// 无 alloc 的小字符串工具（基于固定数组的 core::fmt::Write）
// ============================================================

/// 便携的定长字符串缓冲，只在渲染时的临时格式化中使用
struct FixedStr<const N: usize> {
  buf: [u8; N],
  len: usize,
}

impl<const N: usize> FixedStr<N> {
  const fn new() -> Self {
    Self {
      buf: [0; N],
      len: 0,
    }
  }
  fn as_str(&self) -> &str {
    // SAFETY: 我们只追加了 UTF-8 字节
    unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
  }
}

impl<const N: usize> core::fmt::Write for FixedStr<N> {
  fn write_str(&mut self, s: &str) -> core::fmt::Result {
    let bytes = s.as_bytes();
    if self.len + bytes.len() > N {
      return Err(core::fmt::Error);
    }
    self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
    self.len += bytes.len();
    Ok(())
  }
}

/// 便捷构造
fn heapless_str<const N: usize>() -> FixedStr<N> {
  FixedStr::<N>::new()
}
