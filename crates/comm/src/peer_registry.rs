//! # PeerRegistry —— 已发现 peer 的实例化目录
//!
//! ## 定位
//! `PeerRegistry<N>` 是普通结构体，容量泛型化，支持多实例；
//! 手柄 bin 侧以 `static REGISTRY: PeerRegistry = PeerRegistry::new()`
//! 的形式暴露唯一全局单例（见 `crates/controller/src/lib.rs`）。
//!
//! ## 使用侧
//! - [`Notifier`](crate::Notifier) 在收到 `AnnounceReply` 时 `upsert`
//! - UI / 主循环通过 `snapshot` 拿只读列表渲染
//! - Frame 发送前 `lookup_id_for_mac` 反查 dest_mask
//! - Coordinator 侧可周期 `prune(now, ttl)` 回收长时间未上报的 peer
//!
//! ## 容量与淘汰
//! 容量硬上限 [`MAX_PEERS`]（32，对齐 `dest_mask: u32`）。`last_seen` 是**载荷字段**，
//! 驱动两条淘汰路径：满员时 `upsert` 淘汰最旧 peer（被动兜底），以及 `prune` 主动
//! 清理超龄 peer（TTL）。二者都会释放对应的 `receiver_id` 位供新 peer 复用。
//!
//! ## 并发模型
//! `Mutex<CriticalSectionRawMutex, RefCell<Inner>>`：
//! - `lock()` 只做纯内存操作，无 `.await`
//! - 关中断时间极短（O(N) MAC 比较，N ≤ 32）

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant};
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
  /// registry 已满 → 淘汰 `last_seen` 最旧的 peer 腾出空位后写入新 MAC
  ///
  /// `dest_mask` 位图硬上限为 [`MAX_PEERS`]（32）；一旦填满仍有新 peer 上报，
  /// 与其永久拒绝（旧行为的 [`Full`](Self::Full)），不如淘汰"最久没露面"的那个。
  /// 被淘汰者若之后重新上线，会在下一轮 `Announce` 中作为新 [`Inserted`](Self::Inserted)
  /// 重新入库（分配到刚腾出的 id），自愈成本极低。
  ///
  /// # ⚠️ 短暂的 `receiver_id` 歧义窗口
  /// 被淘汰者腾出的 `evicted_id` **立即**被复用给新 MAC。若被淘汰者**其实仍在线**
  /// （只是恰好 `last_seen` 最旧，例如那段时间没被 solicit），则在它下一次 `Announce`
  /// 重新入库拿到**新** id 之前，会存在一个窗口：**两台物理设备都认为自己是同一个
  /// `receiver_id`** —— 对该 id 的**单播 Frame / 定向命令会同时被两台执行**（`dest_mask`
  /// 寻址歧义）。这是"淘汰复用"与"32 位硬上限"的固有取舍。
  ///
  /// 缓解：调用方（Coordinator）宜在 `Evicted` 后尽快触发一轮 `discover`，让被淘汰者
  /// 重新入库、拿到新 id 关闭该窗口；对**安全/唯一性敏感**的定向命令，应在业务层用
  /// 目标 MAC（而非仅 `receiver_id`）二次确认。
  Evicted {
    /// 新 MAC 分配到的 `receiver_id`（复用被淘汰者腾出的位）
    receiver_id: u8,
    /// 被淘汰的旧 peer 的 `receiver_id`
    evicted_id: u8,
  },
  /// 退化兜底：容量为 0 或内部不变量被破坏时才可能出现
  ///
  /// 正常运行下（[`MAX_PEERS`] > 0）`upsert` 通过淘汰最旧 peer 保证总能腾出空间，
  /// 因此本变体在生产中**不可达**；保留它是为了让"无法写入"这一极端情形有一个
  /// 显式、非 panic 的出口。
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
  ///
  /// # 满员策略（`last_seen` 驱动的淘汰）
  /// 新 MAC 到来且 registry 已满时，淘汰 `last_seen` **最旧**的一条以腾出空位，
  /// 返回 [`UpsertOutcome::Evicted`]（而非旧行为的永久 [`Full`](UpsertOutcome::Full)）。
  /// 这让 `dest_mask` 的 32 个位不会被"曾经出现、早已离线"的 peer 永久占用。
  /// 主动的 TTL 清理见 [`prune`](Self::prune)。
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

      // 新 MAC：尝试分配最小可用位；满员则淘汰 last_seen 最旧的一条再分配
      let mut evicted_id = None;
      let new_id = match Self::allocate_id(&peers) {
        Some(id) => id,
        None => {
          // 找出 last_seen 最旧的条目（相等时取先出现者，稳定）
          let Some(oldest_idx) = peers
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| p.last_seen)
            .map(|(i, _)| i)
          else {
            // 容量为 0 —— 理论不可达（MAX_PEERS > 0）
            return UpsertOutcome::Full;
          };
          evicted_id = Some(peers[oldest_idx].info.receiver_id);
          peers.remove(oldest_idx);
          // 刚腾出一个位，分配必然成功；退化时回退到被淘汰者的 id
          Self::allocate_id(&peers).unwrap_or_else(|| evicted_id.unwrap_or(0))
        }
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
        // 刚腾出过空间，push 不应失败；作为兜底返回 Full
        return UpsertOutcome::Full;
      }
      // 冒泡到 pos
      let mut i = peers.len() - 1;
      while i > pos {
        peers.swap(i - 1, i);
        i -= 1;
      }

      match evicted_id {
        Some(evicted_id) => UpsertOutcome::Evicted {
          receiver_id: new_id,
          evicted_id,
        },
        None => UpsertOutcome::Inserted {
          receiver_id: new_id,
        },
      }
    })
  }

  /// 淘汰所有 `last_seen` 早于 `now - ttl` 的 peer，返回移除数量
  ///
  /// # 用途
  /// 供 Coordinator 侧周期性调用（例如每次 `discover` 前，或独立的低频 task），
  /// 主动回收长时间未再上报的 peer，使其 `receiver_id` / `dest_mask` 位可被
  /// 新 peer 复用。与 [`upsert`](Self::upsert) 的满员淘汰互补：`prune` 是**主动**
  /// 的 TTL 清理，满员淘汰是**被动**的兜底。
  ///
  /// # 时钟边界
  /// 若某条 `last_seen` 竟晚于 `now`（时钟回拨等异常），`checked_duration_since`
  /// 返回 `None`，该条被视为"新鲜"予以保留，绝不误删。
  pub fn prune(&self, now: Instant, ttl: Duration) -> usize {
    self.inner.lock(|cell| {
      let mut peers = cell.borrow_mut();
      let before = peers.len();
      peers.retain(|p| {
        now
          .checked_duration_since(p.last_seen)
          .is_none_or(|age| age <= ttl)
      });
      before - peers.len()
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

  /// 按 `receiver_id` 取单个 peer 的只读快照（[`snapshot`](Self::snapshot) 的轻量单点版）
  ///
  /// # 用途
  /// 逐 id 遍历目录而**不必**把整个 [`snapshot`](Self::snapshot)（最多 [`MAX_PEERS`]
  /// 个 [`PeerInfo`] 的 `Vec`）搬上栈——调用方对 `0..MAX_PEERS` 逐一 `peer_by_id`，
  /// 每次只持有一个小 `PeerInfo`。`receiver_id` 是稳定标识，遍历期间即便有淘汰，
  /// 已淘汰的 id 只是返回 `None`，不会错位。
  #[must_use]
  pub fn peer_by_id(&self, receiver_id: u8) -> Option<PeerInfo> {
    self.inner.lock(|cell| {
      cell
        .borrow()
        .iter()
        .find(|p| p.info.receiver_id == receiver_id)
        .map(|p| p.info)
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
  fn peer_by_id_returns_full_info_and_misses_unknown() {
    let reg = PeerRegistry::new();
    let mac_a = [1, 2, 3, 4, 5, 6];
    let mac_b = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
    let _ = reg.upsert(mac_a, *b"led", -20, now()); // id 0
    let _ = reg.upsert(mac_b, *b"srv", -30, now()); // id 1

    let p0 = reg.peer_by_id(0).expect("id 0 present");
    assert_eq!(p0.receiver_id, 0);
    assert_eq!(p0.mac, mac_a);
    assert_eq!(p0.role, *b"led");
    assert_eq!(p0.rssi_dbm, -20);

    let p1 = reg.peer_by_id(1).expect("id 1 present");
    assert_eq!(p1.mac, mac_b);
    assert_eq!(p1.rssi_dbm, -30);

    // 未分配的 id → None（与 lookup_mac_for_id 语义一致）
    assert_eq!(reg.peer_by_id(2), None);
    assert_eq!(reg.peer_by_id(31), None);
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
  fn overflow_evicts_least_recently_seen() {
    let reg = PeerRegistry::new();
    // 填满 32 个位；id 3 的 peer 用最旧的 last_seen（t=0），其余更新
    for i in 0..32_u8 {
      let t = if i == 3 { 0 } else { 100 + u64::from(i) };
      let _ = reg.upsert([i, 0, 0, 0, 0, 0], *b"aaa", -10, Instant::from_ticks(t));
    }
    assert_eq!(reg.len(), 32);

    // 新 MAC 到来：应淘汰 last_seen 最旧的（id 3），新 peer 复用其腾出的 id
    match reg.upsert([99, 0, 0, 0, 0, 0], *b"new", -10, Instant::from_ticks(999)) {
      UpsertOutcome::Evicted {
        receiver_id,
        evicted_id,
      } => {
        assert_eq!(evicted_id, 3, "应淘汰 last_seen 最旧的 id 3");
        assert_eq!(receiver_id, 3, "新 peer 复用腾出的 id 3");
      }
      other => panic!("expected Evicted, got {:?}", other),
    }
    // 容量不变；旧 MAC(id3) 已被逐出，新 MAC 就位
    assert_eq!(reg.len(), 32);
    assert_eq!(reg.lookup_id_for_mac(&[3, 0, 0, 0, 0, 0]), None);
    assert_eq!(reg.lookup_id_for_mac(&[99, 0, 0, 0, 0, 0]), Some(3));
  }

  #[test]
  fn prune_removes_only_stale_peers() {
    let reg = PeerRegistry::new();
    let _ = reg.upsert([1, 0, 0, 0, 0, 0], *b"old", -10, Instant::from_ticks(0));
    let _ = reg.upsert(
      [2, 0, 0, 0, 0, 0],
      *b"new",
      -10,
      Instant::from_millis(10_000),
    );
    assert_eq!(reg.len(), 2);

    // now=10s, ttl=5s → 只有 t=0 的那条超龄（age=10s > 5s）
    let removed = reg.prune(Instant::from_millis(10_000), Duration::from_secs(5));
    assert_eq!(removed, 1);
    assert_eq!(reg.len(), 1);
    assert_eq!(reg.lookup_id_for_mac(&[1, 0, 0, 0, 0, 0]), None);
    assert_eq!(reg.lookup_id_for_mac(&[2, 0, 0, 0, 0, 0]), Some(1));
  }

  #[test]
  fn prune_frees_id_for_reuse() {
    let reg = PeerRegistry::new();
    let _ = reg.upsert([1, 0, 0, 0, 0, 0], *b"led", -10, Instant::from_ticks(0)); // id 0
    let _ = reg.upsert([2, 0, 0, 0, 0, 0], *b"srv", -10, Instant::from_ticks(0)); // id 1
    reg.prune(Instant::from_millis(1), Duration::from_ticks(0)); // 全部超龄
    assert_eq!(reg.len(), 0);
    // 腾空后新 peer 从最小 id 0 重新分配
    match reg.upsert([9, 0, 0, 0, 0, 0], *b"mot", -10, Instant::from_millis(2)) {
      UpsertOutcome::Inserted { receiver_id } => assert_eq!(receiver_id, 0),
      other => panic!("expected Inserted id 0, got {:?}", other),
    }
  }

  #[test]
  fn prune_keeps_fresh_peers_and_reports_zero() {
    let reg = PeerRegistry::new();
    let _ = reg.upsert(
      [1, 0, 0, 0, 0, 0],
      *b"led",
      -10,
      Instant::from_millis(1_000),
    );
    let removed = reg.prune(Instant::from_millis(1_500), Duration::from_secs(5));
    assert_eq!(removed, 0);
    assert_eq!(reg.len(), 1);
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
