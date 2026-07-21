//! # comm
//!
//! 一个**可复用的双向通信编排 crate**：把"发送 / 接收 / 发现 / 回复 / peer
//! 列表 / receiver 选择 / 密钥管理 / 抗重放"这些**流程**统一封装成 `Notifier`
//! 和 `Receiver` 两个门面。使用者只需实现一个 [`CommLink`] trait，就能在任
//! 意物理链路（ESP-NOW、UART、TCP、内存回环…）上跑起完整的控制协议。
//!
//! ## 目标使用体验
//!
//! ### 发送端（controller / notifier）
//! ```ignore
//! use comm::prelude::*;
//!
//! let notifier = Notifier::builder()
//!     .link(my_link)
//!     .role_tag(*b"joy")
//!     .build();
//! notifier.spawn(spawner);
//!
//! // 主循环
//! notifier.discover();                    // 主动发起一次发现
//! for peer in notifier.peers() { /* ... */ }
//! notifier.select_targets(0b0000_0011);   // 选择 receiver 0 + 1
//! notifier.send_frame(&frame);            // 广播状态帧
//! ```
//!
//! ### 接收端（receiver / led / motor）
//! ```ignore
//! use comm::prelude::*;
//!
//! Receiver::builder()
//!     .link(my_link)
//!     .role_tag(*b"led")
//!     .command_handler(|_src, cmd| {
//!         match cmd.kind {
//!             CommandBody::LedBlink { .. } => turn_on_led(),
//!             _ => {}
//!         }
//!     })
//!     .build()
//!     .spawn(spawner);
//! ```
//!
//! ## 模块布局
//! - [`link`]      —— 唯一的硬件抽象层：`CommLink` trait
//! - [`keyring`]   —— 当前 `KeyId` + 各 slot 的 tx_counter
//! - [`replay`]    —— per-key-id 滑动窗口的实例化版
//! - [`peer_registry`] —— 已发现的 peer 目录（实例化，无全局 static）
//! - [`selector`]  —— pending / active 双状态目标选择器
//! - [`notifier`]  —— 发送端门面
//! - [`receiver`]  —— 接收端门面
//! - [`prelude`]   —— 常用 re-export
//!
//! ## 设计原则
//! - **no_std by default**，直接依赖 embassy 家族（`embassy-sync` /
//!   `embassy-time` / `embassy-futures`）；不做运行时无关抽象
//! - **`CommLink` 是唯一的可插拔点**——ESP-NOW / UART / loopback 各自实现
//! - **协议逻辑复用 `protocol`**，本 crate 只负责编排
//! - **零 heap 分配**：所有集合走 `heapless::Vec<T, N>`

#![no_std]
#![deny(clippy::correctness)]
#![warn(clippy::suspicious, clippy::style, missing_docs)]

// std 只在 host 侧集成测试 / loopback feature 才需要
#[cfg(any(test, feature = "loopback"))]
extern crate std;

pub mod keyring;
pub mod link;
pub mod notifier;
pub mod peer_registry;
pub mod receiver;
pub mod replay;
pub mod selector;

/// crate 内共享的命令帧派发逻辑
mod dispatch;

#[cfg(feature = "loopback")]
pub mod loopback;

pub mod prelude;

// ============================================================
// 公共 API 面（proj-pub-use-reexport）
// ============================================================

pub use keyring::{DEFAULT_KEY_ID, Keyring, KeyringError};
pub use link::{CommLink, LinkError, Packet};
pub use notifier::{
  DEFAULT_NONCE_BROADCAST_INTERVAL, EntropySource, Notifier, NotifierError, init_session,
  run_nonce_broadcast_loop,
};
pub use peer_registry::{
  MAC_LEN, MAX_PEERS, PeerInfo, PeerRegistry, ROLE_TAG_LEN, RSSI_UNKNOWN, UpsertOutcome,
};
pub use receiver::{CommandOutcome, CommandSource, Receiver, ReceiverError};
pub use replay::{ReplayCheckError, ReplayGuard};
pub use selector::{DestMask, Selector};

// ============================================================
// 角色语义别名（`api-descriptive-typealias`）
// ============================================================
//
// `Notifier` / `Receiver` 是最初的命名，能力语义清晰但**角色语义**容易误导——
// 两者在消息发送/接收能力层面已完全对称。为了让新代码能选择更精确的命名而不
// 破坏既有 API，提供两个 zero-cost `pub type` 别名：
//
// - `Coordinator<L>`：会话协调者（PeerRegistry / Selector / discover 的持有方）
// - `Endpoint<L>`：叶子端点（可主动 report / send_frame，但不做拓扑决策）
//
// 新老写法完全互通，任选其一即可；混用也不会引入编译或运行时开销。
// 详细分工表见 [`notifier`] 与 [`receiver`] 模块顶部的"# 角色定位"章节。

/// 会话协调者别名 —— 语义等价于 [`Notifier`]
pub type Coordinator<L> = Notifier<L>;

/// 叶子端点别名 —— 语义等价于 [`Receiver`]
pub type Endpoint<L> = Receiver<L>;

// 常用协议类型 re-export，避免用户额外 depend 一次 protocol
pub use protocol::{
  Command, CommandBody, CommandResponse, ErrorCode, Frame, GamepadState, KeyId, ResponseBody,
  session_nonce,
};

// ============================================================
// 编译期尺寸护栏（`mem-assert-type-size`）
// ============================================================
//
// 目的：这些类型都会被大量放进 `Signal`、`heapless::Vec` 或作为函数参数
// 频繁移动，一旦意外膨胀（例如 `ResponseBody` 增加一个大 payload 变体）会
// 立刻导致栈占用与拷贝成本失控。此处以**当前实测尺寸 + 少量余量**做上限，
// 若未来任意一处增长，编译期就会失败——迫使 reviewer 显式提高阈值并思考
// 影响面。数值是软目标，非硬性要求；随协议演进可上调，但严禁"随手上调"。
const _: () = {
  use core::mem::size_of;
  // Frame 主体：`GamepadState` + `dest_mask` + 若干头字段
  assert!(size_of::<Frame>() <= 64, "Frame size regression");
  // Command：`CommandBody` 是所有下行命令 enum，容易长胖
  assert!(size_of::<Command>() <= 64, "Command size regression");
  // CommandResponse：`ResponseBody` 里最大的变体决定尺寸
  assert!(
    size_of::<CommandResponse>() <= 64,
    "CommandResponse size regression"
  );
  // PeerInfo：会被塞进 32 长度的 heapless::Vec，务必紧凑
  assert!(
    size_of::<peer_registry::PeerInfo>() <= 16,
    "PeerInfo size regression"
  );
};
