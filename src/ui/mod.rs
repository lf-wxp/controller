//! # OLED UI 显示层
//!
//! ## 职责
//! - 从主循环收到最新 [`Frame`] 后，把手柄状态实时渲染到 SSD1306 128×64 OLED
//! - 集成传输层健康状态（BLE 是否已连、ESP-NOW 是否可用）与电量
//! - 与其它 Transport 一样通过 [`Transport`] trait 挂到 [`CompositeTransport`]
//!
//! ## 架构（与 BLE / ESP-NOW 保持一致）
//! ```text
//!                        Signal<Frame>
//!  ┌──────────────┐   overwrite-on-write   ┌───────────────────────┐
//!  │  main loop   │─────────────────────► │ oled_task              │
//!  │  transport   │                        │  ├─ 读 shared_state    │
//!  │  .send()     │                        │  ├─ 组装 UiState       │
//!  └──────────────┘                        │  └─ layout::render     │
//!                                          └───────────────────────┘
//! ```
//!
//! ## 连接状态如何进来？
//! 通过一组 `AtomicBool`/`AtomicU8` 供 BLE / ESP-NOW 任务写入。
//! ESP32（xtensa）支持 32 位原子操作，`AtomicBool` / `AtomicU8` 均可无锁使用。
//!
//! ## Toast
//! 接收到 [`crate::transport::control`] 分发的 `ShowToast` 命令时，通过
//! [`TOAST_SIGNAL`] 将短提示传递到 `oled_task`，底部显示 3 秒后自动消失。

pub mod layout;

use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

use crate::hal::battery::{BatteryLevel, battery_level_state};
use crate::protocol::Frame;
use crate::transport::Transport;

// ============================================================
// Toast 提示（Host 下发 ShowToast 命令时弹出）
// ============================================================

/// Toast 最大长度（与 Command::ShowToast.bytes 一致）
pub const TOAST_MAX_LEN: usize = 5;

/// Toast 内容：定长字节 + 有效长度
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toast {
  /// 有效字节数 0..=TOAST_MAX_LEN
  pub len: u8,
  /// ASCII 内容（不满位保留上次值，绘制层必须只看 [..len]）
  pub bytes: [u8; TOAST_MAX_LEN],
}

impl Toast {
  /// 从任意字节 slice 构造（超平会被截断到 TOAST_MAX_LEN）
  pub fn from_slice(src: &[u8]) -> Self {
    let mut bytes = [0_u8; TOAST_MAX_LEN];
    let n = src.len().min(TOAST_MAX_LEN);
    bytes[..n].copy_from_slice(&src[..n]);
    Self {
      len: n as u8,
      bytes,
    }
  }
}

/// Toast 共享通道：命令处理层 → oled_task
pub type ToastSignal = Signal<CriticalSectionRawMutex, Toast>;

/// 全局 Toast 通道（command handler 写入，oled_task 读取）
pub static TOAST_SIGNAL: ToastSignal = Signal::new();

/// Toast 显示时长（毫秒）
pub const TOAST_DURATION_MS: u64 = 3000;

/// 便捷入口：将一段 ASCII 字节推送到屏幕底部显示一下子
///
/// 主要给 [`crate::transport::control::handle_command`] 调用。
pub fn signal_toast(bytes: &[u8]) {
  TOAST_SIGNAL.signal(Toast::from_slice(bytes));
}

// ============================================================
// 共享连接状态（供 BLE / ESP-NOW 任务写入，UI 任务读取）
// ============================================================

/// BLE 连接状态：true = Host 已连接
pub static BLE_CONNECTED: AtomicBool = AtomicBool::new(false);
/// ESP-NOW 是否上线（Wi-Fi 硬件就绪 + 广播任务运行中）
pub static ESP_NOW_READY: AtomicBool = AtomicBool::new(false);
/// 电池百分比 0..=100（未接测量硬件时保持 100）
pub static BATTERY_LEVEL: AtomicU8 = AtomicU8::new(100);
/// Host 心跳存活标志：最近 [`HEARTBEAT_TIMEOUT_MS`] 内收到过任何 Command 则为 true
pub static HOST_HEARTBEAT_ALIVE: AtomicBool = AtomicBool::new(false);
/// Host 心跳超时阈值（毫秒：5 秒一刷心跳，15 秒未收到则视为断开）
pub const HEARTBEAT_TIMEOUT_MS: u64 = 15_000;
/// 最后一次收到 Command 的时间（毫秒，embassy Instant.as_millis 的低 32 位）
///
/// 低 32 位 ≈ 49.7 天回绕，对手柄运行时长无影响。用 [`touch_host_heartbeat`] 写入、
/// 用 [`check_host_heartbeat_liveness`] 检查。
pub static LAST_HEARTBEAT_TICK_MS: AtomicU32 = AtomicU32::new(0);

/// 便捷 setter：BLE 连接建立
pub fn set_ble_connected(connected: bool) {
  BLE_CONNECTED.store(connected, Ordering::Relaxed);
}

/// 便捷 setter：ESP-NOW 就绪
pub fn set_esp_now_ready(ready: bool) {
  ESP_NOW_READY.store(ready, Ordering::Relaxed);
}

/// 便捷 setter：电池电量（0..=100，超范围会 clamp）
pub fn set_battery_level(level: u8) {
  BATTERY_LEVEL.store(level.min(100), Ordering::Relaxed);
}

/// 便捷 setter：Host 心跳存活标志
pub fn set_host_heartbeat_alive(alive: bool) {
  HOST_HEARTBEAT_ALIVE.store(alive, Ordering::Relaxed);
}

/// 发现可用心跳时调用：拍一下时间戳，并设置 alive = true
///
/// 由 [`crate::transport::control::handle_command`] 在每次成功处理命令后调用。
pub fn touch_host_heartbeat() {
  let now_ms = embassy_time::Instant::now().as_millis() as u32;
  LAST_HEARTBEAT_TICK_MS.store(now_ms, Ordering::Relaxed);
  HOST_HEARTBEAT_ALIVE.store(true, Ordering::Relaxed);
}

/// 检查心跳是否超时；若超时则把 `HOST_HEARTBEAT_ALIVE` 置为 false
///
/// 由 `oled_task` 在每次刷屏前调用（依靠满 20 Hz 的刷新频率即可及时发现）。
pub fn check_host_heartbeat_liveness() {
  let last = LAST_HEARTBEAT_TICK_MS.load(Ordering::Relaxed);
  // last == 0 代表从未收到过心跳，保持 alive=false
  if last == 0 {
    return;
  }
  let now_ms = embassy_time::Instant::now().as_millis() as u32;
  // wrapping_sub 自然处理 32 位回绕（不可能刚好在 49.7 天后心跳）
  if now_ms.wrapping_sub(last) as u64 > HEARTBEAT_TIMEOUT_MS {
    HOST_HEARTBEAT_ALIVE.store(false, Ordering::Relaxed);
  }
}

// ============================================================
// UI 帧共享通道
// ============================================================

/// UI 渲染任务共享通道（Signal = "最后写入者赢"）
///
/// 主循环把最新 [`Frame`] 写入，`oled_task` 在自己的节奏里取出、组装 [`UiState`]、绘屏。
pub type UiFrameSignal = Signal<CriticalSectionRawMutex, Frame>;

// ============================================================
// UiState —— 一次绘屏所需的全部信息
// ============================================================

/// UI 渲染所需状态（一次绘屏的完整数据）
///
/// 来自两处：
/// - **payload**：当前手柄输入（`Frame.payload` + `Frame.seq`）
/// - **health**：连接/电量（从 `AtomicBool`/`AtomicU8` 读取快照）
///
/// 使用值语义 `Copy`，避免共享借用带来的复杂度。
#[derive(Debug, Clone, Copy)]
pub struct UiState {
  /// 当前展示的手柄帧
  pub frame: Frame,
  /// BLE 连接状态快照
  pub ble_connected: bool,
  /// ESP-NOW 就绪状态快照
  pub esp_now_ready: bool,
  /// Host 心跳活跃标志（H 选项新增）
  pub host_heartbeat_alive: bool,
  /// 电池电量 0..=100
  pub battery: u8,
  /// 电池分级快照（L 选项：用于渲染层决定是否画告警边框）
  pub battery_level: BatteryLevel,
  /// 当前活跃的 Toast（未过期时不为 None）
  pub toast: Option<Toast>,
}

impl UiState {
  /// 从最新 Frame + 共享状态 + 当前 Toast 构造一个快照
  pub fn snapshot(frame: Frame, toast: Option<Toast>) -> Self {
    Self {
      frame,
      ble_connected: BLE_CONNECTED.load(Ordering::Relaxed),
      esp_now_ready: ESP_NOW_READY.load(Ordering::Relaxed),
      host_heartbeat_alive: HOST_HEARTBEAT_ALIVE.load(Ordering::Relaxed),
      battery: BATTERY_LEVEL.load(Ordering::Relaxed),
      battery_level: battery_level_state(),
      toast,
    }
  }
}

// ============================================================
// UiTransport —— 让 UI 也走 Transport trait
// ============================================================

/// UI 传输适配器：把 `send(&frame)` 转成 Signal 写入
///
/// 与 [`BleHidTransport`](crate::transport::ble_hid::BleHidTransport) /
/// [`EspNowTransport`](crate::transport::esp_now::EspNowTransport) 保持一致的
/// "handle-only" 模式：本结构不持有硬件，OLED 由 [`oled_task`] 独占。
pub struct UiTransport {
  signal: &'static UiFrameSignal,
}

impl UiTransport {
  /// 构造 handle。真正的 OLED 刷屏由 [`oled_task`] 在后台运行。
  pub const fn new(signal: &'static UiFrameSignal) -> Self {
    Self { signal }
  }
}

impl Transport for UiTransport {
  type Error = Infallible;

  fn send(&mut self, frame: &Frame) -> Result<(), Self::Error> {
    // Frame 是 Copy，值语义写入 Signal，覆盖旧值
    self.signal.signal(*frame);
    Ok(())
  }
}

// ============================================================
// oled_task —— 后台刷屏任务
// ============================================================

use defmt::{info, warn};
use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Instant, Timer};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::DrawTarget;
use ssd1306::Ssd1306;
use ssd1306::mode::BufferedGraphicsMode;
use ssd1306::prelude::I2CInterface;
use ssd1306::size::DisplaySize128x64;

use crate::config::display::REFRESH_INTERVAL_MS;

/// OLED 显示驱动的具体类型别名
///
/// 泛型 `I2C`：实际是 esp-hal 的 `I2c<'static, Blocking>`，但为了模块解耦
/// 这里保持泛型；调用方传入什么类型都行，只要它实现 `embedded_hal::i2c::I2c`。
pub type OledDisplay<I2C> =
  Ssd1306<I2CInterface<I2C>, DisplaySize128x64, BufferedGraphicsMode<DisplaySize128x64>>;

/// OLED 刷屏后台任务
///
/// # 设计要点
/// - **限流**：`REFRESH_INTERVAL_MS`（默认 50 ms ≈ 20 Hz）—— OLED I²C 写全屏
///   约 6..8 ms，20 Hz 已经足够流畅且不会挤占 CPU
/// - **两种唤醒源**：
///   * Signal 收到新 Frame（<= 50 ms 就有一次）
///   * 强制超时（50 ms 到点也刷）—— 保证"即使发送端停止，屏幕也不会卡在旧帧"
/// - **失败即忽略**：I²C 抖动/断线只 warn 一次，下一帧照常尝试
///
/// # embassy_executor 任务约束
/// `I2C` 类型参数必须在使用处能推导；embassy `#[task]` 要求单态化，所以此
/// task 本身**不带泛型**——由调用方创建一个具体类型的 wrapper task。
///
/// 见 [`OledDisplay`] 的泛型定义。为了让 embassy `#[task]` 能生成 spawn token，
/// 调用方（main.rs）需要用具体的 `esp_hal::i2c::master::I2c` 类型实例化。
#[embassy_executor::task]
pub async fn oled_task(
  mut display: OledDisplay<esp_hal::i2c::master::I2c<'static, esp_hal::Blocking>>,
  signal: &'static UiFrameSignal,
) -> ! {
  use ssd1306::mode::DisplayConfig;

  info!("[UI] OLED task started");

  // 初始化 OLED：清屏 + 上电（若失败，回退到"永远显示空屏"）
  if display.init().is_err() {
    warn!("[UI] OLED init failed — task will keep retrying");
  }
  display.clear(BinaryColor::Off).ok();
  let _ = display.flush();

  // 最近一次收到的 Frame（未收到时用零帧兄底）
  let mut latest = Frame::new(0, Default::default());
  // 当前活跃的 Toast + 过期时间
  let mut active_toast: Option<Toast> = None;
  let mut toast_expire_at = Instant::from_millis(0);

  loop {
    // 三路唤醒：Signal 新帧 / Toast 信号 / 定时刷屏
    let refresh = Timer::after(Duration::from_millis(REFRESH_INTERVAL_MS));
    match select3(signal.wait(), TOAST_SIGNAL.wait(), refresh).await {
      Either3::First(new_frame) => {
        latest = new_frame;
      }
      Either3::Second(toast) => {
        active_toast = Some(toast);
        toast_expire_at = Instant::now() + Duration::from_millis(TOAST_DURATION_MS);
      }
      Either3::Third(()) => {
        // 定时到：用旧 latest 刷屏（避免屏幕卡死在很久以前）
      }
    }

    // Toast 过期则清除
    if active_toast.is_some() && Instant::now() >= toast_expire_at {
      active_toast = None;
    }

    // 心跳超时检查（依赖 20 Hz 刷屏频率即可及时发现）
    check_host_heartbeat_liveness();

    // 组装快照 + 渲染
    let state = UiState::snapshot(latest, active_toast);
    if layout::render(&mut display, &state).is_err() {
      warn!("[UI] render failed");
      continue;
    }
    if display.flush().is_err() {
      warn!("[UI] flush failed");
    }
  }
}
