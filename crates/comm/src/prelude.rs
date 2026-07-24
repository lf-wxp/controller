//! # Prelude —— 常用类型一站式 re-export
//!
//! ```ignore
//! use comm::prelude::*;
//! ```
//!
//! 保持极简：只 re-export 用户"每次都会用到"的类型；避免污染。

// 门面
pub use crate::notifier::{Notifier, NotifierError};
pub use crate::receiver::{CommandOutcome, CommandSource, Receiver, ReceiverError};

// 硬件层 trait
pub use crate::link::{CommLink, LinkError, Packet};

// 常用运行时状态
pub use crate::keyring::{DEFAULT_KEY_ID, Keyring};
pub use crate::peer_registry::{PeerInfo, PeerRegistry};
pub use crate::replay::ReplayGuard;
pub use crate::selector::{DEST_MASK_ALL, DEST_MASK_NONE, DestMask, Selector};

// 观测：丢弃计数快照（`comm::metrics::snapshot()` 的返回类型）
pub use crate::metrics::DropCounts;

// 协议类型
pub use protocol::{
  Command, CommandBody, CommandResponse, ErrorCode, Frame, GamepadState, KeyId, ResponseBody,
};
