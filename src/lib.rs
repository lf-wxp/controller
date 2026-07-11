#![no_std]

//! # 通用 ESP32 手柄控制器
//!
//! 硬件：ESP32-WROOM-32E
//! 框架：esp-hal 1.1.x（no_std）+ embassy 异步
//!
//! ## 模块划分
//! - [`config`]：硬件常量集中区（引脚编号、调优参数、显示屏配置）
//! - [`hal`]：硬件抽象层（按钮/开关/摇杆/旋钮/LED）
//! - [`input`]：输入聚合层（把 hal 组件聚合成一个 GamepadState 采样器）
//! - [`metrics`]：全局运行时观测计数器（Ack 覆盖 / flash 磨损）
//! - [`peer_registry`]：已发现接收方目录（MAC→receiver_id 映射 + AnnounceReply 入库）
//! - [`protocol`]：协议帧（GamepadState + 二进制 Frame 编解码）
//! - [`self_test`]：启动自检（S1，验证协议核心不变式）
//! - [`transport`]：传输层抽象（Transport trait + defmt 日志实现等）
//! - [`ui`]：OLED 显示层（SSD1306 128x64 实时状态渲染）

pub mod config;
pub mod hal;
pub mod input;
pub mod metrics;
pub mod peer_registry;
pub mod protocol;
pub mod self_test;
pub mod transport;
pub mod ui;
