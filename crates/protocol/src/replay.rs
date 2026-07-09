//! # 抗重放窗口（Anti-Replay Sliding Window，K2 选项）
//!
//! ## 职责
//! 抵挡"抓包 + 重发"攻击 —— 即使攻击者拿到一条**完整合法** Command 帧
//! （通过了 CRC + HMAC 校验），也无法通过重发达成命令重复执行。
//!
//! ## 算法：64-bit 滑动位图窗口
//! IPsec / OpenVPN 使用的经典方案：
//! ```text
//!  bit 0  bit 1  bit 2  ...  bit 63
//!   ▲                          ▲
//!   │ last_seq                 │ last_seq - 63
//!   │ (最新已见)                │ (窗口最旧)
//!   
//!  bit N = 1 表示 "last_seq - N" 这个 seq 已经见过
//! ```
//!
//! ## 收到新 seq 时的分支
//! | 条件                       | 动作                                             |
//! |----------------------------|--------------------------------------------------|
//! | `seq == 0`                 | 拒绝（0 保留给"未初始化"哨兵值）                  |
//! | `seq > last_seq`           | 接受；窗口左移 `(seq - last_seq)`；bit 0 = 1     |
//! | `last_seq - 63 <= seq <= last_seq` | 查 bit；若已 set 则拒绝，否则 set 后接受 |
//! | `seq < last_seq - 63`      | 拒绝（太旧）                                     |
//!
//! ## 为什么允许"回看 63 步"？
//! 现实链路（Wi-Fi / BLE）会有乱序：seq 100 可能在 seq 99 之前到达。
//! 严格要求 seq > last_seq 会误伤合法乱序帧；64 位窗口足够容忍 100Hz 命令流
//! 短暂 0.6s 的乱序抖动，同时**每个 seq 只能被接受一次**保证防重放。
//!
//! ## 状态持久化（U 选项）
//! 启用 [`AntiReplayWindow::encode`] / [`AntiReplayWindow::decode`] 后，手柄重启
//! 仍能从 [`crate::hal::persist::PersistentConfig`] 恢复完整的 `(last_seq, bitmap)` ——
//! 避免“掰手柄电→重发旧抓包”的偷袭。
//!
//! 落盘节奏：不能每条命令都写 flash（磨损）；参见 [`crate::config::persist`] 中
//! 的预留门限（默认每 100 递增才会触发一次 “replay-only” 的 dirty）。

/// Anti-replay 校验失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayError {
  /// seq == 0 —— 0 保留给"未初始化"，合法 seq 从 1 开始
  InvalidSeq,
  /// seq 已经在窗口内出现过，拒绝重放
  AlreadySeen,
  /// seq 比窗口最旧位置还小，无法判断是否重放 —— 保守拒绝
  TooOld,
}

#[cfg(feature = "defmt")]
impl defmt::Format for ReplayError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::InvalidSeq => defmt::write!(f, "ReplayError::InvalidSeq"),
      Self::AlreadySeen => defmt::write!(f, "ReplayError::AlreadySeen"),
      Self::TooOld => defmt::write!(f, "ReplayError::TooOld"),
    }
  }
}

/// 滑动位图窗口宽度（bits）—— 决定容忍多大的乱序
pub const WINDOW_WIDTH: u32 = 64;

/// 抗重放滑动窗口
///
/// # 字段
/// - `last_seq`：见过的最大 seq
/// - `bitmap`：最近 64 个 seq 的状态；bit N (N ∈ 0..64) 对应 seq = `last_seq - N`
///
/// # 初始状态
/// `last_seq = 0, bitmap = 0` —— 任意 `seq >= 1` 都能通过检查
///
/// # 序列化（U 选项）
/// [`Self::encode`] / [`Self::decode`] 以 [`Self::ENCODED_LEN`] = 12 字节広播
/// `last_seq (u32 LE) || bitmap (u64 LE)`，供 [`crate::hal::persist`] 落盘。
/// **decode 无需校验**：`u32 + u64` 的任何位串都属于合法内部状态（高位 bitmap
/// = 0 代表“近期无 seq”，不会引入系统性错误）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AntiReplayWindow {
  last_seq: u32,
  bitmap: u64,
}
impl AntiReplayWindow {
  /// 持久化字节长度（U 选项）：4B last_seq + 8B bitmap
  pub const ENCODED_LEN: usize = 12;

  /// 构造空窗口（初始 `last_seq = 0`）
  pub const fn new() -> Self {
    Self {
      last_seq: 0,
      bitmap: 0,
    }
  }

  /// 当前窗口内已确认的最大 seq（0 = 尚未见过任何 seq）
  pub const fn last_seq(&self) -> u32 {
    self.last_seq
  }

  /// 当前位图快照（仅供持久化与调试使用）
  ///
  /// # 为什么不直接 `pub` 字段？
  /// - `bitmap` 与 `last_seq` 之间存在不变量（bit 0 总对应 `last_seq`），
  ///   外部直接写入会破坏不变量
  /// - 只暴露 getter + [`Self::from_parts`] 保持“只读快照 / 带校验重建”的能力
  pub const fn bitmap(&self) -> u64 {
    self.bitmap
  }

  /// 从持久化快照重建窗口（U 选项）
  ///
  /// # 参数
  /// - `last_seq`：之前落盘的最大 seq（0 表示“未见过任何 seq”）
  /// - `bitmap`：相对于 `last_seq` 的位图
  ///
  /// # 无效输入的处理
  /// 任何 `u32 + u64` 都属于合法内部状态（包括 `last_seq=0`、`bitmap=0`、
  /// `bitmap=u64::MAX` 等边界情况），因此不需要 `Result` 返回类型。
  /// 依靠 [`Self::decode`] 上层的 CRC 保护去判断“字节是否未损坏”。
  pub const fn from_parts(last_seq: u32, bitmap: u64) -> Self {
    Self { last_seq, bitmap }
  }

  /// 序列化为 [`Self::ENCODED_LEN`] 字节数组（U 选项：LE）
  ///
  /// # 布局
  /// ```text
  ///   [0..4]   last_seq  (u32 LE)
  ///   [4..12]  bitmap    (u64 LE)
  /// ```
  pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
    let mut buf = [0_u8; Self::ENCODED_LEN];
    buf[0..4].copy_from_slice(&self.last_seq.to_le_bytes());
    buf[4..12].copy_from_slice(&self.bitmap.to_le_bytes());
    buf
  }

  /// 从 [`Self::ENCODED_LEN`] 字节数组反序列化（U 选项）
  ///
  /// 不返回 `Result`：参见 [`Self::from_parts`] 的说明，任何字节串都合法。
  /// 上层写入 flash 时包围 CRC（见 [`crate::hal::persist::PersistentConfig`]），
  /// “字节是否被损坏” 已在那一层拦住。
  #[must_use]
  pub fn decode(bytes: &[u8; Self::ENCODED_LEN]) -> Self {
    let last_seq = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let bitmap = u64::from_le_bytes([
      bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9], bytes[10], bytes[11],
    ]);
    Self { last_seq, bitmap }
  }

  /// 校验 `seq` 是否可以被接受，并**就地更新**窗口状态
  ///
  /// # 参数
  /// - `seq`：来自远端 Command 帧的 32-bit 序列号（>= 1）
  ///
  /// # 返回
  /// - `Ok(())`：该 seq 未曾见过，已计入窗口（后续同 seq 会被拒绝）
  /// - `Err(...)`：拒绝的具体原因，见 [`ReplayError`]
  ///
  /// # 副作用
  /// 仅当返回 `Ok(())` 时才修改内部状态；`Err` 时窗口保持不变 ——
  /// 保证恶意帧无法"污染"合法窗口。
  pub fn check_and_update(&mut self, seq: u32) -> Result<(), ReplayError> {
    // 0 保留：某些库把 0 当作"未初始化"，我们与之对齐避免误判
    if seq == 0 {
      return Err(ReplayError::InvalidSeq);
    }

    if seq > self.last_seq {
      // 新记录 —— 窗口整体左移；旧位状态废弃
      let shift = seq - self.last_seq;
      if shift >= WINDOW_WIDTH {
        // 差距太大，直接把窗口重置为"只有当前这一位"
        self.bitmap = 1;
      } else {
        // 左移 shift 位；然后把 bit 0（表示 last_seq 本身）置 1
        self.bitmap = (self.bitmap << shift) | 1;
      }
      self.last_seq = seq;
      return Ok(());
    }

    // seq <= last_seq —— 需要判断"是否在窗口内 + 是否已见过"
    let age = self.last_seq - seq;
    if age >= WINDOW_WIDTH {
      return Err(ReplayError::TooOld);
    }
    let mask = 1_u64 << age;
    if self.bitmap & mask != 0 {
      return Err(ReplayError::AlreadySeen);
    }
    // 窗口内未见过：接受并 set 对应 bit
    self.bitmap |= mask;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rejects_zero_seq() {
    let mut w = AntiReplayWindow::new();
    assert_eq!(w.check_and_update(0), Err(ReplayError::InvalidSeq));
  }

  #[test]
  fn accepts_monotonic_sequence() {
    let mut w = AntiReplayWindow::new();
    for seq in 1..=100 {
      assert_eq!(w.check_and_update(seq), Ok(()));
    }
    assert_eq!(w.last_seq(), 100);
  }

  #[test]
  fn rejects_exact_replay() {
    let mut w = AntiReplayWindow::new();
    assert_eq!(w.check_and_update(5), Ok(()));
    assert_eq!(w.check_and_update(5), Err(ReplayError::AlreadySeen));
  }

  #[test]
  fn accepts_out_of_order_within_window() {
    let mut w = AntiReplayWindow::new();
    // 先接受 seq=10
    assert_eq!(w.check_and_update(10), Ok(()));
    // 乱序到达 seq=8, 9, 5 —— 都在窗口内（10-8=2, 10-9=1, 10-5=5），且未见过
    assert_eq!(w.check_and_update(8), Ok(()));
    assert_eq!(w.check_and_update(9), Ok(()));
    assert_eq!(w.check_and_update(5), Ok(()));
    // 再收 seq=8 应被拒绝
    assert_eq!(w.check_and_update(8), Err(ReplayError::AlreadySeen));
  }

  #[test]
  fn rejects_too_old() {
    let mut w = AntiReplayWindow::new();
    assert_eq!(w.check_and_update(100), Ok(()));
    // 100 - 63 = 37 是窗口最旧；seq=36 应被拒绝
    assert_eq!(w.check_and_update(36), Err(ReplayError::TooOld));
    // seq=37 应被接受（正好在窗口边缘）
    assert_eq!(w.check_and_update(37), Ok(()));
  }

  #[test]
  fn large_gap_resets_bitmap() {
    let mut w = AntiReplayWindow::new();
    assert_eq!(w.check_and_update(1), Ok(()));
    // 跳跃到 seq=1000（远大于窗口宽度）
    assert_eq!(w.check_and_update(1000), Ok(()));
    // 现在 seq=1 已经不在窗口内（1000 - 1 >= 64），应被拒绝为 TooOld
    assert_eq!(w.check_and_update(1), Err(ReplayError::TooOld));
    // 但 seq=1000 仍已见过
    assert_eq!(w.check_and_update(1000), Err(ReplayError::AlreadySeen));
  }

  #[test]
  fn window_edge_seq() {
    let mut w = AntiReplayWindow::new();
    assert_eq!(w.check_and_update(64), Ok(()));
    // last_seq=64, 窗口覆盖 seq=1..=64；seq=1 应恰好在窗口边缘（64-1=63 < 64）
    assert_eq!(w.check_and_update(1), Ok(()));
    // seq=1 再来应被拒绝
    assert_eq!(w.check_and_update(1), Err(ReplayError::AlreadySeen));
  }

  #[test]
  fn err_does_not_mutate_state() {
    let mut w = AntiReplayWindow::new();
    assert_eq!(w.check_and_update(100), Ok(()));
    let last_before = w.last_seq();
    // 一次 replay 拒绝应保持 last_seq 不变
    let _ = w.check_and_update(100);
    assert_eq!(w.last_seq(), last_before);
    // 一次 too-old 拒绝也不应改变 last_seq
    let _ = w.check_and_update(1);
    assert_eq!(w.last_seq(), last_before);
  }

  // ---- U 选项：持久化序列化相关测试 ----

  #[test]
  fn encoded_len_is_12() {
    let w = AntiReplayWindow::new();
    let bytes = w.encode();
    assert_eq!(bytes.len(), AntiReplayWindow::ENCODED_LEN);
    assert_eq!(bytes.len(), 12);
  }

  #[test]
  fn encode_default_all_zero() {
    let w = AntiReplayWindow::default();
    let bytes = w.encode();
    assert_eq!(bytes, [0_u8; 12]);
  }

  #[test]
  fn roundtrip_after_activity() {
    let mut w = AntiReplayWindow::new();
    // 构造一个非平凡的 bitmap：接收 100 后乱序接收 95, 90
    assert_eq!(w.check_and_update(100), Ok(()));
    assert_eq!(w.check_and_update(95), Ok(()));
    assert_eq!(w.check_and_update(90), Ok(()));
    let before = w;
    let bytes = w.encode();
    let restored = AntiReplayWindow::decode(&bytes);
    assert_eq!(restored, before);
    assert_eq!(restored.last_seq(), 100);
    // 重新回放 100 / 95 / 90 应均拒绝（已见过）
    let mut w2 = restored;
    assert_eq!(w2.check_and_update(100), Err(ReplayError::AlreadySeen));
    assert_eq!(w2.check_and_update(95), Err(ReplayError::AlreadySeen));
    assert_eq!(w2.check_and_update(90), Err(ReplayError::AlreadySeen));
    // 与此同时新 seq 仍可接受
    assert_eq!(w2.check_and_update(101), Ok(()));
  }

  #[test]
  fn from_parts_matches_bitmap_getter() {
    let w = AntiReplayWindow::from_parts(0x1234_5678, 0xDEAD_BEEF_CAFE_BABE);
    assert_eq!(w.last_seq(), 0x1234_5678);
    assert_eq!(w.bitmap(), 0xDEAD_BEEF_CAFE_BABE);
  }

  #[test]
  fn decode_accepts_any_bytes() {
    // 任何 12 字节都应能 decode 成功（上层 CRC 已保障完整性）
    let arbitrary = [0xFF_u8; 12];
    let w = AntiReplayWindow::decode(&arbitrary);
    assert_eq!(w.last_seq(), u32::MAX);
    assert_eq!(w.bitmap(), u64::MAX);
  }

  #[test]
  fn encode_endianness_le() {
    // last_seq = 1 → [1, 0, 0, 0]；bitmap = 1 → [1, 0, 0, 0, 0, 0, 0, 0]
    let w = AntiReplayWindow::from_parts(1, 1);
    let bytes = w.encode();
    assert_eq!(&bytes[0..4], &[1, 0, 0, 0]);
    assert_eq!(&bytes[4..12], &[1, 0, 0, 0, 0, 0, 0, 0]);
  }
}
