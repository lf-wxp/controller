//! # controller-dashboard
//!
//! ESP32 Controller 的 **WebBluetooth 调试面板**。
//! 用 Leptos 0.8 CSR 模式，编译到 WASM 后由 Trunk 打包成静态站点。
//!
//! ## 架构
//! ```text
//!  Chrome/Edge (WebBluetooth) ──navigator.bluetooth──► ESP32 手柄
//!         │
//!         │ notify/write 事件
//!         ▼
//!   BluetoothManager (bluetooth.rs)
//!         │
//!         │ 更新 signal
//!         ▼
//!   AppState (state.rs, RwSignal)
//!         │
//!         │ 响应式 UI 刷新
//!         ▼
//!   Leptos 组件 (components/*.rs)
//! ```
//!
//! ## 与 esp32 端共享的协议逻辑
//! `protocol` crate 提供 encode/decode 的**唯一权威实现**。
//! 手柄 fw 与 dashboard 编码/解码同一份 Rust 代码 → 协议漂移 = 0。

use leptos::mount::mount_to_body;

mod app;
mod bluetooth;
mod components;
mod state;

fn main() {
  // panic 时打印栈到 devtools console
  console_error_panic_hook::set_once();

  // 桥接 Rust log → 浏览器 devtools
  console_log::init_with_level(log::Level::Info).expect("console_log init");

  log::info!(
    "controller-dashboard v{} starting",
    env!("CARGO_PKG_VERSION")
  );

  mount_to_body(app::App);
}
