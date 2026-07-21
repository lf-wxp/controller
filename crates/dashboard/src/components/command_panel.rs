//! # CommandPanel —— 发送 Command 到手柄
//!
//! 展示 5 种 CommandBody 变体的表单：
//! - Nop
//! - LedBlink { led_idx, count, period_ms }
//! - SetSensitivity { joy_scale, knob_scale }
//! - ShowToast { text }
//! - SetBatteryMode { simulate }

use leptos::prelude::*;
use protocol::{Command, CommandBody, KeyId};

use crate::bluetooth::{StoredHandles, send_command};
use crate::state::{AppState, EventEntry};

/// 命令类型下拉框的选项列表
const COMMAND_TYPES: [(&str, &str); 5] = [
  ("Nop", "Nop"),
  ("LedBlink", "LED 闪烁"),
  ("SetSensitivity", "设置灵敏度"),
  ("ShowToast", "显示 Toast"),
  ("SetBatteryMode", "切换电池模式"),
];

#[component]
pub fn CommandPanel(handles: RwSignal<Option<StoredHandles>, LocalStorage>) -> impl IntoView {
  let state = expect_context::<AppState>();

  // 当前选中的命令类型
  let selected_type = RwSignal::new("Nop".to_string());

  // 各种参数字段（分别用独立 signal，字段间互不干扰）
  let led_idx = RwSignal::new(0_u8);
  let led_count = RwSignal::new(3_u8);
  let led_period = RwSignal::new(200_u16);
  let joy_scale = RwSignal::new(1000_u16);
  let knob_scale = RwSignal::new(1000_u16);
  let toast_text = RwSignal::new(String::from("hi"));
  let bat_simulate = RwSignal::new(false);

  let handle_key_id_change = move |ev: web_sys::Event| {
    let value = event_target_value(&ev);
    if let Ok(raw) = value.parse::<u8>()
      && let Ok(kid) = KeyId::new(raw)
    {
      state.key_id.set(kid);
    }
  };

  let handle_type_change = move |ev: web_sys::Event| {
    selected_type.set(event_target_value(&ev));
  };

  let handle_send_click = move |_| {
    let kind = build_body(
      &selected_type.get(),
      led_idx.get(),
      led_count.get(),
      led_period.get(),
      joy_scale.get(),
      knob_scale.get(),
      &toast_text.get(),
      bat_simulate.get(),
    );

    let Some(kind) = kind else {
      state.push_event(EventEntry::warn("命令参数不合法"));
      return;
    };

    let seq = state.next_tx_seq();
    let cmd = Command {
      seq,
      key_id: state.key_id.get_untracked(),
      kind,
    };
    send_command(state, handles, cmd);
  };

  view! {
    <section class="command-panel" aria-label="发送命令">
      <h2 class="section-title">"发送命令"</h2>

      <div class="command-form">
        // 命令类型选择
        <label class="form-row">
          <span class="form-label">"类型"</span>
          <select
            class="form-select"
            aria-label="命令类型"
            on:change=handle_type_change
            prop:value={move || selected_type.get()}
          >
            {COMMAND_TYPES.iter().map(|(value, label)| view! {
              <option value={*value}>{*label}</option>
            }).collect_view()}
          </select>
        </label>

        // Key ID 选择
        <label class="form-row">
          <span class="form-label">"Key ID"</span>
          <select
            class="form-select"
            aria-label="发送命令使用的密钥槽"
            on:change=handle_key_id_change
            prop:value={move || state.key_id.get().as_u8().to_string()}
          >
            <option value="0">"0 (SECRET_V1)"</option>
            <option value="1">"1 (SECRET_V2)"</option>
          </select>
        </label>

        // 动态参数区
        <div class="form-params">
          <Show when=move || selected_type.get() == "LedBlink" fallback=|| ()>
            <NumberInput label="led_idx" value=led_idx min=0 max=3 />
            <NumberInput label="count" value=led_count min=1 max=20 />
            <NumberU16Input label="period_ms" value=led_period min=50 max=2000 />
          </Show>

          <Show when=move || selected_type.get() == "SetSensitivity" fallback=|| ()>
            <NumberU16Input label="joy_scale" value=joy_scale min=0 max=1000 />
            <NumberU16Input label="knob_scale" value=knob_scale min=0 max=1000 />
          </Show>

          <Show when=move || selected_type.get() == "ShowToast" fallback=|| ()>
            <label class="form-row">
              <span class="form-label">"文本 (≤5 字节)"</span>
              <input
                type="text"
                maxlength="5"
                class="form-input"
                aria-label="Toast 文本内容"
                prop:value={move || toast_text.get()}
                on:input=move |ev| toast_text.set(event_target_value(&ev))
              />
            </label>
          </Show>

          <Show when=move || selected_type.get() == "SetBatteryMode" fallback=|| ()>
            <label class="form-row form-row-inline">
              <input
                type="checkbox"
                class="form-checkbox"
                aria-label="模拟电池模式"
                prop:checked={move || bat_simulate.get()}
                on:change=move |ev| bat_simulate.set(event_target_checked(&ev))
              />
              <span>"启用模拟电池模式（simulate = true）"</span>
            </label>
          </Show>
        </div>

        // 发送按钮
        <button
          type="button"
          class="send-btn"
          aria-label="发送命令到手柄"
          on:click=handle_send_click
          prop:disabled={move || matches!(state.conn.get(),
            crate::state::ConnState::Disconnected | crate::state::ConnState::Connecting)}
        >
          "发送 (seq="{move || state.tx_counter.get().saturating_add(1)}")"
        </button>
      </div>
    </section>
  }
}

/// u8 输入框子组件
#[component]
fn NumberInput(label: &'static str, value: RwSignal<u8>, min: u8, max: u8) -> impl IntoView {
  view! {
    <label class="form-row">
      <span class="form-label">{label}</span>
      <input
        type="number"
        class="form-input"
        min={min as i32}
        max={max as i32}
        aria-label={label}
        prop:value={move || value.get().to_string()}
        on:input=move |ev| {
          if let Ok(v) = event_target_value(&ev).parse::<u8>() {
            value.set(v.clamp(min, max));
          }
        }
      />
    </label>
  }
}

/// u16 输入框子组件
#[component]
fn NumberU16Input(label: &'static str, value: RwSignal<u16>, min: u16, max: u16) -> impl IntoView {
  view! {
    <label class="form-row">
      <span class="form-label">{label}</span>
      <input
        type="number"
        class="form-input"
        min={min as i32}
        max={max as i32}
        aria-label={label}
        prop:value={move || value.get().to_string()}
        on:input=move |ev| {
          if let Ok(v) = event_target_value(&ev).parse::<u16>() {
            value.set(v.clamp(min, max));
          }
        }
      />
    </label>
  }
}

// ============================================================
// 表单值 → CommandBody
// ============================================================

#[allow(clippy::too_many_arguments)]
fn build_body(
  ty: &str,
  led_idx: u8,
  led_count: u8,
  led_period: u16,
  joy_scale: u16,
  knob_scale: u16,
  toast_text: &str,
  bat_simulate: bool,
) -> Option<CommandBody> {
  match ty {
    "Nop" => Some(CommandBody::Nop),
    "LedBlink" => Some(CommandBody::LedBlink {
      led_idx,
      count: led_count,
      period_ms: led_period,
    }),
    "SetSensitivity" => Some(CommandBody::SetSensitivity {
      joy_scale,
      knob_scale,
    }),
    "ShowToast" => {
      let text_bytes = toast_text.as_bytes();
      let len = text_bytes.len().min(5) as u8;
      let mut bytes = [0_u8; 5];
      for (i, b) in text_bytes.iter().take(5).enumerate() {
        bytes[i] = *b;
      }
      Some(CommandBody::ShowToast { len, bytes })
    }
    "SetBatteryMode" => Some(CommandBody::SetBatteryMode {
      simulate: bat_simulate,
    }),
    _ => None,
  }
}

// ============================================================
// Event 辅助（Leptos 内置的没有，自行封装）
// ============================================================

fn event_target_value(ev: &web_sys::Event) -> String {
  use wasm_bindgen::JsCast;
  ev.target()
    .and_then(|t| {
      t.dyn_into::<web_sys::HtmlInputElement>()
        .ok()
        .map(|el| el.value())
    })
    .or_else(|| {
      ev.target().and_then(|t| {
        t.dyn_into::<web_sys::HtmlSelectElement>()
          .ok()
          .map(|el| el.value())
      })
    })
    .unwrap_or_default()
}

fn event_target_checked(ev: &web_sys::Event) -> bool {
  use wasm_bindgen::JsCast;
  ev.target()
    .and_then(|t| {
      t.dyn_into::<web_sys::HtmlInputElement>()
        .ok()
        .map(|el| el.checked())
    })
    .unwrap_or(false)
}
