//! # protocol
//!
//! ESP32 游戏手柄的**纯协议层** —— 无硬件依赖、可跨 target 复用。
//!
//! ## 职责
//! 定义"输入状态"与"传输帧"及二者的编解码，包括：
//! - [`state::GamepadState`]：摇杆/按钮/旋钮的完整快照
//! - [`frame::Frame`]：手柄 → Host 的广播帧（25 字节）
//! - [`command::Command`]：Host → 手柄的反向命令（24 字节，HMAC 签名）
//! - [`response::CommandResponse`]：手柄 → Host 的命令回执（24 字节，HMAC 签名）
//! - [`replay::AntiReplayWindow`]：64-bit 滑动窗口抗重放
//! - [`auth`]：HMAC-SHA256 计算 + session nonce
//! - [`crc::crc16_ibm`]：CRC-16-IBM 校验
//!
//! ## 设计原则
//! - **no_std by default**：可在 esp32、wasm32、host 等任意 target 上编译
//! - **纯类型 + 纯函数**：不做 I/O，只做数据结构定义和字节序列化，便于单元测试
//! - **定长二进制帧**：便于接收端定长解析，避开 varint/length-prefixed 复杂性
//! - **little-endian**：ESP32 是小端 CPU，直接内存序列化最快
//! - **三重防御**：CRC（抗噪声）+ HMAC-SHA256（抗伪造）+ Anti-Replay 窗口（抗重放）
//!
//! ## Feature 门控
//! - `defmt`：为错误类型/状态类型启用 `defmt::Format` trait（手柄端启用）
//! - `serde`：为所有公共数据类型启用 `Serialize`/`Deserialize`（WASM/Dashboard 启用）
//! - `std`：启用 std API（proptest 测试等）
//!
//! ## 数据流
//! ```text
//! HAL 层输入 ──► GamepadState ──encode──► Frame ──serialize──► [u8; FRAME_LEN]
//! ```
//!
//! ## 三种协议帧
//! | 类型             | 长度 | Magic  | 版本 | 认证 | 抗重放           | 密钥轮换                    | 方向             |
//! |------------------|------|--------|------|------|------------------|-----------------------------|------------------|
//! | Frame            | 25 B | 0xC71E | 2    | 无   | 无               | 无                          | 手柄 → Host 广播 |
//! | Command          | 24 B | 0xCB01 | 5    | HMAC | seq+per-key 窗口 | 4-bit key_id → SHARED_SECRETS | Host → 手柄      |
//! | CommandResponse  | 24 B | 0xCB02 | 5    | HMAC | req_seq          | 同上                        | 手柄 → Host      |
//!
//! Frame（手柄状态）不需要 HMAC/防重放 —— 它是**只读广播**，攻击者伪造 Frame
//! 只能让 Host 看到假状态，不会让手柄执行动作。Command / Response 才是"控制面"，
//! 必须签名 + 防重放。

#![no_std]
#![deny(clippy::correctness)]
#![warn(clippy::suspicious, clippy::style)]

pub mod auth;
pub mod codec;
pub mod command;
pub mod config;
pub mod crc;
pub mod frame;
pub mod replay;
pub mod response;
pub mod state;

// ============================================================
// 公共 API 面（proj-pub-use-reexport）
// ============================================================

pub use codec::{DecodeError, FRAME_LEN, decode_frame, encode_frame};
pub use command::{
  COMMAND_LEN, COMMAND_MAGIC, COMMAND_VERSION, Command, CommandBody, CommandDecodeError,
  CommandKind, decode_command, encode_command,
};
pub use frame::{FRAME_MAGIC, Frame, FrameHeader, PROTOCOL_VERSION};
pub use replay::{AntiReplayWindow, ReplayError, WINDOW_WIDTH};
pub use response::{
  CommandResponse, ErrorCode, RESPONSE_LEN, RESPONSE_MAGIC, RESPONSE_VERSION, ResponseBody,
  ResponseDecodeError, ResponseKind, decode_response, encode_response, peek_nonce_hello,
};
pub use state::{ButtonBits, GamepadState};

// K3: session nonce API + O: KeyId newtype 单独 re-export（便于 transport 层使用）
pub use auth::{KEY_ID_MAX, KeyId, KeyIdError, init_session_nonce, session_nonce};
