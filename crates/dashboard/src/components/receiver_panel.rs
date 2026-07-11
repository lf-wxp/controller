//! # ReceiverPanel —— 只读展示已发现的接收方列表
//!
//! 数据来源：手柄将各 receiver 的 `AnnounceReply` 经 BLE 转发给 dashboard，
//! [`crate::bluetooth::on_control_response`] 解码后调用
//! [`AppState::upsert_receiver`] 落入 `receivers` 目录。本面板只做**只读展示**，
//! 不做目标选择（选择是控制器 OLED selector 的职责，见 `src/ui/selector.rs`）。
//!
//! 展示列：id / role / MAC / RSSI（颜色按强弱）/ 最后在线（X s 前）。

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::state::{AppState, PeerInfo, RSSI_UNKNOWN};

/// RSSI 强弱 → 颜色 class（越绿越强，越红越弱，未知灰）
#[must_use]
const fn rssi_class(rssi_dbm: i8) -> &'static str {
  if rssi_dbm == RSSI_UNKNOWN {
    return "rssi-unknown";
  }
  match rssi_dbm {
    ..=-66 => "rssi-weak",
    -65..=-41 => "rssi-medium",
    // >= -40（含正值）均为强信号；正值虽异常但表示极强，归入 strong
    -40.. => "rssi-strong",
  }
}

/// RSSI 文本（未知显示 "--"）
#[must_use]
fn rssi_text(rssi_dbm: i8) -> String {
  if rssi_dbm == RSSI_UNKNOWN {
    "--".into()
  } else {
    format!("{rssi_dbm}")
  }
}

/// 距 last_seen 过去的秒数（向下取整，最小 0）
#[must_use]
fn seconds_since(last_seen_ms: f64) -> u64 {
  let elapsed = crate::state::now_ms() - last_seen_ms;
  elapsed.max(0.0) as u64 / 1000
}

/// 创建一个每秒递增的 tick 信号，驱动"最后在线"文字实时刷新。
///
/// `seconds_since()` 内部调用 `now_ms()` 不是 Leptos Signal，面板唯一的响应式
/// 依赖是 `receivers`。若无新 `AnnounceReply` 到达则面板不重绘、"Xs 前"冻结。
/// 此 tick 信号让渲染闭包每秒强制重绘，保证"最后在线"实时更新。
///
/// **生命周期管理**：`Closure` 经 [`Closure::forget`] 交给 JS 持有（`dyn FnMut`
/// 不实现 `Sync`，无法装入要求 `Send + Sync` 的 [`on_cleanup`] 闭包中），避免
/// 组件函数返回后 `Closure` 被 Rust 侧 drop 导致定时器立即失效。组件卸载时由
/// [`on_cleanup`] 调用 `clearInterval` 停止调用，JS 侧对 closure 的引用随之断开。
fn create_second_tick() -> ReadSignal<u64> {
  let (tick, set_tick) = signal(0u64);

  let closure = Closure::<dyn FnMut()>::new(move || {
    set_tick.update(|t| *t = t.saturating_add(1));
  });

  if let Some(win) = web_sys::window() {
    let cb = closure.as_ref().unchecked_ref::<js_sys::Function>();
    if let Ok(id) = win.set_interval_with_callback_and_timeout_and_arguments_0(cb, 1000) {
      // 交给 JS 持有，使定时器在整个组件生命周期内有效
      closure.forget();
      on_cleanup(move || {
        win.clear_interval_with_handle(id);
      });
    }
  }

  tick
}

#[component]
pub fn ReceiverPanel() -> impl IntoView {
  let state = expect_context::<AppState>();

  // 每秒 tick：驱动"最后在线"文字实时刷新（即使无新 AnnounceReply）
  let tick = create_second_tick();

  // 订阅 receivers 信号：列表变化时自动重绘
  let receivers = move || state.receivers.get();

  view! {
    <section class="receiver-panel" aria-label="已发现的接收方">
      <h2 class="section-title">"接收方列表 (" {move || receivers().len()} ")"</h2>

      <Show
        when=move || !receivers().is_empty()
        fallback=|| view! {
          <p class="receiver-empty">"暂无接收方。手柄会经 BLE 转发 AnnounceReply。"</p>
        }
      >
        <ul class="receiver-list" role="list">
          <For
            // 同时订阅 tick 与 receivers：tick 每秒变化触发列表重建，
            // 使"Xs 前"实时刷新；receivers 变化则立即重绘。
            each=move || {
              tick.get();
              receivers()
            }
            key=|peer: &PeerInfo| peer.receiver_id
            children=move |peer: PeerInfo| {
              let id = peer.receiver_id;
              let mac = peer.mac_str();
              let role = peer.role_str().to_string();
              let rssi = peer.rssi_dbm;
              let last_seen = peer.last_seen_ms;
              view! {
                <li class="receiver-row" aria-label=format!("接收方 {} {}", id, peer.role_str())>
                  <span class="receiver-id mono" aria-label="接收方 ID">{id}</span>
                  <span class="receiver-role">{role}</span>
                  <span class="receiver-mac mono" aria-label="MAC 地址">{mac}</span>
                  <span
                    class={format!("receiver-rssi mono {}", rssi_class(rssi))}
                    aria-label="信号强度 dBm"
                  >
                    {rssi_text(rssi)} "dBm"
                  </span>
                  <span class="receiver-seen mono" aria-label="最后在线">
                    {format!("{}s 前", seconds_since(last_seen))}
                  </span>
                </li>
              }
            }
          />
        </ul>
      </Show>
    </section>
  }
}
