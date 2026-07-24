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
use embassy_time::{Duration, Instant, Timer};
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
// Panic handler + defmt global logger（走 UART，espflash monitor 可解码）：
// - `esp_backtrace`：提供 `#[panic_handler]`，panic 时通过 defmt::error! 打印
// - `esp_println`：以 `#[defmt::global_logger]` 注册全局 defmt logger，输出到 UART
// 两个 crate 都只需 `as _` 触发链接期注册，无需显式调用 init。
use esp_backtrace as _;
use esp_println as _;
use ssd1306::size::DisplaySize128x64;
use ssd1306::{I2CDisplayInterface, Ssd1306};
use static_cell::StaticCell;

use controller::config::display::{I2C_FREQ_HZ, OLED_ROTATION};
use controller::config::tuning::{
  INPUT_SCAN_INTERVAL_MS, JOYSTICK_DEADZONE, PEER_STALE_TTL_MS, TRANSMIT_INTERVAL_MS,
};
use controller::hal::led_effects::led_effects_task;
use controller::hal::persist::{
  InMemoryStorage, NvsStorage, load_or_default, persist_worker_in_memory_task,
  persist_worker_nvs_task,
};
use controller::hal::{AnalogInput, Button, Joystick, Led};
use controller::input::{InputSampler, apply_sensitivity, update_button_led_state};
use controller::protocol::Frame;
use controller::transport::ble_hid::{
  BleHidTransport, EspBleController, FrameSignal, ble_gamepad_task,
};
use controller::transport::esp_now::{
  EspNowRecvLink, EspNowSendLink, EspNowTransport, esp_now_notifier_broadcast_task,
  esp_now_notifier_recv_task,
};
use controller::transport::{CompositeTransport, Transport};
use controller::ui::{
  OledDisplay, SelectorInput, UiFrameSignal, UiTransport, handle_selector_input, oled_task,
};

extern crate alloc;

// This creates a default app-descriptor required by the esp-idf bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
  clippy::large_stack_frames,
  reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
  // defmt global logger 由 `esp_println` 通过 `#[defmt::global_logger]` 在链接期
  // 静态注册，此处无需显式 init。（原 `rtt_target::rtt_init_defmt!()` 已移除。）

  let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
  let peripherals = esp_hal::init(config);

  // ==== 参考启动引脚（strapping pin）====
  // GPIO0 / GPIO2 / GPIO5 (LED_1) / GPIO12 (JOY_BTN) / GPIO15 (COLOR_LED)
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

  // 拆分出 esp_now（move），留下的 station/access_point 也必须保留。
  // station 保留字段用来读 MAC-48（喂给 comm::Notifier 双身份 handler_config）。
  let esp_radio::wifi::Interfaces {
    esp_now: esp_now_iface,
    station,
    access_point: _ap,
    ..
  } = interfaces;

  // 本机 MAC-48：两处用途 ——
  //   1. `comm::CommandHandlerConfig::my_mac`（AssignId 分派匹配；Notifier 通常收不到
  //      给自己的 AssignId，主要作 AnnounceReply 的 my_mac 来源）
  //   2. `EspNowRecvLink` 自帧回环过滤（丢弃 src == own_mac 的帧）
  let own_mac = station.mac_address();
  info!(
    "[ESP-NOW] station MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
    own_mac[0], own_mac[1], own_mac[2], own_mac[3], own_mac[4], own_mac[5]
  );
  let _station = station; // 保活：drop 会关闭 station 接口

  // Wi-Fi controller / 剩余接口都需要长命周期——泄漏为 'static
  static WIFI_CTRL: StaticCell<esp_radio::wifi::WifiController<'static>> = StaticCell::new();
  let _wifi_ctrl_static = WIFI_CTRL.init(wifi_controller);

  // 拆出 esp_now 的 manager / sender / receiver；分别包成 comm::CommLink 的两半
  let (esp_now_manager, esp_now_sender, esp_now_receiver) = esp_now_iface.split();
  // manager 泄漏为 'static：一是 drop 会关闭 ESP-NOW（保活），二是 send_link
  // 需要它做单播 peer 惰性 `add_peer`（AssignId 单播，Phase 1）。
  static ESP_NOW_MANAGER: StaticCell<esp_radio::esp_now::EspNowManager<'static>> =
    StaticCell::new();
  let esp_now_manager_static: &'static esp_radio::esp_now::EspNowManager<'static> =
    ESP_NOW_MANAGER.init(esp_now_manager);

  // 用 EspNowSendLink / EspNowRecvLink 把 esp-radio 的两半包成 comm::CommLink，
  // 交给 comm::Notifier 门面的两个 loop 驱动。Frame/Command/Response 三路 signal
  // 已在 esp_now/mod.rs 以 static 形式定义，无需 StaticCell。
  let send_link = EspNowSendLink::new(esp_now_sender, esp_now_manager_static);
  // own_mac 传入 recv link：过滤自帧回环（self-echo），避免双身份 Notifier 自发现
  let recv_link = EspNowRecvLink::new(esp_now_receiver, own_mac);

  // 组装双身份 Notifier 门面（link 无关；收拢 keyring / registry / replay / 三路
  // signal + command handler）。拿到 `&'static` 后既喂两条后台 loop，主循环也用它
  // `discover()`。
  let notifier = controller::transport::esp_now::init_notifier(own_mac);

  // Spawn Notifier broadcast loop（对应旧 esp_now_broadcast_task）
  spawner.spawn(
    esp_now_notifier_broadcast_task(notifier, send_link)
      .expect("Failed to build ESP-NOW notifier broadcast task token"),
  );

  // Spawn Notifier recv loop（对应旧 esp_now_receive_task；含 AnnounceReply
  // upsert / 自动 AssignId / Command 派发到 control::dispatch_command_from_esp_now）
  spawner.spawn(
    esp_now_notifier_recv_task(notifier, recv_link)
      .expect("Failed to build ESP-NOW notifier recv task token"),
  );

  // ============================================================
  // K3: 初始化 session nonce + spawn Nonce 广播任务
  //
  // 手柄每次上电生成一个新的 4 字节 session nonce，作为 HMAC 计算前缀混入。
  // 攻击者即使 dump 出 SHARED_SECRET，用旧 nonce 抓包再回放也无法通过
  // 校验 —— 因为下次重启时 nonce 已经变了。
  //
  // 能力已下沉到 `comm`：
  //   - `SimpleEntropy` 实现 `EntropySource`（硬件 RNG ⊕ 时钟抖动）
  //   - `comm::init_session` 一行完成 seed 采样 + 写入 SESSION_NONCE
  //   - `nonce_broadcast_task` 直接调门面的 `run_nonce_broadcast_loop`，无需手写
  // ============================================================
  {
    let mut entropy = controller::hal::rng::SimpleEntropy;
    let nonce = comm::init_session(&mut entropy);
    info!("[SEC] Session nonce initialized (hw RNG): 0x{:08x}", nonce);
  }
  spawner
    .spawn(nonce_broadcast_task(notifier).expect("Failed to build nonce_broadcast_task token"));

  // BLE 侧响应中继：向 dashboard 补发 NonceHello + 转发 REGISTRY 接收方目录
  spawner.spawn(ble_response_relay_task().expect("Failed to build ble_response_relay_task token"));

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
  // 电池监测：暂不启用
  // —— 本硬件电池经充放电模块 + AMS1117 LDO 稳压后给 ESP32 供电，没有任何一路
  //    电池原始电压引到 ADC 引脚，MCU 物理上无法测量电量。故不再 spawn 模拟任务
  //    （模拟递减只会给出假读数 + 假的低电量告警）；OLED 电量图标也已移除。
  //    若日后加一路“电池 → 分压 → GPIO34”的采样线，再启用真实测量任务即可。
  // ============================================================
  info!("[BAT] battery monitoring disabled (no ADC sense path on this hardware)");

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
  let output_default = OutputConfig::default();

  // 4 个通用按钮：按下拉低（active_low）
  let button_1 = Button::new(Input::new(peripherals.GPIO27, input_pullup), false);
  let button_2 = Button::new(Input::new(peripherals.GPIO13, input_pullup), false);
  let button_3 = Button::new(Input::new(peripherals.GPIO25, input_pullup), false);
  let button_4 = Button::new(Input::new(peripherals.GPIO23, input_pullup), false);

  // 摇杆按下键（IO12，标准 KY-023 / PS2 摇杆接法：一端 GND、按下拉低）
  //
  // 电气结构（见 hal/joystick.rs 文件头注释）：
  //   GND ── [按钮 SW] ── IO12
  // 需要 `Pull::Up`（保证未按下时读到 HIGH）+ `active_high = false`（按下 = LOW = 按下态）
  //
  // ⚠️ 历史配置曾用 `Pull::Down + active_high=true`（假设按钮接 3V3），
  // 但实机诊断 raw 电平不随按压变化，说明按钮的另一端不是 3V3。
  // 与项目里其它 4 个通用按钮统一采用"按下拉低"接法。
  let joystick_btn = Button::new(
    Input::new(peripherals.GPIO12, input_pullup),
    /* active_high = */ false,
  );

  // ADC1：摇杆 X/Y + 2 个旋钮
  let mut adc_config = AdcConfig::new();
  let joystick_x_pin = adc_config.enable_pin(peripherals.GPIO32, Attenuation::_11dB);
  let joystick_y_pin = adc_config.enable_pin(peripherals.GPIO33, Attenuation::_11dB);
  let knob_1_pin = adc_config.enable_pin(peripherals.GPIO36, Attenuation::_11dB);
  let knob_2_pin = adc_config.enable_pin(peripherals.GPIO39, Attenuation::_11dB);
  let mut adc = Adc::new(peripherals.ADC1, adc_config);

  let mut joystick_x = AnalogInput::new(joystick_x_pin, JOYSTICK_DEADZONE);
  let mut joystick_y = AnalogInput::new(joystick_y_pin, JOYSTICK_DEADZONE);
  let mut knob_1 = AnalogInput::new(knob_1_pin, /* deadzone = */ 0);
  let mut knob_2 = AnalogInput::new(knob_2_pin, 0);

  // ---- 硬件 POST：ADC 通道各采一次原始值并校验（非致命） ----
  let adc_ok = {
    let jx = joystick_x.read_raw(&mut adc);
    let jy = joystick_y.read_raw(&mut adc);
    let k1 = knob_1.read_raw(&mut adc);
    let k2 = knob_2.read_raw(&mut adc);
    controller::hal::post::check_adc(&[("JOY_X", jx), ("JOY_Y", jy), ("KNOB1", k1), ("KNOB2", k2)])
  };

  let mut joystick = Joystick::new(joystick_x, joystick_y, joystick_btn);

  // ---- 摇杆上电校准 ----
  //
  // 摇杆电位器的机械居中位置往往不严格对应电气 3V3/2，直接以 ADC_MID=2048
  // 作中值会导致静止时轴输出偏几个单位（例如 +0..+5 抖动）。此处采样均值
  // 作为 zero_offset，运行时按此扣减。
  //
  // 前置条件：上电时用户不能推着摇杆；若均值偏离 ADC_MID 超过阈值，
  // AnalogInput::calibrate 会拒绝并回退到 ADC_MID，同时打 warn 日志。
  let (joy_x_zero, joy_y_zero) = joystick.calibrate(&mut adc);
  info!("[JOY] calibrated zero: x={} y={}", joy_x_zero, joy_y_zero);

  // 2 个 LED（active_high 点亮）——所有权直接交给 led_effects_task
  let led_1: Led<'static> = Led::new(
    Output::new(peripherals.GPIO5, Level::Low, output_default),
    true,
  );
  let led_2: Led<'static> = Led::new(
    Output::new(peripherals.GPIO18, Level::Low, output_default),
    true,
  );

  // 彩灯（IO15，4 颗并联 LED，阳极接 IO15、阴极接 GND，每路带限流电阻）
  //
  // ⚠️ IO15 是 strapping pin：复位阶段需为高才正常启动。此处在 app 启动后
  // （复位采样早已完成）才配成推挽输出，故不影响启动；以 Level::Low 初始化即可。
  // active_high = true：置高 = 灌电流点亮。所有权交给 led_effects_task 持续闪烁。
  let color_led: Led<'static> = Led::new(
    Output::new(peripherals.GPIO15, Level::Low, output_default),
    true,
  );

  // Spawn LED 特效任务（LED1/LED2 + 彩灯 硬件从主循环转移到此任务）
  spawner.spawn(
    led_effects_task(led_1, led_2, color_led).expect("Failed to build led_effects_task token"),
  );

  // ============================================================
  // OLED（SSD1306 128x64 via I²C，IO21=SDA / IO22=SCL）
  //   构造 I²C → 包装成 Ssd1306 → StaticCell 泄漏为 'static → spawn 后台任务
  // ============================================================
  let i2c_config = I2cConfig::default().with_frequency(Rate::from_hz(I2C_FREQ_HZ));
  let mut i2c = I2c::new(peripherals.I2C0, i2c_config)
    .expect("Failed to init I2C for OLED")
    .with_sda(peripherals.GPIO21)
    .with_scl(peripherals.GPIO22);

  // ---- 硬件 POST：探测 OLED 是否在总线上应答（非致命，无屏也继续启动） ----
  let oled_present =
    controller::hal::post::probe_oled(&mut i2c, controller::hal::post::OLED_I2C_ADDR);

  // 让 I2c 拿到 'static 生命周期（OLED 任务永不返回）
  static I2C_BUS: StaticCell<I2c<'static, Blocking>> = StaticCell::new();
  let i2c_static: &'static mut I2c<'static, Blocking> = I2C_BUS.init(i2c);
  // 移出 owned 值给 SSD1306（与 BLE controller 同样的"init once, move out"惯用法）
  let i2c_owned: I2c<'static, Blocking> = unsafe {
    // SAFETY: I2C_BUS 只被 init 一次并读取一次，后续不会再访问 i2c_static 位置
    core::ptr::read(i2c_static as *const _)
  };

  let interface = I2CDisplayInterface::new(i2c_owned);
  // 屏幕物理安装方向由 `config::display::OLED_ROTATION` 集中控制
  // （手柄外壳把屏幕装反了 180°，此处按需补偿；调向只改配置常量即可）
  let display: OledDisplay<I2c<'static, Blocking>> =
    Ssd1306::new(interface, DisplaySize128x64, OLED_ROTATION).into_buffered_graphics_mode();

  // UI 帧通道：主循环 → oled_task
  static UI_SIGNAL: StaticCell<UiFrameSignal> = StaticCell::new();
  let ui_signal: &'static UiFrameSignal = UI_SIGNAL.init(UiFrameSignal::new());

  // 汇总开机自检结果 → oled_task 在进入正常刷新前先展示一屏 POST 摘要。
  // protocol/radio 能走到这里即代表已越过各自初始化（失败会提前 panic）。
  let post_report = controller::hal::post::PostReport {
    protocol_ok: true,
    radio_ok: true,
    oled_present,
    adc_ok,
  };

  spawner
    .spawn(oled_task(display, ui_signal, post_report).expect("Failed to build oled_task token"));

  // ============================================================
  // 聚合成 InputSampler + 挂上复合 Transport（BLE HID + ESP-NOW）
  // ============================================================
  let mut sampler = InputSampler::new(
    button_1, button_2, button_3, button_4, joystick, knob_1, knob_2,
  );

  // 一次 send()，同时送达 BLE + ESP-NOW + OLED；任一失败不影响其它
  // 嵌套 CompositeTransport：三路组合
  let mut transport = CompositeTransport::new(
    BleHidTransport::new(signal),
    CompositeTransport::new(EspNowTransport::new(notifier), UiTransport::new(ui_signal)),
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
  // Announce 命令的 seq 直接由 SESSION_KEYRING 分配，与其它 Command 共享同一个
  // 递增计数器（反重放窗口按 key_id 内部自己推进，seq 空间无需隔离）。
  loop {
    // ---- 采样 + 本地反馈（LED 位图写入 AtomicU8，effect task 应用到硬件）----
    let mut sample = sampler.poll(&mut adc);
    update_button_led_state(&sample);

    // ---- 应用灵敏度缩放（可能被 Host 命令动态修改）----
    apply_sensitivity(&mut sample.state);

    // ---- 驱动接收方选择器：长按切换模式、摇杆 Y 移光标、Btn1/Btn2 加减目标 ----
    //
    // Selecting 模式下选择器会把 `suppress_frame_send` 置 true，此时下面的
    // `transport.send(&frame)` 会被跳过，避免摇杆游走干扰接收端。
    //
    // 长按触发源 = IO12 摇杆按钮：IO15 原为拨动开关输入，但实际是 4 颗彩灯的
    // 驱动节点（见 config::pins::COLOR_LED），已改为输出驱动彩灯、不再作输入，
    // 故长按进入 Selecting 固定用 IO12（`SelectorInput.switch_on` 只是选择器内部
    // 的"长按触发"抽象字段名，与物理拨动开关无关）。
    //
    // 副作用提示：
    // - 长按 IO12 摇杆按钮 ≥800ms 会进入 Selecting，此期间 Frame 发送被抑制
    // - IO12 短按（<800ms）不影响，仍会作为 `ButtonBits::JoyBtn` 位正常上报
    let selector_outcome = handle_selector_input(SelectorInput {
      switch_on: sample.joy_button.is_down(),
      btn1_just_pressed: sample.buttons[0] == controller::hal::button::ButtonState::JustPressed,
      btn2_just_pressed: sample.buttons[1] == controller::hal::button::ButtonState::JustPressed,
      joy_y: sample.joystick.y,
      now: Instant::now(),
    });

    // ---- 首次进入 Selecting：广播 Announce，让所有 receiver 上报 AnnounceReply ----
    //
    // 门面一行搞定：分配 seq + encode + 入队 CMD_OUT_SIG（满时丢弃并计 metrics）。
    if selector_outcome.just_entered {
      // 发现前先淘汰长时间未再上报的接收方，让候选列表反映当前在线情况；仍在线者
      // 会在本轮 AnnounceReply 中立即重新入库（见 `PEER_STALE_TTL_MS` 文档）。
      controller::REGISTRY.prune(Instant::now(), Duration::from_millis(PEER_STALE_TTL_MS));
      notifier.discover();
    }

    // ---- 退出 Selecting：目标位图已在选择器内提交，恢复正常发帧 ----
    if selector_outcome.just_exited {
      info!(
        "[UI] exited target selector, dest_mask=0x{:08x}",
        controller::ui::active_dest_mask()
      );
    }

    // ---- 每 TRANSMIT_EVERY_N 次采样发一帧，Selecting 时静默不发 ----
    if tick.is_multiple_of(TRANSMIT_EVERY_N) && !selector_outcome.suppress_frame_send {
      // dest_mask 从选择器的已生效目标位图取——Normal 模式下默认为广播
      // (BROADCAST_DEST_MASK)，Selecting 退出后会替换为用户选中的候选 peer 位图。
      let frame = Frame::with_dest(seq, sample.state, controller::ui::active_dest_mask());
      // 类型级 Infallible 断言：当前四条 transport（BLE/ESP-NOW/UI/defmt）的
      // `Error` 都是 [`core::convert::Infallible`]，`CompositeTransport` 组合
      // 后 `Error = CompositeError<Infallible, CompositeError<Infallible, Infallible>>`
      // 是"不可构造"的类型—— 用 `match e {}` 做空模式穷举比 `.unwrap()` 更严谨：
      //
      // - `.unwrap()`：语义暗示"可能 panic"，且未来任一 transport 变成可失败时会
      //   静默把编译期不变量降级为运行时 panic
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

// ============================================================
// K3: Nonce 广播 task 本地包装
// ============================================================
//
// `Notifier::run_nonce_broadcast_loop` 是 async 方法（`#[embassy_executor::task]`
// 需要具体的 async fn 签名，不能直接标注方法），因此在此用 task 包一层。
// 门面自己持有 `RESP_SIG`，无需再手抄那份 `&'static` 引用。
#[embassy_executor::task]
async fn nonce_broadcast_task(notifier: &'static comm::Notifier) -> ! {
  notifier
    .run_nonce_broadcast_loop(comm::DEFAULT_NONCE_BROADCAST_INTERVAL)
    .await
}

// ============================================================
// BLE 响应中继：向 dashboard 周期推送 NonceHello + 接收方目录
// ============================================================
//
// dashboard 经 BLE 连接，但此前"看不到"接收方列表，根因有二：
//   1. NonceHello 仅经 ESP-NOW 广播（nonce_broadcast_task 用 esp_now::RESP_SIG），
//      BLE 侧从不发送 —— dashboard 拿不到 session nonce，AUTH_ENABLED=true 下
//      **任何**带 HMAC 的响应（Ack / AnnounceReply）验签都失败被丢弃。
//   2. AnnounceReply 是 comm 内部发现机制，upsert 进 REGISTRY 后不转发到 BLE。
//
// 本任务在 BLE 已连接时周期性：先补一条 NonceHello（让 dashboard 免鉴权
// bootstrap nonce），再把 REGISTRY 快照逐条以 AnnounceReply 推给 dashboard。
//
// # 背压式入队（替代旧的"固定 gap + 撞运气 try_send"）
// RESPONSE_SIGNAL 是深度 [`OUTBOUND_QUEUE_DEPTH`](comm::notifier::signals::OUTBOUND_QUEUE_DEPTH)=4
// 的有界队列，且**与命令 Ack 共用**（`broadcast_response` 也写它）。旧实现每条之间
// 固定 sleep 80ms 后 `try_send`，有两个隐患：
//   1. peer 数多 / BLE 卡顿时靠后的 AnnounceReply 会被静默丢弃；
//   2. `(1+N)*80ms` 在 N≥24 时超过周期，导致 relay 周期重叠、负载叠加。
// 现改为 [`relay_send`] 的**背压式入队**：只在队列留有富余空位（保留 ≥1 格给延迟
// 敏感的命令 Ack）时才入队，否则轮询等 BLE tx 任务先取走。永不溢出、不挤丢 Ack、
// peer 数多只是本轮耗时更长（单任务顺序执行，周期 Timer 在循环之后，天然无重叠）。
const BLE_RELAY_PERIOD_MS: u64 = 2000;
/// relay 入队前要求的最小空闲槽位：给命令 Ack（`broadcast_response`）至少留 1 格，
/// 顺带兜住"检查空位与 try_send 之间被并发 Ack 抢走 1 格"的竞态（留 2 即使被抢 1 仍成）。
const BLE_RELAY_MIN_FREE_SLOTS: usize = 2;
/// 队列暂无富余空位时的轮询间隔
const BLE_RELAY_POLL_MS: u64 = 20;
/// 单条 relay 消息等待队列空位的上限：正常连接时消费者毫秒级取走；超时说明 BLE
/// 卡死/断开，放弃本轮剩余、等下个周期（届时重新快照 REGISTRY）。
const BLE_RELAY_SEND_TIMEOUT_MS: u64 = 500;

#[embassy_executor::task]
async fn ble_response_relay_task() -> ! {
  use comm::{CommandResponse, KeyId, RSSI_UNKNOWN, session_nonce};
  use controller::transport::ble_hid::{RESPONSE_SIGNAL, signal_response};
  use controller::ui::BLE_CONNECTED;

  // 背压式入队：见任务上方文档。返回 false = 超时（BLE 卡死/断开），调用方应放弃本轮。
  async fn relay_send(resp: CommandResponse) -> bool {
    let mut waited_ms = 0_u64;
    loop {
      if RESPONSE_SIGNAL.free_capacity() >= BLE_RELAY_MIN_FREE_SLOTS {
        // 有富余空位：signal_response 内部 try_send，此处几乎必成（保留余量兜住竞态）
        signal_response(resp);
        return true;
      }
      if waited_ms >= BLE_RELAY_SEND_TIMEOUT_MS {
        return false;
      }
      Timer::after(Duration::from_millis(BLE_RELAY_POLL_MS)).await;
      waited_ms += BLE_RELAY_POLL_MS;
    }
  }

  loop {
    if BLE_CONNECTED.load(Ordering::Relaxed) {
      // 1) NonceHello —— dashboard 借此免鉴权 bootstrap session nonce
      if relay_send(CommandResponse::nonce_hello(session_nonce())).await {
        // 2) 逐 receiver_id 遍历 REGISTRY → 每台一条 AnnounceReply。
        //    刻意用 `peer_by_id` 单点取而非 `snapshot()`：后者会把最多 MAX_PEERS 个
        //    PeerInfo 的 Vec 搬上栈并跨 await 常驻，触发 large_stack_frames；单点版
        //    每次只持有一个小 PeerInfo。
        for id in 0..(comm::MAX_PEERS as u8) {
          let Some(peer) = controller::REGISTRY.peer_by_id(id) else {
            continue;
          };
          // comm 的未知 RSSI 哨兵是 i8::MIN(-128)，协议约定的未知值是 -127；
          // 映射后 dashboard 才会显示 "--" 而非 -128dBm
          let rssi = if peer.rssi_dbm == RSSI_UNKNOWN {
            -127
          } else {
            peer.rssi_dbm
          };
          let sent = relay_send(CommandResponse::announce_reply(
            0,
            KeyId::DEFAULT,
            peer.mac,
            rssi,
            peer.role,
          ))
          .await;
          if !sent {
            // 超时：BLE 卡死/断开，放弃本轮剩余 peer，等下个周期重试
            break;
          }
        }
      }
    }
    Timer::after(Duration::from_millis(BLE_RELAY_PERIOD_MS)).await;
  }
}
