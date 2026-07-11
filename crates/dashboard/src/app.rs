//! # App —— 顶层组件
//!
//! 负责：
//! - 初始化 [`AppState`] 并通过 context 注入
//! - 布局：StatusPanel 顶栏 + 左侧 GamepadVisual + 中间 CommandPanel + 右侧 EventLog

use leptos::prelude::*;

use crate::bluetooth::StoredHandles;
use crate::components::{
  command_panel::CommandPanel, event_log::EventLog, gamepad_visual::GamepadVisual,
  receiver_panel::ReceiverPanel, status_panel::StatusPanel,
};
use crate::state::AppState;

#[component]
pub fn App() -> impl IntoView {
  // 顶层初始化 —— 全组件树共享同一份 AppState
  let state = AppState::new();
  provide_context(state);

  // BluetoothManager 的连接句柄 —— 用 LocalStorage 版本的 RwSignal
  //
  // # 为什么必须用 LocalStorage？
  // [`StoredHandles`] 内部持有 `Rc<NotifySubscription>`（`Closure` + JS 对象），
  // 都是 `!Send + !Sync` 的 wasm 单线程类型。默认的 `RwSignal<T> = RwSignal<T, SyncStorage>`
  // 要求 `T: Send + Sync`，不满足；`RwSignal::new_local` 使用 `LocalStorage`，
  // 允许存 `!Send + !Sync` 数据，正好适配 wasm 单线程模型。
  let handles: RwSignal<Option<StoredHandles>, LocalStorage> = RwSignal::new_local(None);

  view! {
    <div class="app-shell">
      <StatusPanel handles />

      <main class="app-main">
        <GamepadVisual />
        <CommandPanel handles />
        <ReceiverPanel />
        <EventLog />
      </main>

      <footer class="app-footer">
        <p class="footer-note">
          "🎮 controller-dashboard · 使用同一 controller-protocol crate 与 ESP32 手柄通信"
        </p>
        <p class="footer-note">
          "浏览器需支持 WebBluetooth（Chrome / Edge 最新版）；地址必须是 "
          <code>"localhost"</code> " 或 " <code>"https://"</code>
        </p>
      </footer>
    </div>
  }
}
