//! # StatusPanel —— 顶栏连接状态
//!
//! 展示：品牌、连接状态（带光晕状态药丸）、电量、Session、帧序号、连接按钮。

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

  // 连接状态药丸：单独的派生信号，绑定到 `state.conn`，
  // 连接状态变化时只重绘药丸，不波及其它指标。
  let conn_class = move || format!("conn-pill {}", state.conn.get().css_state());
  let conn_label = move || state.conn.get().label();

  view! {
    <header class="status-panel" aria-label="连接状态栏">
      <div class="brand">
        <span class="brand-emoji" aria-hidden="true">"🎮"</span>
        <h1 class="brand-title">"ESP32 Controller"</h1>
      </div>

      // 连接状态：突出的彩色药丸（最关键的信息）
      <div
        class=conn_class
        aria-live="polite"
        role="status"
      >
        <span class="conn-dot" aria-hidden="true"></span>
        <span class="conn-text">{conn_label}</span>
      </div>

      <div class="status-cells">
        // 电量
        <div class="status-metric" aria-label="电量">
          <span class="metric-icon" aria-hidden="true">"🔋"</span>
          <span class="metric-body">
            <span class="metric-label">"电量"</span>
            <span class="metric-value">
              {move || match state.battery.get() {
                Some(p) => format!("{p}%"),
                None => "--".into(),
              }}
            </span>
          </span>
        </div>

        // Session nonce
        <div class="status-metric" aria-label="会话密钥">
          <span class="metric-icon" aria-hidden="true">"🔑"</span>
          <span class="metric-body">
            <span class="metric-label">"会话"</span>
            <span class="metric-value mono">
              {move || match state.session_nonce.get() {
                Some(n) => format!("0x{n:08x}"),
                None => "等待中".into(),
              }}
            </span>
          </span>
        </div>

        // Frame seq
        <div class="status-metric" aria-label="最新帧序号">
          <span class="metric-icon" aria-hidden="true">"🎯"</span>
          <span class="metric-body">
            <span class="metric-label">"帧序号"</span>
            <span class="metric-value mono">
              {move || format!("{}", state.last_frame_seq.get())}
            </span>
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
          ConnState::Connected => "断开连接",
        }}
      </button>
    </header>
  }
}
