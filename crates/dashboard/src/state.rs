//! # 全局 App 状态（RwSignal-based）
//!
//! Leptos 的响应式原语是 `RwSignal<T>`：读会自动订阅，写会自动重绘所有订阅者。
//! 本模块用几个粒度合适的 signal 保存 dashboard 运行时状态，通过
//! `provide_context::<AppState>()` 在组件树中共享。
//!
//! ## 粒度设计
//! - **不用**单个"大 State + RwSignal"包含所有字段（改一个字段全组件重绘）
//! - **改用**"多个 signal，每个持有一类相关字段"，让组件按需订阅

use std::collections::VecDeque;

use leptos::prelude::*;
use protocol::{ErrorCode, GamepadState, KeyId, ResponseBody};

/// 事件日志方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventDirection {
  /// 手柄 → Dashboard（帧、响应）
  Rx,
  /// Dashboard → 手柄（命令）
  Tx,
  /// 内部信息（连接、错误等）
  Info,
  /// 警告
  Warn,
}

impl EventDirection {
  /// 用于 UI 徽章的 CSS class 名
  pub const fn badge_class(self) -> &'static str {
    match self {
      Self::Rx => "badge-rx",
      Self::Tx => "badge-tx",
      Self::Info => "badge-info",
      Self::Warn => "badge-warn",
    }
  }

  /// 用于事件日志前缀的短标签
  pub const fn label(self) -> &'static str {
    match self {
      Self::Rx => "RX",
      Self::Tx => "TX",
      Self::Info => "IN",
      Self::Warn => "WN",
    }
  }
}

/// 事件日志条目
#[derive(Debug, Clone)]
pub struct EventEntry {
  /// 时间戳（毫秒；`performance.now()` 起点自浏览器）
  pub ts_ms: f64,
  /// 事件方向
  pub dir: EventDirection,
  /// 简短描述
  pub summary: String,
  /// 原始字节（可选，用于 hex dump）
  pub bytes: Option<Vec<u8>>,
}

impl EventEntry {
  /// 便捷构造：仅摘要
  pub fn info(summary: impl Into<String>) -> Self {
    Self {
      ts_ms: now_ms(),
      dir: EventDirection::Info,
      summary: summary.into(),
      bytes: None,
    }
  }

  /// 便捷构造：警告
  pub fn warn(summary: impl Into<String>) -> Self {
    Self {
      ts_ms: now_ms(),
      dir: EventDirection::Warn,
      summary: summary.into(),
      bytes: None,
    }
  }

  /// 便捷构造：Rx 附带字节
  pub fn rx(summary: impl Into<String>, bytes: Vec<u8>) -> Self {
    Self {
      ts_ms: now_ms(),
      dir: EventDirection::Rx,
      summary: summary.into(),
      bytes: Some(bytes),
    }
  }

  /// 便捷构造：Tx 附带字节
  pub fn tx(summary: impl Into<String>, bytes: Vec<u8>) -> Self {
    Self {
      ts_ms: now_ms(),
      dir: EventDirection::Tx,
      summary: summary.into(),
      bytes: Some(bytes),
    }
  }
}

/// 连接状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnState {
  #[default]
  Disconnected,
  Connecting,
  Connected,
}

impl ConnState {
  /// UI 显示文本
  pub const fn label(self) -> &'static str {
    match self {
      Self::Disconnected => "未连接",
      Self::Connecting => "连接中...",
      Self::Connected => "已连接",
    }
  }

  /// 用于状态药丸（连接状态区）的 CSS class
  pub const fn css_state(self) -> &'static str {
    match self {
      Self::Disconnected => "conn-off",
      Self::Connecting => "conn-connecting",
      Self::Connected => "conn-on",
    }
  }
}

/// 事件日志最大长度（超过则丢弃最旧）
pub const EVENT_LOG_MAX_LEN: usize = 200;

/// 全局 App 状态 —— 通过 `provide_context` 注入
#[derive(Clone, Copy)]
pub struct AppState {
  /// 蓝牙连接状态
  pub conn: RwSignal<ConnState>,
  /// 电量百分比（Some = 已从 BatterySnapshot 收到）
  pub battery: RwSignal<Option<u8>>,
  /// 当前 session nonce（Some = 已从 NonceHello 收到）
  pub session_nonce: RwSignal<Option<u32>>,
  /// 用户选择的 key_id（发送命令时使用）
  pub key_id: RwSignal<KeyId>,
  /// 手柄输入状态实时快照
  pub gamepad: RwSignal<GamepadState>,
  /// 帧 seq（用于显示接收速率）
  pub last_frame_seq: RwSignal<u32>,
  /// dashboard 端发送命令的 tx_counter（防重放）
  pub tx_counter: RwSignal<u32>,
  /// 最近一条 Response 的解析结果（用于 UI 提示）
  pub last_response: RwSignal<Option<(u32, ResponseBody)>>,
  /// 事件日志（环形缓冲，最新在末尾）
  pub events: RwSignal<VecDeque<EventEntry>>,
  /// 已发现的接收方目录（由 `AnnounceReply` 经 BLE 转发上报后填充）
  pub receivers: RwSignal<Vec<PeerInfo>>,
}

impl AppState {
  /// 创建一份全新的 AppState（供 `App` 组件顶层初始化）
  pub fn new() -> Self {
    let state = Self {
      conn: RwSignal::new(ConnState::Disconnected),
      battery: RwSignal::new(None),
      session_nonce: RwSignal::new(None),
      key_id: RwSignal::new(KeyId::DEFAULT),
      gamepad: RwSignal::new(GamepadState::default()),
      last_frame_seq: RwSignal::new(0),
      tx_counter: RwSignal::new(0),
      last_response: RwSignal::new(None),
      events: RwSignal::new(VecDeque::with_capacity(EVENT_LOG_MAX_LEN)),
      receivers: RwSignal::new(Vec::with_capacity(MAX_PEERS)),
    };
    state.push_event(EventEntry::info("Dashboard 已启动，等待连接手柄"));
    state
  }

  /// 追加一条事件到日志（自动裁剪到 [`EVENT_LOG_MAX_LEN`]）
  pub fn push_event(&self, entry: EventEntry) {
    self.events.update(|events| {
      if events.len() >= EVENT_LOG_MAX_LEN {
        events.pop_front();
      }
      events.push_back(entry);
    });
  }

  /// 递增 tx_counter 并返回新值（>= 1，用作 Command.seq）
  pub fn next_tx_seq(&self) -> u32 {
    self.tx_counter.update(|c| *c = c.saturating_add(1));
    self.tx_counter.get_untracked()
  }
}

impl Default for AppState {
  fn default() -> Self {
    Self::new()
  }
}

// ============================================================
// 工具
// ============================================================

/// 当前时间（毫秒，`performance.now()` 起点自浏览器加载时刻）
///
/// 相比 `js_sys::Date::now()` 精度更高（子毫秒级）；WASM 无本地时钟，只能靠 JS。
pub fn now_ms() -> f64 {
  web_sys::window()
    .and_then(|w| w.performance())
    .map(|p| p.now())
    .unwrap_or(0.0)
}

/// 便捷：把 [`ErrorCode`] 转为 UI 友好字符串
pub const fn error_code_label(code: ErrorCode) -> &'static str {
  match code {
    ErrorCode::InvalidArgument => "参数不合法",
    ErrorCode::Unsupported => "命令暂不支持",
    ErrorCode::Busy => "内部忙",
  }
}

// ============================================================
// 接收方目录（Receiver Registry）
// ============================================================

/// 支持的最大接收方数量（对应协议 `dest_mask: u32` 的 32 个 bit）
pub const MAX_PEERS: usize = 32;

/// role_tag 定长字节数（对应协议 AnnounceReply payload 里的 `role_tag: [u8; 3]`）
pub const ROLE_TAG_LEN: usize = 3;

/// MAC-48 长度
pub const MAC_LEN: usize = 6;

/// 未知 RSSI 的哨兵值（渲染时判定为"未知"跳过）。
///
/// 与协议侧 `AnnounceReply.rssi_dbm` 的未知约定保持一致：协议文档
/// （`crates/protocol/src/response.rs`）规定 `-127` 表示未知。
pub const RSSI_UNKNOWN: i8 = -127;

/// 一个已发现接收方的展示信息
///
/// 字段刻意与控制器侧 `comm::PeerInfo` 保持一致（单一真相来源），
/// 便于两边语义对齐：`receiver_id` 自动分配（0..32），`role` 为定长 3 字节
/// ASCII（不足右侧补 `\0`），UI 显示时用 [`Self::role_str`] 去尾零。
///
/// 注意：不含 `Eq`——`last_seen_ms: f64` 不实现 `Eq`（仅 `PartialEq`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeerInfo {
  /// 逻辑接收器 ID（0..32）；用于 `dest_mask` 位映射
  pub receiver_id: u8,
  /// MAC-48
  pub mac: [u8; MAC_LEN],
  /// 展示用的角色标签（ASCII 3 字节；'\0' 填充未使用位）
  pub role: [u8; ROLE_TAG_LEN],
  /// 最近一次接收到的信号强度（dBm，负数）；[`RSSI_UNKNOWN`] 表示未知
  pub rssi_dbm: i8,
  /// 最近一次收到该 peer 消息的时间戳（`performance.now()` 起点，毫秒）
  pub last_seen_ms: f64,
}

impl PeerInfo {
  /// 借用有效的 role 字节切片（去掉尾部的 `\0` 填充）
  pub fn role_bytes(&self) -> &[u8] {
    let end = self
      .role
      .iter()
      .position(|&b| b == 0)
      .unwrap_or(ROLE_TAG_LEN);
    &self.role[..end]
  }

  /// role 的 UTF-8 字符串视图（用于 UI 渲染）
  pub fn role_str(&self) -> &str {
    core::str::from_utf8(self.role_bytes()).unwrap_or("???")
  }

  /// MAC 的 `aa:bb:cc:dd:ee:ff` 表示
  pub fn mac_str(&self) -> String {
    self
      .mac
      .iter()
      .map(|b| format!("{b:02x}"))
      .collect::<Vec<_>>()
      .join(":")
  }
}

impl AppState {
  /// 把一个接收方的 `AnnounceReply` 记入 receivers 目录
  ///
  /// 与控制器 `comm::PeerRegistry::upsert` 语义对齐：MAC 已存在则只更新
  /// `role`/`rssi`/`last_seen`；否则分配一个最低的空闲 `receiver_id` 后插入。
  pub fn upsert_receiver(&self, mac: [u8; MAC_LEN], role: [u8; ROLE_TAG_LEN], rssi_dbm: i8) {
    // `rssi_dbm` 直接来自协议解码（负数），无需调用方合成未知值：协议侧约定
    // `-127` 表示未知（见 [`RSSI_UNKNOWN`]），会原样透传而来，切勿传 0。
    let now = now_ms();
    self.receivers.update(|list| {
      // 已存在：只更新可变字段
      if let Some(peer) = list.iter_mut().find(|p| p.mac == mac) {
        peer.role = role;
        peer.rssi_dbm = rssi_dbm;
        peer.last_seen_ms = now;
        return;
      }
      // 新 MAC：分配一个最低的空闲 id（trailing_ones 给出最低的 0 位位置）
      let used_mask: u32 = list
        .iter()
        .filter(|p| p.receiver_id < 32)
        .fold(0u32, |mask, p| mask | (1u32 << p.receiver_id));
      let free_bit = used_mask.trailing_ones();
      if free_bit >= 32 {
        return; // 32 个 slot 已满：静默丢弃（只读面板不持久化，可接受）
      }
      list.push(PeerInfo {
        receiver_id: free_bit as u8,
        mac,
        role,
        rssi_dbm,
        last_seen_ms: now,
      });
      // 按 receiver_id 升序排列，与控制器 PeerRegistry 行为对齐（稳定 UI 显示顺序）
      list.sort_by_key(|p| p.receiver_id);
    });
  }
}
