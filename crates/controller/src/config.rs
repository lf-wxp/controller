//! 硬件配置常量集中区
//!
//! 更换硬件/更换引脚时只改此文件，其他代码不应硬编码任何 GPIO 编号。
//!
//! # 硬件清单（ESP32-WROOM-32E）
//!
//! ## 输入
//! - 摇杆 X 轴：IO32（ADC1_CH4）
//! - 摇杆 Y 轴：IO33（ADC1_CH5）
//! - 摇杆按钮：IO12  ⚠️ strapping pin，需 Pull::Down
//! - 按钮 1-4  ：IO27 / IO13 / IO25 / IO23，按下拉低，使用内部上拉
//! - 彩灯输出  ：IO15  ⚠️ strapping pin；4 颗并联彩灯（阳极接 IO15、阴极接 GND）驱动脚，推挽输出置高点亮（详见 pins::COLOR_LED）
//! - 旋钮 1    ：IO36（ADC1_CH0，SENSOR_VP）
//! - 旋钮 2    ：IO39（ADC1_CH3，SENSOR_VN）
//!
//! ## 输出
//! - LED 1     ：IO5   ⚠️ strapping pin，上电可能短暂高电平
//! - LED 2     ：IO18
//!
//! ## 通信
//! - I²C SDA   ：IO21
//! - I²C SCL   ：IO22
//! - OLED 地址 ：0x3C（SSD1306 128x64）

/// 引脚编号（对应 ESP32 GPIO 编号）
///
/// 用作文档/断言参考。实际取用 peripherals.GPIOxx 时仍需在 main 中显式引用。
pub mod pins {
  // ==== 摇杆 ====
  /// 摇杆 X 轴（ADC1_CH4）
  pub const JOYSTICK_X: u8 = 32;
  /// 摇杆 Y 轴（ADC1_CH5）
  pub const JOYSTICK_Y: u8 = 33;
  /// 摇杆按下键 ⚠️ strapping pin
  pub const JOYSTICK_BTN: u8 = 12;

  // ==== 按钮（4 个通用按钮）====
  pub const BUTTON_1: u8 = 27;
  pub const BUTTON_2: u8 = 13;
  pub const BUTTON_3: u8 = 25;
  pub const BUTTON_4: u8 = 23;

  // ==== 彩灯（IO15）====
  /// 彩灯驱动脚（IO15，驱动 4 颗并联彩灯 LED1–LED4）⚠️ strapping pin
  ///
  /// 原理图实测接线：4 颗 LED 并联，**阳极接 IO15、阴极接 GND**（右轨经 LED3 接地），
  /// 每路带限流电阻。要点亮必须由 IO15 **主动输出高电平**灌电流，因此本脚配成
  /// 推挽输出（`active_high = true`，置高 = 亮），由 `led_effects_task` 驱动闪烁。
  ///
  /// 历史说明：早期固件把它当"接地拨键"读输入，但该网络无接地通路 + 内部弱上拉
  /// （~45kΩ，几十 µA）带不动 LED，故读输入恒为 HIGH、彩灯也不亮；现改为输出驱动。
  /// ⚠️ strapping：复位阶段需为高才正常启动，故在 app 启动后（复位采样完成）再配输出。
  pub const COLOR_LED: u8 = 15;

  // ==== 旋钮 ====
  /// 旋钮 1（ADC1_CH0，SENSOR_VP，仅输入）
  pub const KNOB_1: u8 = 36;
  /// 旋钮 2（ADC1_CH3，SENSOR_VN，仅输入）
  pub const KNOB_2: u8 = 39;

  // ==== 电池电压测量 ====
  /// 电池电压 ADC（ADC1_CH6，仅输入，未与 Wi-Fi 冲突）
  ///
  /// 硬件接线：
  /// ```text
  ///  VBAT ──[R1=100kΩ]──┬── GPIO34
  ///                    │
  ///                  [R2=100kΩ]
  ///                    │
  ///                   GND
  /// ```
  /// 分压比 1/2 → 4.2V(满) 到 GPIO34 = 2.1V；配合 Attenuation::_11dB 满量程 3.3V 可测。
  pub const VBAT_ADC: u8 = 34;

  // ==== LED 输出 ====
  /// LED 1 ⚠️ strapping pin
  pub const LED_1: u8 = 5;
  /// LED 2
  pub const LED_2: u8 = 18;

  // ==== I²C（OLED）====
  pub const I2C_SDA: u8 = 21;
  pub const I2C_SCL: u8 = 22;
}

/// 调优参数
pub mod tuning {
  // ==== ADC ====
  /// ESP32 ADC 12-bit 满量程
  pub const ADC_MAX: u16 = 4095;
  /// ADC 中位（摇杆归中时的理想值）
  pub const ADC_MID: u16 = 2048;
  /// 摇杆死区（ADC 原始值）：以 MID 为中心，±此值内视为归中
  pub const JOYSTICK_DEADZONE: u16 = 200;
  /// 归一化输出范围（摇杆：-AXIS_RANGE..+AXIS_RANGE；旋钮：0..AXIS_RANGE）
  pub const AXIS_RANGE: i16 = 1000;
  /// ADC 移动平均滤波窗口大小（越大越稳但越迟钝）
  pub const ADC_FILTER_WINDOW: usize = 4;
  /// 摇杆上电校准采样次数
  ///
  /// 启动时对每个轴连续采样求平均，作为该轴的真实静止中值 `zero_offset`。
  /// 32 次采样在 ADC 白噪声 ±10 的量级下，均值 stderr 约 ±1.8，足够稳定。
  pub const JOYSTICK_CALIBRATION_SAMPLES: u16 = 32;
  /// 摇杆校准拒绝阈值（ADC 原始值）
  ///
  /// 上电时若用户已经把摇杆推歪，采样均值会远离 [`ADC_MID`]。为避免把
  /// 错误的中值写入 `zero_offset`，此偏离超过该阈值时拒绝校准，
  /// 回退到理论中值 [`ADC_MID`]。选 500 ≈ 满量程的 12%，覆盖典型个体
  /// 差异（一般 <10%），又能识别明显的推杆。
  pub const JOYSTICK_CALIBRATION_MAX_OFFSET: u16 = 500;

  // ==== 按键消抖 ====
  /// 按键消抖时长（毫秒）
  pub const DEBOUNCE_MS: u64 = 20;

  // ==== 扫描周期 ====
  /// 输入扫描周期（毫秒，10ms = 100Hz）
  pub const INPUT_SCAN_INTERVAL_MS: u64 = 10;
  /// 发送周期（毫秒，33ms ≈ 30Hz）
  pub const TRANSMIT_INTERVAL_MS: u64 = 33;
}

/// OLED 显示屏配置
pub mod display {
  use ssd1306::rotation::DisplayRotation;

  /// SSD1306 I²C 地址
  pub const OLED_ADDR: u8 = 0x3C;
  /// 屏幕宽度（像素）
  pub const OLED_WIDTH: u16 = 128;
  /// 屏幕高度（像素）
  pub const OLED_HEIGHT: u16 = 64;
  /// I²C 时钟频率（400 kHz）
  pub const I2C_FREQ_HZ: u32 = 400_000;

  /// 屏幕物理安装方向补偿
  ///
  /// SSD1306 的 4 个 `DisplayRotation` 值本质是 (`SegmentRemap`, `ReverseComDir`) 的
  /// 4 种组合（X 翻 / Y 翻 各 2 种），并非数学意义上的连续旋转：
  ///
  /// | 值           | SegmentRemap | ReverseComDir | 相对 `Rotate0` 效果 |
  /// |--------------|:------------:|:-------------:|--------------------|
  /// | `Rotate0`    | true         | true          | 库定义的"正方向" |
  /// | `Rotate90`   | false        | true          | X 翻（左右镜像）|
  /// | `Rotate180`  | false        | false         | X 翻 + Y 翻（真 180° 旋转）|
  /// | `Rotate270`  | true         | false         | Y 翻（上下镜像）|
  ///
  /// 因手柄外壳把 OLED 装反了 180°，屏幕整体倒置显示更符合视觉方向。
  /// 若发现文字方向不对，请依次尝试 `Rotate0/90/180/270` 定位物理方向。
  pub const OLED_ROTATION: DisplayRotation = DisplayRotation::Rotate180;

  /// OLED 刷新周期（毫秒；≈ 20 Hz，与传输 30 Hz 独立，减轻 I²C 压力）
  pub const REFRESH_INTERVAL_MS: u64 = 50;
  /// 字体单字符宽度（FONT_6X10，像素）
  pub const FONT_W: i32 = 6;
  /// 字体单字符高度（FONT_6X10，像素）
  pub const FONT_H: i32 = 10;
  /// 行高（含行间距，像素）
  pub const LINE_H: i32 = 10;
  /// 单行文本最大字符数（128 / 6 ≈ 21，留一个像素余量取 21）
  pub const LINE_CHARS: usize = 21;
}

/// 电池电量测量配置
///
/// # 两种工作模式
/// - **真实测量**（[`SIMULATE`] = false）：从 GPIO34 通过 1/2 分压电路读电压
/// - **模拟递减**（[`SIMULATE`] = true）：每次采样电量 -1%，到 0 后回到 100
///   —— 用于**无实际测量硬件**时验证 UI/BLE 电量通路
pub mod battery {
  /// 采样周期（毫秒）
  ///
  /// 电池电压变化很慢（分钟级），5 秒采一次已经足够，避免高频采样浪费 CPU。
  pub const SAMPLE_INTERVAL_MS: u64 = 5_000;

  /// 滑动平均窗口（次采样）
  ///
  /// 8 次 × 5 秒 = 40 秒时间窗，能平滑掉短时电流波动导致的电压抖动。
  pub const FILTER_WINDOW: usize = 8;

  /// 是否走模拟模式（无 VBAT 分压硬件时用 true）
  ///
  /// TODO(hardware): 实际焊接分压电路后改为 false。
  pub const SIMULATE: bool = true;

  /// 分压系数（VBAT 电压 = ADC 读到的电压 × 此值）
  ///
  /// 100 kΩ + 100 kΩ 分压时为 2.0。若采用 220kΩ + 100kΩ（分压 1/3.2），则为 3.2。
  pub const DIVIDER_RATIO: f32 = 2.0;

  /// ADC 参考电压（V，Attenuation::_11dB 时约 3.3V）
  pub const ADC_VREF_V: f32 = 3.3;

  /// 电池空电电压（V，锂电池 3.3V 视为 0%）
  pub const BATTERY_MIN_V: f32 = 3.30;

  /// 电池满电电压（V，锂电池 4.2V 视为 100%）
  pub const BATTERY_MAX_V: f32 = 4.20;
}

/// 密钥环（O 选项：HMAC 密钥轮换）
///
/// # 迁移说明
/// 全部密钥常量已下沉到 [`controller_protocol::config::keyring`] 供 dashboard/WASM 端
/// 复用；本模块作为向后兼容的 re-export。旧路径 `crate::config::keyring::*`
/// 全部保持可用。
pub mod keyring {
  pub use controller_protocol::config::keyring::{
    DEFAULT_KEY_ID, KEY_SLOTS, SECRET_V1, SECRET_V2, SHARED_SECRETS,
  };
}

/// 持久化配置（P 选项：NVS 真机落盘）
///
/// # 存储后端切换
/// [`USE_NVS_STORAGE`] 常量决定使用哪个 [`crate::hal::persist::PersistentStorage`] 实现：
///
/// | 值 | 后端                | 用途                                       |
/// |----|---------------------|--------------------------------------------|
/// | `false` | [`InMemoryStorage`](crate::hal::persist::InMemoryStorage) | 无真机调试期间；重启即丢失 |
/// | `true`  | [`NvsStorage`](crate::hal::persist::NvsStorage)             | 真机部署；落盘到 flash NVS 分区 |
///
/// # Flash 布局（双缓冲）
/// 我们复用 ESP-IDF 默认 partition table 的 nvs 分区（0x9000..0xE000, 20KB）
/// 的前 8KB，分成两个 4KB slot 做双缓冲：
///
/// ```text
///   0x9000..0x9FFF  slot A (SECTOR_SIZE = 4KB，实际只用前 12B + CRC)
///   0xA000..0xAFFF  slot B (SECTOR_SIZE = 4KB，实际只用前 12B + CRC)
///   0xB000..0xDFFF  预留（未来 partition 扩展）
/// ```
///
/// # 双缓冲策略
/// 每次写入交替使用 slot A / slot B（原子锁定其中一个），读取时选择两个 slot
/// 中 CRC 通过且 `last_seq` 更大的那份 —— 即使写入过程中断电导致一份损坏，
/// 另一份仍可恢复。
///
/// # 与 ESP-IDF NVS 库的冲突
/// 若未来引入官方 `nvs_flash` 库，两者会争抢同一 partition。届时应把 P 分区
/// 迁移到自定义 partition-table.csv 中的独立 slot（例如 `storage` 类型分区）。
pub mod persist {
  /// 是否启用真机 NVS 落盘（P 选项）
  ///
  /// - `false`（当前默认）：走 [`InMemoryStorage`](crate::hal::persist::InMemoryStorage)，
  ///   重启即丢失。**无真机时的稳态**。
  /// - `true`：走 [`NvsStorage`](crate::hal::persist::NvsStorage)，落盘到 flash。
  ///   接入真机 + 首次烧录时改成 true 即可，无需其它代码改动。
  ///
  /// # 为什么不用 feature flag？
  /// feature 需要重新 build、影响 crate 元数据；此处只是"选择一个后端"，
  /// 直接用编译期常量更简单，编译器会把另一分支 dead-code 消除。
  pub const USE_NVS_STORAGE: bool = false;

  /// NVS 分区起始 flash offset（字节）
  ///
  /// 对应 ESP-IDF 默认 partition table 中 `nvs` 分区的起始地址。
  pub const NVS_PARTITION_OFFSET: u32 = 0x9000;

  /// 单个 slot 大小（字节）—— 与 flash sector 对齐
  ///
  /// esp-storage 的 [`FlashStorage::SECTOR_SIZE`](esp_storage::FlashStorage::SECTOR_SIZE) = 4096。
  /// 写入时会擦除整个 sector，因此 slot 必须至少 4096 字节。
  pub const SLOT_SIZE: u32 = 4096;

  /// Slot A 起始 offset
  pub const SLOT_A_OFFSET: u32 = NVS_PARTITION_OFFSET;

  /// Slot B 起始 offset
  pub const SLOT_B_OFFSET: u32 = NVS_PARTITION_OFFSET + SLOT_SIZE;
}
