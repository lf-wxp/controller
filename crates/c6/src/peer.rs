//! Peer 相关**常量集合**（receiver 侧）
//!
//! 从 v0.2.0 → v0.3.0：本模块原来的 [`PeerCtx`] 已迁到 [`comm`] 门面：
//! - `receiver_id` → `static AtomicU8`（配合 [`comm::Receiver::builder().my_id(...)`]）
//! - `replay` 抗重放 → [`comm::ReplayGuard`]（在 [`crate::radio`] 里作 `static`）
//! - `auth_warned` / `decode_warned` 首次告警 → 交给 comm 内部
//!   [`dispatch_packet`](comm::dispatch)（若未来需要精细化告警可再包一层）
//! - `assign()` mac 比对 → [`comm::notifier::run_receive_loop`] 自动完成
//!
//! 剩下的只是"UI/日志共享"的几个常量。
//!
//! [`PeerCtx`]: 已删除，见 git 历史
//! [`dispatch_packet`]: comm::dispatch

/// ESP-NOW 广播地址（`FF:FF:FF:FF:FF:FF`）。
///
/// 除了作为 [`comm::CommLink::BROADCAST`] 的常量来源外，UI 层也可以从这里读取。
pub const BROADCAST: [u8; 6] = [0xFF; 6];

/// 首次上电 / 未收到 `AssignId` 之前的占位 receiver_id。
///
/// 采用 comm 的"未分配"约定 [`comm::receiver::UNASSIGNED_ID`]（= `u8::MAX`），
/// **而非 `0`**：
/// - `0` 是一个**合法**的已分配 id（占 `dest_mask` 的第 0 位）。若拿它当"未分配"
///   哨兵，真正被分配到 `id = 0` 的 receiver 会被 UI 误判为"未分配"，且无法进入
///   comm 对未分配 receiver 的**宽限接收**（grace）路径。
/// - `u8::MAX` 落在合法范围 `0..=31` 之外，`1 << 255` 不会命中任何 `dest_mask`，
///   天然表达"尚未被寻址"，并与 [`comm::Receiver`] 的判定保持一致。
pub const INITIAL_RECEIVER_ID: u8 = comm::receiver::UNASSIGNED_ID;

/// receiver_id 上限（0..=31，对应 `dest_mask: u32` 的 32 个位）。
///
/// comm 内部不再检查此上限（`AssignId` 是内部动作），仅由 UI 层用于渲染断言。
pub const RECEIVER_ID_MAX: u8 = 31;

/// AnnounceReply 里的 `role_tag`：3 字节 ASCII，标识本 receiver 的角色。
///
/// 本项目是 "LCD Display Sink"，用 `lcd` 表示；不足右侧补 0。
pub const ROLE_TAG: [u8; 3] = *b"lcd";
