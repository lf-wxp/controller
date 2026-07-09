//! # EventLog —— 时序日志 + hex dump
//!
//! 展示 [`crate::state::AppState::events`] 里的所有条目：
//! - 时间戳（毫秒相对启动）
//! - 方向徽章（RX/TX/IN/WN）
//! - 摘要文本
//! - 可展开的原始字节 hex dump

use leptos::prelude::*;

use crate::state::{AppState, EventEntry};

#[component]
pub fn EventLog() -> impl IntoView {
  let state = expect_context::<AppState>();

  let handle_clear_click = move |_| {
    state.events.update(|events| events.clear());
  };

  view! {
    <section class="event-log" aria-label="事件时序日志">
      <div class="event-log-head">
        <h2 class="section-title">"事件日志"</h2>
        <button
          type="button"
          class="clear-btn"
          aria-label="清空事件日志"
          on:click=handle_clear_click
        >
          "清空"
        </button>
      </div>

      <ol class="event-list" role="log" aria-live="polite">
        {move || {
          let events = state.events.get();
          if events.is_empty() {
            view! {
              <li class="event-empty">"（暂无事件；点击右上方连接手柄开始）"</li>
            }.into_any()
          } else {
            events
              .iter()
              .rev()  // 最新在顶部
              .cloned()
              .map(|entry| view! { <EventRow entry /> }.into_any())
              .collect_view()
              .into_any()
          }
        }}
      </ol>
    </section>
  }
}

/// 单条事件行
#[component]
fn EventRow(entry: EventEntry) -> impl IntoView {
  let expanded = RwSignal::new(false);
  let has_bytes = entry.bytes.is_some();

  // 用 StoredValue 让下方多个闭包/view! 位置都能共享同一份不可变数据
  let bytes_store = StoredValue::new(entry.bytes.clone());
  let summary_store = StoredValue::new(entry.summary.clone());
  let badge_class = entry.dir.badge_class();
  let badge_label = entry.dir.label();
  let ts_ms = entry.ts_ms;
  let aria_summary = format!("{} {}", badge_label, entry.summary);

  let handle_toggle_click = move |_| {
    if has_bytes {
      expanded.update(|v| *v = !*v);
    }
  };

  let handle_key_down = move |ev: web_sys::KeyboardEvent| {
    if !has_bytes {
      return;
    }
    if ev.key() == "Enter" || ev.key() == " " {
      ev.prevent_default();
      expanded.update(|v| *v = !*v);
    }
  };

  view! {
    <li class="event-row" role="listitem">
      <div
        class="event-head"
        tabindex={if has_bytes { "0" } else { "-1" }}
        role={if has_bytes { "button" } else { "presentation" }}
        aria-expanded={move || if has_bytes { expanded.get().to_string() } else { "false".into() }}
        aria-label={aria_summary}
        on:click=handle_toggle_click
        on:keydown=handle_key_down
      >
        <span class={format!("badge {}", badge_class)}>{badge_label}</span>
        <span class="event-time mono">{format!("{ts_ms:>10.1}ms")}</span>
        <span class="event-summary">{move || summary_store.get_value()}</span>
        <Show when=move || has_bytes fallback=|| ()>
          <span class="expand-caret" aria-hidden="true">
            {move || if expanded.get() { "▼" } else { "▶" }}
          </span>
        </Show>
      </div>

      <Show
        when=move || expanded.get() && bytes_store.with_value(|b| b.is_some())
        fallback=|| ()
      >
        <pre class="hex-dump mono" aria-label="十六进制字节">
          {move || bytes_store.with_value(|b| b.as_ref().map(|v| hex_dump(v)).unwrap_or_default())}
        </pre>
      </Show>
    </li>
  }
}

/// 把字节序列格式化为 "00 01 02 ...  (len=N)" 的多行 hex dump
fn hex_dump(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len() * 4);
  for (i, b) in bytes.iter().enumerate() {
    if i > 0 && i % 8 == 0 {
      out.push('\n');
    }
    out.push_str(&format!("{b:02x} "));
  }
  out.push_str(&format!("\n[len={}]", bytes.len()));
  out
}
