//! # GamepadVisual —— 摇杆 / 按钮 / 旋钮 实时可视化
//!
//! 使用纯 SVG + CSS：
//! - 摇杆：外圈 + 内小圆点（相对 X/Y 偏移）
//! - 按钮：Btn1-4 共 4 个圆形亮灯
//! - 旋钮：两根垂直进度条

use controller_protocol::state::ButtonBits;
use leptos::prelude::*;

use crate::state::AppState;

/// 摇杆坐标域（与手柄端 AXIS_RANGE 对齐）
const AXIS_RANGE: i32 = 1000;
/// SVG viewBox 半径（像素）
const SVG_RADIUS: f64 = 80.0;
/// 摇杆点半径
const DOT_RADIUS: f64 = 12.0;

#[component]
pub fn GamepadVisual() -> impl IntoView {
  let state = expect_context::<AppState>();

  view! {
    <section class="gamepad-visual" aria-label="手柄输入可视化">
      <h2 class="section-title">"手柄输入"</h2>

      <div class="visual-grid">
        // 摇杆
        <div class="visual-cell visual-joystick" aria-label="摇杆 XY">
          <div class="cell-heading">"Joystick"</div>
          {move || {
            let s = state.gamepad.get();
            let cx = SVG_RADIUS + (s.joy_x as f64 / AXIS_RANGE as f64) * (SVG_RADIUS - DOT_RADIUS);
            let cy = SVG_RADIUS - (s.joy_y as f64 / AXIS_RANGE as f64) * (SVG_RADIUS - DOT_RADIUS);
            view! {
              <svg viewBox={format!("0 0 {} {}", SVG_RADIUS * 2.0, SVG_RADIUS * 2.0)}
                   class="joystick-svg" aria-hidden="true">
                // 外圆
                <circle cx={SVG_RADIUS} cy={SVG_RADIUS} r={SVG_RADIUS - 2.0}
                        class="joystick-frame" />
                // 十字线
                <line x1={SVG_RADIUS} y1="6" x2={SVG_RADIUS} y2={SVG_RADIUS * 2.0 - 6.0}
                      class="joystick-cross" />
                <line x1="6" y1={SVG_RADIUS} x2={SVG_RADIUS * 2.0 - 6.0} y2={SVG_RADIUS}
                      class="joystick-cross" />
                // 摇杆点
                <circle cx={cx} cy={cy} r={DOT_RADIUS} class="joystick-dot" />
              </svg>
              <div class="joystick-readout mono">
                {format!("X={:+05} Y={:+05}", s.joy_x, s.joy_y)}
              </div>
            }
          }}
        </div>

        // 按钮阵列
        <div class="visual-cell visual-buttons" aria-label="按钮">
          <div class="cell-heading">"Buttons"</div>
          <div class="button-grid">
            <ButtonLamp label="Btn1" bit={ButtonBits::Btn1} />
            <ButtonLamp label="Btn2" bit={ButtonBits::Btn2} />
            <ButtonLamp label="Btn3" bit={ButtonBits::Btn3} />
            <ButtonLamp label="Btn4" bit={ButtonBits::Btn4} />
          </div>
        </div>

        // 旋钮
        <div class="visual-cell visual-knobs" aria-label="旋钮">
          <div class="cell-heading">"Knobs"</div>
          <div class="knob-row">
            <KnobBar label="Knob1" value=Signal::derive(move || state.gamepad.get().knob_1) />
            <KnobBar label="Knob2" value=Signal::derive(move || state.gamepad.get().knob_2) />
          </div>
        </div>
      </div>
    </section>
  }
}

/// 单个按钮亮灯
#[component]
fn ButtonLamp(label: &'static str, bit: ButtonBits) -> impl IntoView {
  let state = expect_context::<AppState>();
  let bit_idx = bit as u16;

  let is_pressed = Signal::derive(move || (state.gamepad.get().buttons & (1 << bit_idx)) != 0);

  view! {
    <div class="btn-lamp-wrap" aria-label={label}>
      <span class={move || if is_pressed.get() { "btn-lamp on" } else { "btn-lamp" }}
            aria-hidden="true"></span>
      <span class="btn-lamp-label">{label}</span>
    </div>
  }
}

/// 旋钮进度条
#[component]
fn KnobBar(label: &'static str, value: Signal<u16>) -> impl IntoView {
  let percent = Signal::derive(move || {
    let raw = value.get() as f32;
    (raw / AXIS_RANGE as f32 * 100.0).clamp(0.0, 100.0)
  });

  view! {
    <div class="knob-wrap" aria-label={label}>
      <div class="knob-label">{label}</div>
      <div class="knob-bar">
        <div class="knob-fill" style:height={move || format!("{:.1}%", percent.get())}></div>
      </div>
      <div class="knob-value mono">{move || format!("{}", value.get())}</div>
    </div>
  }
}
