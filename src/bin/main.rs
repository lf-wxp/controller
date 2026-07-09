#![no_std]
#![no_main]
#![deny(
  clippy::mem_forget,
  reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use core::sync::atomic::{AtomicBool, Ordering};

use bt_hci::controller::ExternalController;
use defmt::info;
use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use panic_rtt_target as _;
use ssd1306::rotation::DisplayRotation;
use ssd1306::size::DisplaySize128x64;
use ssd1306::{I2CDisplayInterface, Ssd1306};
use static_cell::StaticCell;

use controller::config::display::I2C_FREQ_HZ;
use controller::config::tuning::{INPUT_SCAN_INTERVAL_MS, JOYSTICK_DEADZONE, TRANSMIT_INTERVAL_MS};
use controller::hal::battery::{IS_SIMULATED, battery_monitor_simulated_task};
use controller::hal::led_effects::led_effects_task;
use controller::hal::persist::{
  InMemoryStorage, NvsStorage, load_or_default, persist_worker_in_memory_task,
  persist_worker_nvs_task,
};
use controller::hal::{AnalogInput, Button, Joystick, Led, Switch};
use controller::input::{InputSampler, apply_sensitivity, update_button_led_state};
use controller::protocol::Frame;
use controller::transport::ble_hid::{
  BleHidTransport, EspBleController, FrameSignal, ble_gamepad_task,
};
use controller::transport::esp_now::{
  EspNowFrameSignal, EspNowTransport, esp_now_broadcast_task, esp_now_receive_task,
  nonce_broadcast_task,
};
use controller::transport::{CompositeTransport, Transport};
use controller::ui::{OledDisplay, UiFrameSignal, UiTransport, oled_task};

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
  clippy::large_stack_frames,
  reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
  rtt_target::rtt_init_defmt!();

  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  // ==== 参考启动引脚（strapping pin）====
  // GPIO0 / GPIO2 / GPIO5 (LED_1) / GPIO12 (JOY_BTN) / GPIO15 (SWITCH_1)
  // 项目未使用、被模块本身占用的引脚：
  let _ = peripherals.GPIO6;
  let _ = peripherals.GPIO7;
  let _ = peripherals.GPIO8;
  let _ = peripherals.GPIO9;
  let _ = peripherals.GPIO10;
  let _ = peripherals.GPIO11;
  let _ = peripherals.GPIO16;
  let _ = peripherals.GPIO20;

  esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 98768);
  // COEX needs more RAM - so we've added some more
  esp_alloc::heap_allocator!(size: 64 * 1024);

  let timg0 = TimerGroup::new(peripherals.TIMG0);
  let sw_interrupt =
    esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
  esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

  info!("Embassy initialized!");

  // ============================================================
  // S1: 启动自检 —— 在 spawn 任何 task / 初始化 session nonce 之前执行
  //
  // 校验协议核心不变式（密钥环长度、HMAC 环回、Frame 编解码 21 字节），
  // 一旦 build 配置 / feature flag / 依赖版本引入自洽性破坏，
  // 立即 panic → RTT 打印 backtrace → ESP32 复位。
  //
  // 注意：self_test::run 内部会临时把 SESSION_NONCE 设为固定测试值，
  // 因此必须在下方 `init_session_nonce(seed)` 之前调用；真实 nonce
  // 稍后由硬件 RNG 覆盖，测试值不会污染运行时。
  // ============================================================
  controller::self_test::run();

  // ============================================================
  // Wi-Fi / BLE / ESP-NOW 初始化
  //   - Wi-Fi 控制器构造后必须长期保留（driver 生命周期与其绑定）
  //   - Interfaces 里的 `esp_now` 字段拆出来交给广播任务；
  //     剩下的 station/access_point 同样泄漏为 'static 保活
  //   - BLE controller 通过 StaticCell 延长到 'static，交给后台任务
  // ============================================================
  let (wifi_controller, interfaces) = esp_radio::wifi::new(peripherals.WIFI, Default::default())
    .expect("Failed to initialize Wi-Fi controller");

  // 拆分出 esp_now（move），留下的 station/access_point 也必须保留
  let esp_radio::wifi::Interfaces {
    esp_now: esp_now_iface,
    station: _station,
    access_point: _ap,
    ..
  } = interfaces;

  // Wi-Fi controller / 剩余接口都需要长命周期——泄漏为 'static
  static WIFI_CTRL: StaticCell<esp_radio::wifi::WifiController<'static>> = StaticCell::new();
  let _wifi_ctrl_static = WIFI_CTRL.init(wifi_controller);

  // 拆出 esp_now 的 sender 单向、独占；——发送任务只需要 sender
  let (esp_now_manager, esp_now_sender, esp_now_receiver) = esp_now_iface.split();
  // manager 本项目不用，但 drop 会关闭 ESP-NOW，同样泄漏为 'static
  static ESP_NOW_MANAGER: StaticCell<esp_radio::esp_now::EspNowManager<'static>> =
    StaticCell::new();
  let _esp_now_manager_static = ESP_NOW_MANAGER.init(esp_now_manager);

  // ESP-NOW 帧通道：主循环 → 广播任务
  static ESP_NOW_SIGNAL: StaticCell<EspNowFrameSignal> = StaticCell::new();
  let esp_now_signal: &'static EspNowFrameSignal = ESP_NOW_SIGNAL.init(EspNowFrameSignal::new());

  // Spawn ESP-NOW 广播任务（sender 值传递）
  spawner.spawn(
    esp_now_broadcast_task(esp_now_sender, esp_now_signal)
      .expect("Failed to build ESP-NOW broadcast task token"),
  );

  // Spawn ESP-NOW 接收任务（receiver 值传递，用于接收 Command 帧）
  spawner.spawn(
    esp_now_receive_task(esp_now_receiver).expect("Failed to build ESP-NOW receive task token"),
  );

  // ============================================================
  // K3: 初始化 session nonce + spawn Nonce 广播任务
  //
  // 手柄每次上电生成一个新的 4 字节 session nonce，作为 HMAC 计算前缀混入。
  // 攻击者即使 dump 出 SHARED_SECRET，用旧 nonce 抓包再回放也无法通过
  // 校验 —— 因为下次重启时 nonce 已经变了。
  //
  // Q 选项：使用 esp-hal 硬件 RNG 作为 seed 源。上面已经完成 Wi-Fi/BLE
  // controller 初始化，RF phy 会向 RNG 硬件寄存器持续注入真随机熵源，
  // `Rng::random()` 此时输出的是真随机数（而非启动时的 PRNG 弱熵）。
  // ============================================================
  {
    let seed = controller::hal::rng::init_seed();
    controller::protocol::init_session_nonce(seed);
    info!(
      "[SEC] Session nonce initialized (hw RNG): 0x{:08x}",
      controller::protocol::session_nonce()
    );
  }
  spawner.spawn(nonce_broadcast_task().expect("Failed to build nonce_broadcast_task token"));

  // ============================================================
  // K4 + P: 持久化配置——启动时载入、后台异步落盘
  //
  // 存储后端由 [`config::persist::USE_NVS_STORAGE`] 选择：
  // - `false`（默认）：[`InMemoryStorage`]，重启丢失 —— 无真机调试使用
  // - `true`：[`NvsStorage`]，写入 flash NVS 分区（双缓冲防断电损坏）
  //
  // 两条 spawn 分支都会编译（语义完整），但编译器会把 `USE_NVS_STORAGE` 为
  // 常量的分支做 dead-code 消除 ——未选中的那一支不会产生任何指令。
  // ============================================================
  if controller::config::persist::USE_NVS_STORAGE {
    static NVS_STORAGE: StaticCell<NvsStorage> = StaticCell::new();
    let persist_storage: &'static mut NvsStorage =
      NVS_STORAGE.init(NvsStorage::new(peripherals.FLASH));
    // 载入并回填全局运行时状态（灵敏度 / 电池模式 + U 选项 replay_windows）
    let loaded_cfg = load_or_default(persist_storage);
    loaded_cfg.apply_to_runtime();
    loaded_cfg.apply_replay_windows_to_runtime();
    info!(
      "[K4+P+U] Applied persisted config (NVS): joy={} knob={} bat_sim={} last_seq={}",
      loaded_cfg.joy_sensitivity,
      loaded_cfg.knob_sensitivity,
      loaded_cfg.battery_simulated,
      loaded_cfg.last_seq
    );
    spawner.spawn(
      persist_worker_nvs_task(persist_storage)
        .expect("Failed to build persist_worker_nvs_task token"),
    );
  } else {
    static PERSIST_STORAGE: StaticCell<InMemoryStorage> = StaticCell::new();
    let persist_storage: &'static mut InMemoryStorage =
      PERSIST_STORAGE.init(InMemoryStorage::new());
    let loaded_cfg = load_or_default(persist_storage);
    loaded_cfg.apply_to_runtime();
    loaded_cfg.apply_replay_windows_to_runtime();
    info!(
      "[K4+U] Applied persisted config (in-memory mock): joy={} knob={} bat_sim={} last_seq={}",
      loaded_cfg.joy_sensitivity,
      loaded_cfg.knob_sensitivity,
      loaded_cfg.battery_simulated,
      loaded_cfg.last_seq
    );
    spawner.spawn(
      persist_worker_in_memory_task(persist_storage)
        .expect("Failed to build persist_worker task token"),
    );
  }
  // ============================================================
  // 电池监测（当前走模拟递减；接入真实分压硬件后改为 battery_monitor_real_task）
  // —— 任务会每 5 秒写一次 crate::ui::BATTERY_LEVEL，UI 与 BLE 同步时看到
  // ============================================================
  if IS_SIMULATED {
    spawner
      .spawn(battery_monitor_simulated_task().expect("Failed to build battery monitor task token"));
  } else {
    // TODO(hardware): 接入 GPIO34 分压硬件后，在这里 spawn battery_monitor_real_task
    // 同时把 config::battery::SIMULATE 改为 false
    info!("[BAT] Real battery monitor path not wired up yet");
  }

  let ble_transport = BleConnector::new(peripherals.BT, Default::default()).unwrap();
  let ble_controller = ExternalController::<_, 1>::new(ble_transport);

  // 把 BLE controller 延长到 'static 才能交给 embassy task
  static BLE_CONTROLLER: StaticCell<EspBleController<'static>> = StaticCell::new();
  let ble_controller: &'static mut EspBleController<'static> = BLE_CONTROLLER.init(ble_controller);

  // Report signal —— 主循环 → BLE 任务的单向通道
  static REPORT_SIGNAL: StaticCell<FrameSignal> = StaticCell::new();
  let signal: &'static FrameSignal = REPORT_SIGNAL.init(FrameSignal::new());

  // Spawn BLE 后台任务
  //
  // 注意：`ble_gamepad_task` 要求 controller 是**值传递**且 `'static`；
  // 但 `StaticCell::init` 返回 `&'static mut T`。这里用 `core::mem::replace`
  // 把值搬出来（原位置留一个占位，永远不会再被使用）。
  //
  // 更简单的做法：`ble_controller` 用 `let ble_controller = BLE_CONTROLLER.init(...)`
  // 得到 `&'static mut`，然后 `unsafe { core::ptr::read(ble_controller) }` 也行，
  // 但 `mem::replace` 更安全（虽然浪费一次拷贝，一次性开销可忽略）。
  //
  // 由于 EspBleController 不是 Copy，且我们绝不会再从 `&'static mut` 位置访问它，
  // 这里 mem::replace 一个 fresh 空 controller 会有类型问题。
  // 因此改用**直接 move**：`StaticCell::init_with(|| controller)` 可以做到，但 API
  // 是 `init(v)` 返回 mut ref。所以我们做一个稍微不同的模式：
  //   1) 声明 `static ONCE: StaticCell = ...`
  //   2) 用 `ONCE.init(controller)` 返回 `&'static mut` 引用
  //   3) 立即 spawn 时把 `&'static mut` deref 成 owned（**只能用一次**）
  //
  // 但 `EspBleController<'static>` 不能通过 &mut 转成 owned。
  // 所以真正可行的方案：**让 task 接受 &'static mut** 而不是 owned。
  // 这需要修改 task 签名。为避免此复杂性，改用如下手段：
  //
  //   let ctrl: EspBleController<'static> = unsafe {
  //     core::ptr::read(BLE_CONTROLLER.init(ble_controller))
  //   };
  //
  // 这是 embassy 生态里常见的"init once, move out"惯用法，且我们绝不会二次读。
  //
  // # 运行时防护（Q1 加固）
  // 加一个 `AtomicBool` guard —— 若未来有人**误复制这段 unsafe 代码**导致二次
  // `ptr::read`，会立刻 panic 而非产生双重释放/双重所有权 UB。开销：一次 CAS。
  static BLE_CONTROLLER_TAKEN: AtomicBool = AtomicBool::new(false);
  assert!(
    !BLE_CONTROLLER_TAKEN.swap(true, Ordering::AcqRel),
    "BLE controller must be moved out exactly once (double `ptr::read` = UB)",
  );
  let ble_controller_owned: EspBleController<'static> = unsafe {
    // SAFETY:
    // 1. `BLE_CONTROLLER` 由 `StaticCell::init` 初始化过一次（若二次 init 会 panic）。
    // 2. 上面的 `BLE_CONTROLLER_TAKEN` guard 保证这段代码**运行时**只执行一次；
    //    即便未来有人误复制此 unsafe 块，二次进入会立即 panic。
    // 3. `ble_controller` 变量在 read 之后不会再被访问，也不会重新赋值。
    core::ptr::read(ble_controller as *const _)
  };
  spawner.spawn(
    ble_gamepad_task(ble_controller_owned, signal).expect("Failed to build BLE gamepad task token"),
  );

  // ============================================================
  // 硬件初始化：按钮 / 开关 / 摇杆 / 旋钮 / LED
  // ============================================================
  let input_pullup = InputConfig::default().with_pull(Pull::Up);
  let input_pulldown = InputConfig::default().with_pull(Pull::Down);
  let output_default = OutputConfig::default();

  // 4 个通用按钮：按下拉低（active_low）
  let button_1 = Button::new(Input::new(peripherals.GPIO27, input_pullup), false);
  let button_2 = Button::new(Input::new(peripherals.GPIO13, input_pullup), false);
  let button_3 = Button::new(Input::new(peripherals.GPIO25, input_pullup), false);
  let button_4 = Button::new(Input::new(peripherals.GPIO23, input_pullup), false);

  // 拨动开关（IO15 strapping，Pull::Up）
  let switch_1 = Switch::new(
    Input::new(peripherals.GPIO15, input_pullup),
    /* active_high = */ false,
    /* initial_on  = */ false,
  );

  // 摇杆按下键（IO12 strapping，Pull::Down；⚠️ 电气结构见 hal/joystick.rs 说明）
  let joystick_btn = Button::new(
    Input::new(peripherals.GPIO12, input_pulldown),
    /* active_high = */ true,
  );

  // ADC1：摇杆 X/Y + 2 个旋钮
  let mut adc_config = AdcConfig::new();
  let joystick_x_pin = adc_config.enable_pin(peripherals.GPIO32, Attenuation::_11dB);
  let joystick_y_pin = adc_config.enable_pin(peripherals.GPIO33, Attenuation::_11dB);
  let knob_1_pin = adc_config.enable_pin(peripherals.GPIO36, Attenuation::_11dB);
  let knob_2_pin = adc_config.enable_pin(peripherals.GPIO39, Attenuation::_11dB);
  let mut adc = Adc::new(peripherals.ADC1, adc_config);

  let joystick_x = AnalogInput::new(joystick_x_pin, JOYSTICK_DEADZONE);
  let joystick_y = AnalogInput::new(joystick_y_pin, JOYSTICK_DEADZONE);
  let joystick = Joystick::new(joystick_x, joystick_y, joystick_btn);

  let knob_1 = AnalogInput::new(knob_1_pin, /* deadzone = */ 0);
  let knob_2 = AnalogInput::new(knob_2_pin, 0);

  // 2 个 LED（active_high 点亮）——所有权直接交给 led_effects_task
  let led_1: Led<'static> = Led::new(
    Output::new(peripherals.GPIO5, Level::Low, output_default),
    true,
  );
  let led_2: Led<'static> = Led::new(
    Output::new(peripherals.GPIO18, Level::Low, output_default),
    true,
  );

  // Spawn LED 特效任务（LED 硬件从主循环转移到此任务）
  spawner.spawn(led_effects_task(led_1, led_2).expect("Failed to build led_effects_task token"));

  // ============================================================
  // OLED（SSD1306 128x64 via I²C，IO21=SDA / IO22=SCL）
  //   构造 I²C → 包装成 Ssd1306 → StaticCell 泄漏为 'static → spawn 后台任务
  // ============================================================
  let i2c_config = I2cConfig::default().with_frequency(Rate::from_hz(I2C_FREQ_HZ));
  let i2c = I2c::new(peripherals.I2C0, i2c_config)
    .expect("Failed to init I2C for OLED")
    .with_sda(peripherals.GPIO21)
    .with_scl(peripherals.GPIO22);

  // 让 I2c 拿到 'static 生命周期（OLED 任务永不返回）
  static I2C_BUS: StaticCell<I2c<'static, Blocking>> = StaticCell::new();
  let i2c_static: &'static mut I2c<'static, Blocking> = I2C_BUS.init(i2c);
  // 移出 owned 值给 SSD1306（与 BLE controller 同样的"init once, move out"惯用法）
  let i2c_owned: I2c<'static, Blocking> = unsafe {
    // SAFETY: I2C_BUS 只被 init 一次并读取一次，后续不会再访问 i2c_static 位置
    core::ptr::read(i2c_static as *const _)
  };

  let interface = I2CDisplayInterface::new(i2c_owned);
  let display: OledDisplay<I2c<'static, Blocking>> =
    Ssd1306::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
      .into_buffered_graphics_mode();

  // UI 帧通道：主循环 → oled_task
  static UI_SIGNAL: StaticCell<UiFrameSignal> = StaticCell::new();
  let ui_signal: &'static UiFrameSignal = UI_SIGNAL.init(UiFrameSignal::new());

  spawner.spawn(oled_task(display, ui_signal).expect("Failed to build oled_task token"));

  // ============================================================
  // 聚合成 InputSampler + 挂上复合 Transport（BLE HID + ESP-NOW）
  // ============================================================
  let mut sampler = InputSampler::new(
    button_1, button_2, button_3, button_4, switch_1, joystick, knob_1, knob_2,
  );

  // 一次 send()，同时送达 BLE + ESP-NOW + OLED；任一失败不影响其它
  // 嵌套 CompositeTransport：三路组合
  let mut transport = CompositeTransport::new(
    BleHidTransport::new(signal),
    CompositeTransport::new(
      EspNowTransport::new(esp_now_signal),
      UiTransport::new(ui_signal),
    ),
  );

  info!("Hardware ready. Entering main loop.");

  // ============================================================
  // 主循环：
  //   1) 高频（100Hz）扫描输入 → 驱动 LED 本地反馈
  //   2) 低频（≈30Hz） 送出协议帧到 BLE Transport
  // 单一 loop 内用计数器分频，避免多任务同步复杂度。
  // ============================================================
  const TRANSMIT_EVERY_N: u32 = (TRANSMIT_INTERVAL_MS / INPUT_SCAN_INTERVAL_MS) as u32;

  let mut seq: u32 = 0;
  let mut tick: u32 = 0;
  loop {
    // ---- 采样 + 本地反馈（LED 位图写入 AtomicU8，effect task 应用到硬件）----
    let mut sample = sampler.poll(&mut adc);
    update_button_led_state(&sample);

    // ---- 应用灵敏度缩放（可能被 Host 命令动态修改）----
    apply_sensitivity(&mut sample.state);

    // ---- 每 TRANSMIT_EVERY_N 次采样发一帧 ----
    if tick.is_multiple_of(TRANSMIT_EVERY_N) {
      let frame = Frame::new(seq, sample.state);
      // 类型级 Infallible 断言：当前四条 transport（BLE/ESP-NOW/UI/defmt）的
      // `Error` 都是 [`core::convert::Infallible`]，`CompositeTransport` 组合
      // 后 `Error = CompositeError<Infallible, CompositeError<Infallible, Infallible>>`
      // 是"不可构造"的类型 —— 用 `match e {}` 做空模式穷举比 `.unwrap()` 更严谨：
      //
      // - `.unwrap()`：语义暗示"可能 panic"，且未来任一 transport 变成可失败时会
      //   静默把编译期不变量降级成运行时 panic
      // - `match e {}`：编译器要求穷举所有变体；一旦 transport 引入可失败 `Error`
      //   变体，此处立刻编译失败，强迫显式处理
      if let Err(e) = transport.send(&frame) {
        match e {}
      }
      seq = seq.wrapping_add(1);
    }

    tick = tick.wrapping_add(1);
    Timer::after(Duration::from_millis(INPUT_SCAN_INTERVAL_MS)).await;
  }
}
