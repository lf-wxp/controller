//! # StatusPanel —— 顶栏连接状态
//!
//! 展示：连接按钮、连接状态点、电量、session nonce、当前 key_id。

use leptos::prelude::*;

use crate::bluetooth::{StoredHandles, disconnect, request_and_connect};
use crate::state::{AppState, ConnState};

#[component]
pub fn StatusPanel(handles: RwSignal<Option<StoredHandles>, LocalStorage>) -> impl IntoView {
  let state = expect_context::<AppState>();

  let handle_connect_click = move |_| {
    if matches!(state.conn.get_untracked(), ConnState::Connected) {
      disconnect(state, handles);
    } else {
      request_and_connect(state, handles);
    }
  };

  view! {
    <header class="status-panel" aria-label="连接状态栏">
      <div class="brand">
        <span class="brand-emoji" aria-hidden="true">"🎮"</span>
        <h1 class="brand-title">"ESP32 Controller Dashboard"</h1>
      </div>

      <div class="status-cells">
        // 连接状态
        <div class="status-cell" aria-live="polite">
          <span class={move || format!("dot {}", state.conn.get().dot_class())}
                aria-hidden="true"></span>
          <span class="cell-label">{move || state.conn.get().label()}</span>
        </div>

        // 电量
        <div class="status-cell" aria-label="电量">
          <span class="cell-icon" aria-hidden="true">"🔋"</span>
          <span class="cell-label">
            {move || match state.battery.get() {
              Some(p) => format!("{p}%"),
              None => "--".into(),
            }}
          </span>
        </div>

        // Session nonce
        <div class="status-cell" aria-label="Session nonce">
          <span class="cell-icon" aria-hidden="true">"🔑"</span>
          <span class="cell-label mono">
            {move || match state.session_nonce.get() {
              Some(n) => format!("0x{n:08x}"),
              None => "waiting".into(),
            }}
          </span>
        </div>

        // Frame seq
        <div class="status-cell" aria-label="最新帧序号">
          <span class="cell-icon" aria-hidden="true">"🎯"</span>
          <span class="cell-label mono">
            {move || format!("seq={}", state.last_frame_seq.get())}
          </span>
        </div>
      </div>

      <button
        type="button"
        class={move || format!("connect-btn {}",
          if matches!(state.conn.get(), ConnState::Connected) { "connect-btn-active" } else { "" })}
        aria-label={move || match state.conn.get() {
          ConnState::Connected => "断开手柄连接",
          _ => "连接手柄",
        }}
        on:click=handle_connect_click
        prop:disabled={move || matches!(state.conn.get(), ConnState::Connecting)}
      >
        {move || match state.conn.get() {
          ConnState::Disconnected => "连接手柄",
          ConnState::Connecting => "连接中...",
          ConnState::Connected => "断开",
        }}
      </button>
    </header>
  }
}
