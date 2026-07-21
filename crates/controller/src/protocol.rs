//! # 协议层 re-export shim
//!
//! 本文件把 [`protocol`] crate 的所有公共 API 重新导出到
//! `crate::protocol` 命名空间下，**保持与旧目录布局完全一致的路径**。
//!
//! 迁移前手柄代码到处写着 `use crate::protocol::{Frame, GamepadState};`，
//! 或 `use crate::protocol::state::ButtonBits;`。为了避免"因为拆 crate 就要
//! 改 14 个文件"的连锁反应，本 shim 保留完全等价的路径。
//!
//! ## 结构
//! - 子模块 re-export：`crate::protocol::{crc, auth, state, frame, ...}` 全部指向
//!   `protocol::{crc, auth, state, frame, ...}`
//! - 顶层 re-export：常用类型（`Frame`、`GamepadState`、`Command` 等）
//!   直接从 `crate::protocol` 拿

pub use protocol::{
  auth, codec, command, config as protocol_config, crc, frame, replay, response, state,
};

// 常用类型的顶层 re-export（对齐 protocol lib.rs 的 pub use 面）
pub use protocol::{
  AntiReplayWindow, ButtonBits, COMMAND_LEN, COMMAND_MAGIC, COMMAND_VERSION, Command, CommandBody,
  CommandDecodeError, CommandKind, CommandResponse, DecodeError, ErrorCode, FRAME_LEN, FRAME_MAGIC,
  Frame, FrameHeader, GamepadState, KEY_ID_MAX, KeyId, KeyIdError, PROTOCOL_VERSION, RESPONSE_LEN,
  RESPONSE_MAGIC, RESPONSE_VERSION, ResponseBody, ResponseDecodeError, ResponseKind, WINDOW_WIDTH,
  decode_command, decode_frame, decode_response, encode_command, encode_frame, encode_response,
  init_session_nonce,
};
