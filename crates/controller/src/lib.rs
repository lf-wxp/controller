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
//! - [`REGISTRY`]：已发现接收方目录全局单例（`comm::PeerRegistry`）
//! - [`protocol`]：协议帧（GamepadState + 二进制 Frame 编解码）
//! - [`self_test`]：启动自检（S1，验证协议核心不变式）
//! - [`transport`]：传输层抽象（Transport trait + defmt 日志实现等）
//! - [`ui`]：OLED 显示层（SSD1306 128x64 实时状态渲染）

pub mod config;
pub mod hal;
pub mod input;
pub mod metrics;
pub mod protocol;
pub mod self_test;
pub mod transport;
pub mod ui;

/// 全局 Peer 目录 —— 手柄侧唯一的接收方注册表。
///
/// 多个任务共享同一份 registry：
/// - `transport::esp_now::esp_now_receive_task`：`AnnounceReply` 入库（写）
/// - `ui::selector`：候选列表快照（读）
/// - `bin/main`：启动自检 / 空态判断（读）
///
/// `PeerRegistry::new()` 是 `const fn`，可静态初始化（内部
/// `Mutex<CriticalSectionRawMutex, RefCell<..>>` 提供关中断互斥）。
pub static REGISTRY: comm::PeerRegistry = comm::PeerRegistry::new();

/// 会话密钥环 —— 供 [`comm::Notifier`] 双身份 handler 内部生成 seq / 选择 active key_id。
///
/// # 背景
/// - `comm::Notifier` 的 `run_receive_loop` 需要一个 `&'static Keyring`：
///   * 当自动响应 `AnnounceReply` 触发 `AssignId` 时，用 `keyring.next_seq()` 取新 seq
///   * 当自动回 Ack 时，用 `keyring.active()` 取当前 key_id
/// - 手柄侧本来就有一个全局递增的 Command seq（`ASSIGN_SEQ` 等分散的 AtomicU32）；
///   迁移到 comm 后统一交给这一份 `Keyring` 管理。
///
/// `Keyring::new()` 是 `const fn`，默认 active = [`comm::DEFAULT_KEY_ID`]（K0）；
/// 若未来需要密钥轮换，用 `SESSION_KEYRING.rotate_to(...)` 显式切换。
pub static SESSION_KEYRING: comm::Keyring = comm::Keyring::new();
