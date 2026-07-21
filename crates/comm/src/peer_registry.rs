//! # PeerRegistry —— 已发现 peer 的实例化目录
//!
//! ## 定位
//! `PeerRegistry<N>` 是普通结构体，容量泛型化，支持多实例；
//! 手柄 bin 侧以 `static REGISTRY: PeerRegistry = PeerRegistry::new()`
//! 的形式暴露唯一全局单例（见 `crates/controller/src/lib.rs`）。
//!
//! ## 使用侧
//! - [`Notifier`](crate::Notifier) 在收到 `AnnounceReply` 时 [`upsert`](Self::upsert)
//! - UI / 主循环通过 [`snapshot`](Self::snapshot) 拿只读列表渲染
//! - Frame 发送前 [`lookup_id_for_mac`](Self::lookup_id_for_mac) 反查 dest_mask
//!
//! ## 并发模型
//! `Mutex<CriticalSectionRawMutex, RefCell<Inner>>`：
//! - `lock()` 只做纯内存操作，无 `.await`
//! - 关中断时间极短（O(N) MAC 比较，N ≤ 32）

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::Instant;
use heapless::Vec;

/// 支持的最大 peer 数量（对应 `dest_mask: u32` 的 32 个 bit）
pub const MAX_PEERS: usize = 32;

/// `role_tag` 定长字节数（对齐 protocol `AnnounceReply.role_tag: [u8; 3]`）
pub const ROLE_TAG_LEN: usize = 3;

/// MAC-48 长度
pub const MAC_LEN: usize = 6;

/// 未知 RSSI 的哨兵值
pub const RSSI_UNKNOWN: i8 = i8::MIN;

// ============================================================
// PeerInfo —— UI 渲染 / 选择用只读快照
// ============================================================

/// 一个 peer 的完整可见状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PeerInfo {
  /// 逻辑 receiver id（0..MAX_PEERS）；用于 `dest_mask` 位映射
  pub receiver_id: u8,
  /// MAC-48
  pub mac: [u8; MAC_LEN],
  /// 展示用 ASCII 角色标签；未使用位以 `\0` 填充
  pub role: [u8; ROLE_TAG_LEN],
  /// 最近一次接收信号强度（dBm，负数）；[`RSSI_UNKNOWN`] 表示未知
  pub rssi_dbm: i8,
}

impl PeerInfo {
  /// 借用有效的 role 字节切片（去掉尾部 `\0` 填充）
  #[must_use]
  pub fn role_bytes(&self) -> &[u8] {
    let end = self
      .role
      .iter()
      .position(|&b| b == 0)
      .unwrap_or(ROLE_TAG_LEN);
    &self.role[..end]
  }
}

// ============================================================
// upsert 结果
// ============================================================

/// [`PeerRegistry::upsert`] 的返回值
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
  /// MAC 未见过 → 分配了新的 `receiver_id`
  Inserted {
    /// 分配的 receiver_id
    receiver_id: u8,
  },
  /// MAC 已存在 → 只更新了 role / rssi / last_seen
  Updated {
    /// 该 MAC 对应的 receiver_id
    receiver_id: u8,
  },
  /// 已达上限且 MAC 是新的 → 拒绝
  Full,
}

// ============================================================
// 内部条目
// ============================================================

#[derive(Debug, Clone, Copy)]
struct PeerEntry {
  info: PeerInfo,
  last_seen: Instant,
}

// ============================================================
// PeerRegistry
// ============================================================

/// peer 目录
///
/// 内部用 `Mutex<RefCell<..>>` 保护 `heapless::Vec<PeerEntry, MAX_PEERS>`。
pub struct PeerRegistry {
  inner: Mutex<CriticalSectionRawMutex, RefCell<Vec<PeerEntry, MAX_PEERS>>>,
}

impl PeerRegistry {
  /// 构造空 registry
  #[must_use]
  pub const fn new() -> Self {
    Self {
      inner: Mutex::new(RefCell::new(Vec::new())),
    }
  }

  /// 把一个 peer 的 AnnounceReply 记入 registry
  pub fn upsert(
    &self,
    mac: [u8; MAC_LEN],
    role: [u8; ROLE_TAG_LEN],
    rssi_dbm: i8,
    now: Instant,
  ) -> UpsertOutcome {
    self.inner.lock(|cell| {
      let mut peers = cell.borrow_mut();

      // 已存在 → 只更新可变字段
      if let Some(entry) = peers.iter_mut().find(|p| p.info.mac == mac) {
        entry.info.role = role;
        entry.info.rssi_dbm = rssi_dbm;
        entry.last_seen = now;
        return UpsertOutcome::Updated {
          receiver_id: entry.info.receiver_id,
        };
      }

      // 分配新 id：最小可用位
      let Some(new_id) = Self::allocate_id(&peers) else {
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
      // 按 id 升序插入
      let pos = peers
        .iter()
        .position(|p| p.info.receiver_id > new_id)
        .unwrap_or(peers.len());
      if peers.push(entry).is_err() {
        return UpsertOutcome::Full;
      }
      // 冒泡到 pos
      let mut i = peers.len() - 1;
      while i > pos {
        peers.swap(i - 1, i);
        i -= 1;
      }

      UpsertOutcome::Inserted {
        receiver_id: new_id,
      }
    })
  }

  /// 查询 MAC 对应的 `receiver_id`
  #[must_use]
  pub fn lookup_id_for_mac(&self, mac: &[u8; MAC_LEN]) -> Option<u8> {
    self.inner.lock(|cell| {
      cell
        .borrow()
        .iter()
        .find(|p| &p.info.mac == mac)
        .map(|p| p.info.receiver_id)
    })
  }

  /// 反查 `receiver_id` 对应的 MAC（[`lookup_id_for_mac`](Self::lookup_id_for_mac) 的逆）
  ///
  /// # 用途（Phase 2）
  /// 定向单播命令：[`Notifier::send_command_to`](crate::Notifier::send_command_to)
  /// 拿到用户选定的 `receiver_id` 后，反查 MAC 才能构造
  /// [`CommandDest::Unicast`](crate::notifier::CommandDest::Unicast)。
  #[must_use]
  pub fn lookup_mac_for_id(&self, receiver_id: u8) -> Option<[u8; MAC_LEN]> {
    self.inner.lock(|cell| {
      cell
        .borrow()
        .iter()
        .find(|p| p.info.receiver_id == receiver_id)
        .map(|p| p.info.mac)
    })
  }

  /// 只读快照（Copy 后独立，不再持有内部引用）
  #[must_use]
  pub fn snapshot(&self) -> Vec<PeerInfo, MAX_PEERS> {
    self.inner.lock(|cell| {
      let mut out: Vec<PeerInfo, MAX_PEERS> = Vec::new();
      for entry in cell.borrow().iter() {
        // MAX_PEERS 与容量一致；push 不会失败
        let _ = out.push(entry.info);
      }
      out
    })
  }

  /// 当前已注册的 peer 数量
  #[must_use]
  pub fn len(&self) -> usize {
    self.inner.lock(|cell| cell.borrow().len())
  }

  /// 是否为空
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.inner.lock(|cell| cell.borrow().is_empty())
  }

  /// 清空 registry（**仅供测试使用**）
  ///
  /// # 使用限制
  /// 函数名以 `_for_test` 结尾以明确警告：**生产代码不应调用本方法**。
  /// 它的存在是为了让上层测试（比如手柄 `ui::selector` 里对全局 static
  /// registry 的场景验证）能在每个 test 用例前把状态重置到空，避免测试
  /// 之间相互串扰。
  ///
  /// # Feature 门控
  /// 为进一步防止生产代码误调用，本方法用 `#[cfg(any(test, feature = "test-utils"))]`
  /// 门控：
  /// - **本 crate `cargo test`**：`test` cfg 自动生效
  /// - **下游 crate 测试**（如 `controller` 的单测）：需要显式在 `[dev-dependencies]`
  ///   打开 `test-utils` feature，例如 `comm = { path = "..", features = ["test-utils"] }`
  ///
  /// 生产二进制编译时本方法**不存在**，从根本上杜绝误用。
  #[cfg(any(test, feature = "test-utils"))]
  pub fn clear_for_test(&self) {
    self.inner.lock(|cell| cell.borrow_mut().clear());
  }

  /// 分配最小的可用 receiver_id
  fn allocate_id(peers: &Vec<PeerEntry, MAX_PEERS>) -> Option<u8> {
    let mut used_mask: u32 = 0;
    for p in peers.iter() {
      if p.info.receiver_id < 32 {
        used_mask |= 1_u32 << p.info.receiver_id;
      }
    }
    let free_bit = used_mask.trailing_ones();
    if free_bit >= 32 {
      None
    } else {
      Some(free_bit as u8)
    }
  }
}

impl Default for PeerRegistry {
  fn default() -> Self {
    Self::new()
  }
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
    let reg = PeerRegistry::new();
    let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    match reg.upsert(mac, *b"led", -42, now()) {
      UpsertOutcome::Inserted { receiver_id } => assert_eq!(receiver_id, 0),
      other => panic!("expected Inserted, got {:?}", other),
    }
    assert_eq!(reg.len(), 1);
  }

  #[test]
  fn re_upsert_same_mac_returns_updated_and_refreshes_fields() {
    let reg = PeerRegistry::new();
    let mac = [0xAA; 6];
    let _ = reg.upsert(mac, *b"led", -30, now());
    match reg.upsert(mac, *b"srv", -50, now()) {
      UpsertOutcome::Updated { receiver_id } => assert_eq!(receiver_id, 0),
      other => panic!("expected Updated, got {:?}", other),
    }
    let snap = reg.snapshot();
    assert_eq!(snap.len(), 1);
    assert_eq!(snap[0].role_bytes(), b"srv");
    assert_eq!(snap[0].rssi_dbm, -50);
  }

  #[test]
  fn lookup_finds_existing_and_misses_unknown() {
    let reg = PeerRegistry::new();
    let mac = [1, 2, 3, 4, 5, 6];
    let _ = reg.upsert(mac, *b"led", -20, now());
    assert_eq!(reg.lookup_id_for_mac(&mac), Some(0));
    assert_eq!(reg.lookup_id_for_mac(&[9, 9, 9, 9, 9, 9]), None);
  }

  #[test]
  fn reverse_lookup_maps_id_to_mac_and_misses_unknown() {
    let reg = PeerRegistry::new();
    let mac_a = [1, 2, 3, 4, 5, 6];
    let mac_b = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
    let _ = reg.upsert(mac_a, *b"led", -20, now()); // id 0
    let _ = reg.upsert(mac_b, *b"srv", -30, now()); // id 1
    assert_eq!(reg.lookup_mac_for_id(0), Some(mac_a));
    assert_eq!(reg.lookup_mac_for_id(1), Some(mac_b));
    // 未分配的 id → None
    assert_eq!(reg.lookup_mac_for_id(2), None);
    assert_eq!(reg.lookup_mac_for_id(31), None);
  }

  #[test]
  fn reverse_lookup_round_trips_with_forward_lookup() {
    let reg = PeerRegistry::new();
    let mac = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60];
    let UpsertOutcome::Inserted { receiver_id } = reg.upsert(mac, *b"srv", -25, now()) else {
      panic!("expected Inserted");
    };
    assert_eq!(reg.lookup_mac_for_id(receiver_id), Some(mac));
    assert_eq!(reg.lookup_id_for_mac(&mac), Some(receiver_id));
  }

  #[test]
  fn ids_allocated_ascending_and_snapshot_sorted() {
    let reg = PeerRegistry::new();
    for i in 0..5_u8 {
      let _ = reg.upsert([i, 0, 0, 0, 0, 0], *b"aaa", -10, now());
    }
    let snap = reg.snapshot();
    let mut prev = -1_i32;
    for peer in snap.iter() {
      let cur = i32::from(peer.receiver_id);
      assert!(cur > prev);
      prev = cur;
    }
  }

  #[test]
  fn overflow_returns_full() {
    let reg = PeerRegistry::new();
    for i in 0..32_u8 {
      let _ = reg.upsert([i, 0, 0, 0, 0, 0], *b"aaa", -10, now());
    }
    assert_eq!(reg.len(), 32);
    match reg.upsert([99, 0, 0, 0, 0, 0], *b"aaa", -10, now()) {
      UpsertOutcome::Full => {}
      other => panic!("expected Full, got {:?}", other),
    }
  }

  #[test]
  fn role_bytes_trims_trailing_nulls() {
    let peer = PeerInfo {
      receiver_id: 0,
      mac: [0; 6],
      role: [b'l', b'd', 0],
      rssi_dbm: RSSI_UNKNOWN,
    };
    assert_eq!(peer.role_bytes(), b"ld");
  }
}
