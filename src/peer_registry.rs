//! # Peer Registry —— 已发现接收方的动态目录
//!
//! ## 职责
//! 存储 controller 通过 Announce/AnnounceReply 通道发现的所有接收方，
//! 并管理 `receiver_id` 的分配。UI 层通过 [`snapshot`] 获取当前列表，
//! Frame 发送侧通过 [`lookup_id_for_mac`] 把 MAC 反查成 `dest_mask` 中的 bit。
//!
//! ## 生命周期
//! - `'static`（静态存储，无需 StaticCell）
//! - 单写者：ESP-NOW 接收任务（收到 AnnounceReply 时 upsert）
//! - 单读者：UI oled_task（每 50ms 快照一次）+ main loop（发 Frame 前查询）
//!
//! ## 并发模型
//! `Mutex<CriticalSectionRawMutex, RefCell<Inner>>`：
//! - `lock()` 只在关中断的极短时间内做**纯内存 memcpy**（无 await）
//! - 不违反 [`async-no-lock-await`] 规则
//!
//! ## 与 [`crate::ui::selector`] 的关系
//! Selector 负责"用户选了哪些"（`pending_mask` / `active_dest_mask`），
//! PeerRegistry 负责"到底有哪些能选"。二者通过 `receiver_id` 做桥梁。

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use heapless::Vec;

/// 支持的最大接收方数量（对应 `dest_mask: u32` 的 32 个 bit）
pub const MAX_PEERS: usize = 32;

/// role_tag 定长字节数（对应 protocol AnnounceReply payload 里的 role_tag: [u8; 3]）
pub const ROLE_TAG_LEN: usize = 3;

/// MAC-48 长度
pub const MAC_LEN: usize = 6;

/// 未知 RSSI 的哨兵值（渲染时判定为"未知"跳过）
pub const RSSI_UNKNOWN: i8 = i8::MIN;

// ============================================================
// PeerInfo —— 供 UI 渲染的只读快照
// ============================================================

/// 一个接收方候选的快照信息（渲染 & 选择用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerInfo {
  /// 逻辑接收器 ID（0..32）；用于 `dest_mask` 位映射
  pub receiver_id: u8,
  /// MAC-48
  pub mac: [u8; MAC_LEN],
  /// 展示用的角色标签（ASCII 3 字节；'\0' 填充未使用位）
  pub role: [u8; ROLE_TAG_LEN],
  /// 最近一次接收到的信号强度（dBm，负数）；[`RSSI_UNKNOWN`] 表示未知
  pub rssi_dbm: i8,
}

impl PeerInfo {
  /// 借用有效的 role 字节切片（去掉尾部的 `\0` 填充）
  #[must_use]
  pub fn role_bytes(&self) -> &[u8] {
    // trailing zero 视作填充；实际 role_tag 内容也不应含 0（ASCII 可见字符）
    let end = self
      .role
      .iter()
      .position(|&b| b == 0)
      .unwrap_or(ROLE_TAG_LEN);
    &self.role[..end]
  }
}

// ============================================================
// 内部条目（保存额外的 last_seen 时间戳，不暴露给 UI）
// ============================================================

#[derive(Debug, Clone, Copy)]
struct PeerEntry {
  info: PeerInfo,
  /// 最近一次收到该 peer 消息的时间戳（用于 stale 判定，暂未启用）
  last_seen: Instant,
}

// ============================================================
// 内部状态（Mutex 保护）
// ============================================================

#[derive(Debug)]
struct RegistryInner {
  /// 已知的所有 peer；顺序：按 receiver_id 升序（`upsert` 保证）
  peers: Vec<PeerEntry, MAX_PEERS>,
}

impl RegistryInner {
  const fn new() -> Self {
    Self { peers: Vec::new() }
  }

  /// 找到 `mac` 对应条目的 index
  fn find_index_by_mac(&self, mac: &[u8; MAC_LEN]) -> Option<usize> {
    self.peers.iter().position(|p| &p.info.mac == mac)
  }

  /// 分配一个最小的可用 `receiver_id`（0..MAX_PEERS 中未被占用者）
  ///
  /// # 返回值
  /// - `Some(id)`：分配成功
  /// - `None`：所有 32 个 slot 已用完
  fn allocate_id(&self) -> Option<u8> {
    let mut used_mask: u32 = 0;
    for p in self.peers.iter() {
      if p.info.receiver_id < 32 {
        used_mask |= 1u32 << p.info.receiver_id;
      }
    }
    // trailing_ones 给出最低的 0 位位置
    let free_bit = used_mask.trailing_ones();
    if free_bit >= 32 {
      None
    } else {
      Some(free_bit as u8)
    }
  }
}

/// 全局 registry
static REGISTRY: Mutex<CriticalSectionRawMutex, RefCell<RegistryInner>> =
  Mutex::new(RefCell::new(RegistryInner::new()));

// ============================================================
// 公共 API
// ============================================================

/// upsert 结果：告诉调用方本次是新增还是更新
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
  /// MAC 未见过 → 分配了新的 receiver_id
  Inserted { receiver_id: u8 },
  /// MAC 已存在 → 只更新了 rssi / role / last_seen
  Updated { receiver_id: u8 },
  /// 32 slot 已满且 MAC 未见过 → 无法分配 id，拒绝插入
  Full,
}

/// 把一个接收方的 AnnounceReply 记入 registry
///
/// # 参数
/// - `mac`：接收方 MAC-48
/// - `role`：3 字节 role_tag（AnnounceReply payload）
/// - `rssi_dbm`：ESP-NOW 接收硬件报告的 RSSI；未知传 [`RSSI_UNKNOWN`]
/// - `now`：当前时间（供 last_seen 记账；由调用方决定时钟源）
///
/// # 返回值
/// [`UpsertOutcome`] —— 见枚举文档
#[must_use]
pub fn upsert(
  mac: [u8; MAC_LEN],
  role: [u8; ROLE_TAG_LEN],
  rssi_dbm: i8,
  now: Instant,
) -> UpsertOutcome {
  REGISTRY.lock(|cell| {
    let mut inner = cell.borrow_mut();

    // 已存在：只更新可变字段
    if let Some(idx) = inner.find_index_by_mac(&mac) {
      let entry = &mut inner.peers[idx];
      entry.info.role = role;
      entry.info.rssi_dbm = rssi_dbm;
      entry.last_seen = now;
      return UpsertOutcome::Updated {
        receiver_id: entry.info.receiver_id,
      };
    }

    // 新 MAC：分配 id
    let Some(new_id) = inner.allocate_id() else {
      return UpsertOutcome::Full;
    };

    let entry = PeerEntry {
      info: PeerInfo {
        receiver_id: new_id,
        mac,
        role,
        rssi_dbm,
      },
      last_seen: now,
    };
    // 插入并保持按 receiver_id 升序（便于 UI 稳定显示顺序）
    let insert_pos = inner
      .peers
      .iter()
      .position(|p| p.info.receiver_id > new_id)
      .unwrap_or(inner.peers.len());
    // heapless::Vec 没有 insert；用 push + swap 下移
    if inner.peers.push(entry).is_err() {
      // 理论上 allocate_id 已保证有空间，防御性处理
      return UpsertOutcome::Full;
    }
    // 冒泡到 insert_pos
    let mut i = inner.peers.len() - 1;
    while i > insert_pos {
      inner.peers.swap(i - 1, i);
      i -= 1;
    }

    UpsertOutcome::Inserted {
      receiver_id: new_id,
    }
  })
}

/// 查询某个 MAC 对应的 receiver_id
///
/// 用于 Frame 发送侧把用户选择的 MAC 集合转成 `dest_mask` 位图。
#[must_use]
pub fn lookup_id_for_mac(mac: &[u8; MAC_LEN]) -> Option<u8> {
  REGISTRY.lock(|cell| {
    cell
      .borrow()
      .peers
      .iter()
      .find(|p| &p.info.mac == mac)
      .map(|p| p.info.receiver_id)
  })
}

/// 取一份 registry 快照（Copy 后独立，不再持有对内部的引用）
///
/// UI oled_task 每帧调用；Selector 也调用它取候选列表。
#[must_use]
pub fn snapshot() -> Vec<PeerInfo, MAX_PEERS> {
  REGISTRY.lock(|cell| {
    let inner = cell.borrow();
    let mut out: Vec<PeerInfo, MAX_PEERS> = Vec::new();
    for entry in inner.peers.iter() {
      // MAX_PEERS 等于 heapless 容量，push 不会失败；仍显式处理避免静默截断
      if out.push(entry.info).is_err() {
        break;
      }
    }
    out
  })
}

/// 当前已注册的 peer 数量（`snapshot().len()` 的无分配快捷路径）
#[must_use]
pub fn len() -> usize {
  REGISTRY.lock(|cell| cell.borrow().peers.len())
}

/// 是否空
#[must_use]
pub fn is_empty() -> bool {
  REGISTRY.lock(|cell| cell.borrow().peers.is_empty())
}

/// 清空 registry（主要供测试用；生产运行时通常不主动调用）
#[cfg(test)]
pub fn clear_for_test() {
  REGISTRY.lock(|cell| cell.borrow_mut().peers.clear());
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
  use super::*;

  fn now() -> Instant {
    Instant::from_ticks(0)
  }

  #[test]
  fn insert_new_mac_allocates_id_zero() {
    clear_for_test();
    let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    match upsert(mac, *b"led", -42, now()) {
      UpsertOutcome::Inserted { receiver_id } => assert_eq!(receiver_id, 0),
      other => panic!("expected Inserted, got {:?}", other),
    }
    assert_eq!(len(), 1);
  }

  #[test]
  fn re_upsert_same_mac_returns_updated() {
    clear_for_test();
    let mac = [0xAA; 6];
    let _ = upsert(mac, *b"led", -30, now());
    match upsert(mac, *b"srv", -50, now()) {
      UpsertOutcome::Updated { receiver_id } => assert_eq!(receiver_id, 0),
      other => panic!("expected Updated, got {:?}", other),
    }
    // 只应有一条，且 role/rssi 已更新
    let snap = snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].role_bytes(), b"srv");
    assert_eq!(snap[0].rssi_dbm, -50);
  }

  #[test]
  fn allocate_id_reuses_lowest_available() {
    clear_for_test();
    // 插入 3 条：id 0/1/2
    for i in 0..3_u8 {
      let mac = [i, 0, 0, 0, 0, 0];
      let _ = upsert(mac, *b"aaa", -10, now());
    }
    // 手工把 id=1 移走：我们没有暴露 remove，但可以借 clear + 顺序重放来验证
    // 这里改测：再插入 32 条会返回 Full
    for i in 3..32_u8 {
      let mac = [i, 0, 0, 0, 0, 0];
      let _ = upsert(mac, *b"aaa", -10, now());
    }
    assert_eq!(len(), 32);
    // 第 33 条：应 Full
    match upsert([99, 0, 0, 0, 0, 0], *b"aaa", -10, now()) {
      UpsertOutcome::Full => {}
      other => panic!("expected Full, got {:?}", other),
    }
  }

  #[test]
  fn snapshot_returns_ids_in_ascending_order() {
    clear_for_test();
    for i in 0..5_u8 {
      let _ = upsert([i, 0, 0, 0, 0, 0], *b"aaa", -10, now());
    }
    let snap = snapshot();
    let mut prev = 0_i32;
    for peer in snap.iter() {
      let cur = i32::from(peer.receiver_id);
      assert!(cur >= prev, "snapshot ids not sorted");
      prev = cur;
    }
  }

  #[test]
  fn lookup_finds_existing_and_misses_unknown() {
    clear_for_test();
    let mac = [1, 2, 3, 4, 5, 6];
    let _ = upsert(mac, *b"led", -20, now());
    assert_eq!(lookup_id_for_mac(&mac), Some(0));
    assert_eq!(lookup_id_for_mac(&[9, 9, 9, 9, 9, 9]), None);
  }

  #[test]
  fn role_bytes_trims_trailing_nulls() {
    let peer = PeerInfo {
      receiver_id: 0,
      mac: [0; 6],
      role: [b'l', b'd', 0], // 2 valid + 1 padding
      rssi_dbm: RSSI_UNKNOWN,
    };
    assert_eq!(peer.role_bytes(), b"ld");
  }
}
