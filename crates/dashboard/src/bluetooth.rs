//! # WebBluetooth 封装
//!
//! `web-sys` 目前的稳定版本**不导出 WebBluetooth API**（浏览器端仍是实验性）。
//! 我们用 `wasm_bindgen(extern)` 直接手工绑定 JS 全局对象上的方法：
//!
//! ```text
//!   navigator.bluetooth.requestDevice({ ... })
//!   device.gatt.connect()
//!   server.getPrimaryService(uuid)
//!   service.getCharacteristic(uuid)
//!   char.startNotifications() / addEventListener('characteristicvaluechanged', ...)
//!   char.writeValue(bytes)
//! ```
//!
//! ## 与 ESP32 手柄的 GATT 对接
//! ```text
//!   Service: c7000001-1e00-4000-8000-0000000000cc
//!   ├── FrameStream  : c7000002-... [read/notify, 21 bytes]
//!   ├── ControlCommand: c7000003-... [write, 20 bytes]
//!   └── ControlResponse: c7000004-... [read/notify, 20 bytes]
//! ```
//!
//! ## 使用流程
//! 1. 用户点击"连接"按钮 → [`request_and_connect`]（弹出浏览器选择器）
//! 2. 连接成功后订阅 `FrameStream` + `ControlResponse` 的 notify
//! 3. Notify 事件回调把字节交给 [`crate::state::AppState`] 更新
//! 4. 用户在 UI 触发 [`send_command`] 时通过 `ControlCommand` 写入手柄
//!
//! ## 安全模型（**重要**）
//! Dashboard 是**信任端**：手柄端做严格的 HMAC + 抗重放校验，Host 侧默认
//! **不做**二次 HMAC 校验，只做长度 + CRC + magic 检查。原因如下：
//!
//! - 攻击者若能伪造 BLE 广播冒充手柄，需要先物理靠近用户并压制真手柄信号；
//!   在此攻击模型下，用户操作意图已经不可信，二次校验意义有限
//! - 加入 HMAC 校验需要 dashboard 侧也持有共享密钥；WebBluetooth 页面部署在
//!   浏览器里，密钥落到 JS bundle 中反而**降低整体安全性**（见 C-3 修复注释）
//! - Response body 里的 `req_seq` 与本地发送的 `seq` 序列**必须由调用方比对**，
//!   本模块只负责传输 + 解码
//!
//! 如果后续需要抗中间人劫持的双向认证，请在**外层**（例如 WebAuthn / TLS to
//! backend）解决，而不是在这里加密钥。

use controller_protocol::{
  Command, FRAME_LEN, RESPONSE_LEN, ResponseBody, decode_frame, decode_response, encode_command,
};
use js_sys::{Array, Object, Reflect, Uint8Array};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::JsFuture;

use crate::state::{AppState, ConnState, EventEntry, error_code_label};

/// UUID 常量 —— 与手柄端 `crate::transport::ble_hid::service` 对齐
mod uuids {
  pub const DEVICE_NAME: &str = "ESP32-Controller";
  pub const CUSTOM_SERVICE: &str = "c7000001-1e00-4000-8000-0000000000cc";
  pub const FRAME_STREAM: &str = "c7000002-1e00-4000-8000-0000000000cc";
  pub const CONTROL_COMMAND: &str = "c7000003-1e00-4000-8000-0000000000cc";
  pub const CONTROL_RESPONSE: &str = "c7000004-1e00-4000-8000-0000000000cc";
}

// ============================================================
// WebBluetooth API 手工声明（wasm_bindgen extern）
// ============================================================
//
// # 为什么 `navigator.bluetooth` 用 fn getter 而不是 `#[wasm_bindgen] extern static`？
// 之前的写法：
// ```rust
// #[wasm_bindgen(thread_local_v2, js_namespace = navigator, js_name = bluetooth)]
// static NAVIGATOR_BLUETOOTH: JsValue;
// ```
// 编译器（rustc + wasm_bindgen 宏展开）能正确处理，但 **rust-analyzer** 只看
// 宏展开前的语法，会误报 `use of extern static is unsafe`。为兼顾 IDE 体验，
// 改用 `web_sys::window().navigator()` + `js_sys::Reflect::get` 动态获取 —
// 运行时开销可忽略（`do_connect` 只在用户点连接时调用一次）。

/// 获取 `navigator.bluetooth` 全局对象
///
/// - `Some(JsValue)`：浏览器支持 WebBluetooth
/// - `None`：浏览器不支持（Firefox / Safari / 未启用 flag 的 Chrome）
fn navigator_bluetooth() -> Option<JsValue> {
  let win = web_sys::window()?;
  let navigator = win.navigator();
  let value = Reflect::get(&navigator, &JsValue::from_str("bluetooth")).ok()?;
  if value.is_undefined() || value.is_null() {
    None
  } else {
    Some(value)
  }
}

#[wasm_bindgen]
extern "C" {
  /// 蓝牙设备（`BluetoothDevice`）
  #[wasm_bindgen(js_name = BluetoothDevice)]
  #[derive(Clone)]
  type BluetoothDevice;

  #[wasm_bindgen(method, getter)]
  fn gatt(this: &BluetoothDevice) -> Option<GattServer>;

  #[wasm_bindgen(method, getter)]
  fn name(this: &BluetoothDevice) -> Option<String>;

  /// GATT 服务器
  #[wasm_bindgen(js_name = BluetoothRemoteGATTServer)]
  #[derive(Clone)]
  type GattServer;

  #[wasm_bindgen(method)]
  fn connect(this: &GattServer) -> js_sys::Promise;

  #[wasm_bindgen(method)]
  fn disconnect(this: &GattServer);

  #[wasm_bindgen(method, js_name = getPrimaryService)]
  fn get_primary_service(this: &GattServer, uuid: &str) -> js_sys::Promise;

  /// GATT service
  #[wasm_bindgen(js_name = BluetoothRemoteGATTService)]
  #[derive(Clone)]
  type GattService;

  #[wasm_bindgen(method, js_name = getCharacteristic)]
  fn get_characteristic(this: &GattService, uuid: &str) -> js_sys::Promise;

  /// GATT characteristic
  #[wasm_bindgen(js_name = BluetoothRemoteGATTCharacteristic, extends = web_sys::EventTarget)]
  #[derive(Clone)]
  type GattCharacteristic;

  #[wasm_bindgen(method, getter)]
  fn value(this: &GattCharacteristic) -> Option<js_sys::DataView>;

  #[wasm_bindgen(method, js_name = startNotifications)]
  fn start_notifications(this: &GattCharacteristic) -> js_sys::Promise;

  #[wasm_bindgen(method, js_name = writeValue)]
  fn write_value(this: &GattCharacteristic, value: &Uint8Array) -> js_sys::Promise;
}

// ============================================================
// GATT 句柄集合
// ============================================================

/// 每个 notify characteristic 订阅后返回的"资源包"
///
/// 把 [`Closure`] 与它绑定的 [`GattCharacteristic`] 打包在一起，保证：
/// - 只要本包被持有，闭包就活着，浏览器 notify 就能正确回调；
/// - 本包被 `drop` 时，通过 [`Drop`] 从事件目标上取消监听并释放闭包，
///   彻底避免多次连接同一设备时闭包无限累积（原实现 `Closure::forget()`
///   导致的软内存泄漏）。
struct NotifySubscription {
  target: GattCharacteristic,
  closure: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for NotifySubscription {
  fn drop(&mut self) {
    // 主动摘掉事件监听：即便 characteristic 稍后被 GC，中途也不会再收到回调
    let event_target: &web_sys::EventTarget = self.target.unchecked_ref();
    // `remove_event_listener_with_callback` 返回 Result 但断连时 characteristic
    // 可能已失效——忽略错误即可，闭包 drop 才是关键
    let _ = event_target.remove_event_listener_with_callback(
      "characteristicvaluechanged",
      self.closure.as_ref().unchecked_ref(),
    );
  }
}

/// 连接后持有的 GATT 句柄集合（[`StoredValue`] 让它可以跨闭包 `Clone`）
///
/// # 生命周期
/// - `Clone` 是**浅拷贝**：`BluetoothDevice` / `GattServer` / `GattCharacteristic`
///   本身是 JS 对象的引用，克隆只增加引用计数
/// - notify 订阅用 [`Rc`] 共享，disconnect 时最后一个引用消失即触发
///   [`NotifySubscription::drop`]，闭包被彻底释放
#[derive(Clone)]
pub struct StoredHandles {
  device: BluetoothDevice,
  server: GattServer,
  command_char: GattCharacteristic,
  // Rc 共享而非 Clone：闭包本身不可 Clone，用 Rc 让多份 StoredHandles 共享
  // 同一份订阅，最后一份释放才 drop
  _frame_sub: Rc<NotifySubscription>,
  _response_sub: Rc<NotifySubscription>,
}

// ============================================================
// 公开 API
// ============================================================

/// 触发浏览器"选择蓝牙设备"弹窗并连接
///
/// **必须在用户交互（如点击按钮）中调用** —— WebBluetooth 要求用户手势才能弹出选择器。
pub fn request_and_connect(
  state: AppState,
  handles: RwSignal<Option<StoredHandles>, LocalStorage>,
) {
  spawn_local(async move {
    state.conn.set(ConnState::Connecting);
    state.push_event(EventEntry::info("请求蓝牙设备..."));

    match do_connect(state).await {
      Ok(h) => {
        let device_name = h.device.name().unwrap_or_else(|| "unknown".into());
        state.conn.set(ConnState::Connected);
        state.push_event(EventEntry::info(format!("已连接: {device_name}")));
        handles.set(Some(h));
      }
      Err(err) => {
        state.conn.set(ConnState::Disconnected);
        state.push_event(EventEntry::warn(format!("连接失败: {err}")));
      }
    }
  });
}

/// 断开当前连接
pub fn disconnect(state: AppState, handles: RwSignal<Option<StoredHandles>, LocalStorage>) {
  if let Some(h) = handles.get_untracked() {
    h.server.disconnect();
    handles.set(None);
    state.conn.set(ConnState::Disconnected);
    state.push_event(EventEntry::info("已断开连接"));
  }
}

/// 通过 ControlCommand characteristic 发送一条命令
///
/// 不阻塞 UI —— 内部 `spawn_local` 异步写入。
///
/// # 事件日志时序（P0-2 加固）
/// 旧实现在**尚未写入**时就 push 了 tx 事件，若 `writeValue` 失败会误导调试
/// （看起来发出去了但对面没收到）。现在改为：
/// - 写入成功 → push `EventEntry::tx`（真的发出去了）
/// - 写入失败 → push `EventEntry::warn`（清晰指出失败原因）
pub fn send_command(
  state: AppState,
  handles: RwSignal<Option<StoredHandles>, LocalStorage>,
  cmd: Command,
) {
  let Some(h) = handles.get_untracked() else {
    state.push_event(EventEntry::warn("未连接，无法发送命令"));
    return;
  };
  let bytes = encode_command(&cmd);
  let summary = format!("Command seq={} kid={:?}", cmd.seq, cmd.key_id);

  spawn_local(async move {
    let js_buf = Uint8Array::from(bytes.as_slice());
    let promise = h.command_char.write_value(&js_buf);
    match JsFuture::from(promise).await {
      Ok(_) => {
        // 只有写入真的成功了才记录 tx 事件——避免日志与实际状态不一致
        state.push_event(EventEntry::tx(summary, bytes.to_vec()));
      }
      Err(err) => {
        state.push_event(EventEntry::warn(format!(
          "写入 ControlCommand 失败: {err:?}"
        )));
      }
    }
  });
}

// ============================================================
// 连接内部实现
// ============================================================

async fn do_connect(state: AppState) -> Result<StoredHandles, String> {
  // 取 navigator.bluetooth（Option 已包含"存在且非 undefined/null"语义）
  let bluetooth = navigator_bluetooth()
    .ok_or_else(|| "浏览器不支持 WebBluetooth（请用最新版 Chrome/Edge）".to_string())?;

  // 构造 requestDevice 参数：{ filters: [{ name: '...' }], optionalServices: ['<uuid>'] }
  let filter = Object::new();
  Reflect::set(
    &filter,
    &JsValue::from_str("name"),
    &JsValue::from_str(uuids::DEVICE_NAME),
  )
  .map_err(|e| format!("设置 filter.name 失败: {e:?}"))?;
  let filters = Array::new();
  filters.push(&filter);

  let optional_services = Array::new();
  optional_services.push(&JsValue::from_str(uuids::CUSTOM_SERVICE));

  let opts = Object::new();
  Reflect::set(&opts, &JsValue::from_str("filters"), &filters)
    .map_err(|e| format!("设置 opts.filters 失败: {e:?}"))?;
  Reflect::set(
    &opts,
    &JsValue::from_str("optionalServices"),
    &optional_services,
  )
  .map_err(|e| format!("设置 opts.optionalServices 失败: {e:?}"))?;

  // requestDevice(opts) —— 通过 Reflect 调用（extern type 未导出）
  let request_device_fn = Reflect::get(&bluetooth, &JsValue::from_str("requestDevice"))
    .map_err(|e| format!("查找 requestDevice 失败: {e:?}"))?;
  let request_device_fn: js_sys::Function = request_device_fn
    .dyn_into()
    .map_err(|_| "requestDevice 不是函数".to_string())?;
  let device_promise: js_sys::Promise = request_device_fn
    .call1(&bluetooth, &opts)
    .map_err(|e| format!("调用 requestDevice 失败: {e:?}"))?
    .dyn_into()
    .map_err(|_| "requestDevice 返回值不是 Promise".to_string())?;

  let device_val = JsFuture::from(device_promise)
    .await
    .map_err(|e| format!("用户取消或 requestDevice 失败: {e:?}"))?;
  let device: BluetoothDevice = device_val.unchecked_into();

  // 连接 GATT
  let server = device
    .gatt()
    .ok_or_else(|| "device.gatt 缺失".to_string())?;
  let server_val = JsFuture::from(server.connect())
    .await
    .map_err(|e| format!("GATT connect 失败: {e:?}"))?;
  let server: GattServer = server_val.unchecked_into();

  // 拿服务
  let service_val = JsFuture::from(server.get_primary_service(uuids::CUSTOM_SERVICE))
    .await
    .map_err(|e| format!("获取 CustomService 失败: {e:?}"))?;
  let service: GattService = service_val.unchecked_into();

  // 拿三个 characteristic
  let frame_char: GattCharacteristic =
    JsFuture::from(service.get_characteristic(uuids::FRAME_STREAM))
      .await
      .map_err(|e| format!("获取 FrameStream 失败: {e:?}"))?
      .unchecked_into();

  let response_char: GattCharacteristic =
    JsFuture::from(service.get_characteristic(uuids::CONTROL_RESPONSE))
      .await
      .map_err(|e| format!("获取 ControlResponse 失败: {e:?}"))?
      .unchecked_into();

  let command_char: GattCharacteristic =
    JsFuture::from(service.get_characteristic(uuids::CONTROL_COMMAND))
      .await
      .map_err(|e| format!("获取 ControlCommand 失败: {e:?}"))?
      .unchecked_into();

  // 订阅 notify（把订阅资源包收进 StoredHandles，让 disconnect 时 Drop 释放闭包）
  let frame_sub = Rc::new(subscribe_frame(state, frame_char).await?);
  let response_sub = Rc::new(subscribe_response(state, response_char).await?);

  Ok(StoredHandles {
    device,
    server,
    command_char,
    _frame_sub: frame_sub,
    _response_sub: response_sub,
  })
}

async fn subscribe_frame(
  state: AppState,
  ch: GattCharacteristic,
) -> Result<NotifySubscription, String> {
  JsFuture::from(ch.start_notifications())
    .await
    .map_err(|e| format!("startNotifications(Frame) 失败: {e:?}"))?;

  let ch_clone = ch.clone();
  let on_change = Closure::<dyn FnMut(_)>::new(move |_ev: web_sys::Event| {
    let Some(value) = ch_clone.value() else {
      return;
    };
    let bytes = data_view_to_vec(&value);
    if bytes.len() != FRAME_LEN {
      state.push_event(EventEntry::warn(format!(
        "Frame 长度异常: {} != {FRAME_LEN}",
        bytes.len()
      )));
      return;
    }
    match decode_frame(&bytes) {
      Ok(frame) => {
        state.gamepad.set(frame.payload);
        state.last_frame_seq.set(frame.header.seq);
      }
      Err(err) => {
        state.push_event(EventEntry::warn(format!("decode Frame 失败: {err:?}")));
      }
    }
  });

  let event_target: &web_sys::EventTarget = ch.unchecked_ref();
  event_target
    .add_event_listener_with_callback(
      "characteristicvaluechanged",
      on_change.as_ref().unchecked_ref(),
    )
    .map_err(|e| format!("Frame 事件绑定失败: {e:?}"))?;

  // 把闭包与 characteristic 一起打包返回；调用方持有它 → notify 存活
  Ok(NotifySubscription {
    target: ch,
    closure: on_change,
  })
}

async fn subscribe_response(
  state: AppState,
  ch: GattCharacteristic,
) -> Result<NotifySubscription, String> {
  JsFuture::from(ch.start_notifications())
    .await
    .map_err(|e| format!("startNotifications(Response) 失败: {e:?}"))?;

  let ch_clone = ch.clone();
  let on_change = Closure::<dyn FnMut(_)>::new(move |_ev: web_sys::Event| {
    let Some(value) = ch_clone.value() else {
      return;
    };
    let bytes = data_view_to_vec(&value);
    if bytes.len() != RESPONSE_LEN {
      state.push_event(EventEntry::warn(format!(
        "Response 长度异常: {} != {RESPONSE_LEN}",
        bytes.len()
      )));
      return;
    }
    match decode_response(&bytes) {
      Ok(resp) => {
        let summary = match &resp.body {
          ResponseBody::Ack => format!("Ack req_seq={}", resp.req_seq),
          ResponseBody::Error(code) => {
            format!(
              "Error req_seq={} → {}",
              resp.req_seq,
              error_code_label(*code)
            )
          }
          ResponseBody::BatterySnapshot { percent } => {
            state.battery.set(Some(*percent));
            format!("Battery={percent}%")
          }
          ResponseBody::NonceHello { nonce } => {
            state.session_nonce.set(Some(*nonce));
            format!("NonceHello=0x{nonce:08x}")
          }
          ResponseBody::AnnounceReply {
            mac,
            rssi_dbm,
            role_tag,
          } => {
            // 落进 receivers 目录（自动分配 id；字段对齐控制器 PeerRegistry）
            // mac / role_tag 已是定长数组引用（&[u8; N]），直接解引用拷贝即可
            state.upsert_receiver(*mac, *role_tag, *rssi_dbm);

            // 同时保留一行可读事件日志
            let role_str = core::str::from_utf8(role_tag).unwrap_or("???");
            format!(
              "AnnounceReply mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} rssi={}dBm role={}",
              mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], rssi_dbm, role_str
            )
          }
        };
        state.last_response.set(Some((resp.req_seq, resp.body)));
        state.push_event(EventEntry::rx(summary, bytes.clone()));
      }
      Err(err) => {
        state.push_event(EventEntry::warn(format!("decode Response 失败: {err:?}")));
      }
    }
  });

  let event_target: &web_sys::EventTarget = ch.unchecked_ref();
  event_target
    .add_event_listener_with_callback(
      "characteristicvaluechanged",
      on_change.as_ref().unchecked_ref(),
    )
    .map_err(|e| format!("Response 事件绑定失败: {e:?}"))?;

  Ok(NotifySubscription {
    target: ch,
    closure: on_change,
  })
}

/// 把 [`js_sys::DataView`] 转为 `Vec<u8>`
fn data_view_to_vec(view: &js_sys::DataView) -> Vec<u8> {
  let len = view.byte_length();
  let mut out = Vec::with_capacity(len);
  for i in 0..len {
    out.push(view.get_uint8(i));
  }
  out
}
