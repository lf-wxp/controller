//! 硬件抽象层（HAL）
//!
//! 提供通用、可复用的硬件组件封装：
//! - [`button`]：数字按钮 + 消抖
//! - [`switch`]：拨动开关
//! - [`analog`]：ADC 通用模拟量读取（滤波 + 归一化）
//! - [`joystick`]：双轴摇杆 + 按下键
//! - [`led`]：数字 LED 输出
//! - [`battery`]：电池电量监测（真实 ADC 或模拟递减）
//! - [`rng`]：硬件随机数（session nonce / 密钥派生的熵源，Q 选项）
//!
//! 这些组件对"硬件用途"完全无感——按钮不知道自己是"激光键"还是"巡视键"，
//! 只知道自己是一个"数字输入"。业务语义由上层（协议 / 应用）赋予。

pub mod analog;
pub mod battery;
pub mod button;
pub mod joystick;
pub mod led;
pub mod led_effects;
pub mod persist;
pub mod rng;
pub mod switch;

// 重导出常用类型
pub use analog::AnalogInput;
pub use button::{Button, ButtonState};
pub use joystick::{Joystick, JoystickReading};
pub use led::Led;
pub use persist::{InMemoryStorage, NvsError, NvsStorage, PersistentConfig, PersistentStorage};
pub use switch::Switch;
