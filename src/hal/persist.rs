//! # 持久化配置层（K4 + P 选项）
//!
//! ## 职责
//! 把手柄的**可变配置**（灵敏度、电池模式、命令抗重放窗口）在断电前
//! 落盘，重启后从存储加载回来，避免每次上电都要重新配置。
//!
//! ## 存储抽象
//! [`PersistentStorage`] trait 屏蔽了具体后端：
//! - [`InMemoryStorage`]：单元测试 & 无 flash 硬件时的 mock（重启丢失）
//! - [`NvsStorage`]：真机 flash 落盘（P 选项）——通过 [`esp_storage::FlashStorage`]
//!   直接操作 NVS 分区，含**双缓冲**保证抗断电损坏
//!
//! 选择哪个后端由 [`crate::config::persist::USE_NVS_STORAGE`] 决定；
//! 切换后端无需修改业务代码（[`crate::transport::control`] 只调 `mark_dirty()`）。
//!
//! ## 数据布局
//! [`PersistentConfig`] 用 `#[repr(C)]` 定长 60 字节（v2，U 选项启用）：
//! 未来若要新增字段，务必在**尾部追加**并升 [`PERSIST_VERSION`]，通过版本
//! 号区分老/新布局做迁移。
//!
//! ```text
//!  offset | size | field
//!  -------+------+------------------------------------------------
//!    0    |  1   | version (= 2)
//!    1    |  1   | battery_simulated (0/1)
//!    2    |  2   | joy_sensitivity  (u16 LE, 0..=1000)
//!    4    |  2   | knob_sensitivity (u16 LE, 0..=1000)
//!    6    |  4   | last_seq (u32 LE)  —— 兼容与日志使用（= replay_windows[0].last_seq）
//!   10    | 48   | replay_windows[KEY_SLOTS]  —— U 选项：4 * 12 字节
//!         |      |   每个 slot: [0..4] last_seq + [4..12] bitmap（LE）
//!   58    |  2   | crc16_ibm(bytes[0..58])
//! ```
//!
//! ## v1 → v2 升级
//! **不兼容式实现**：decode 遇到 v1 直接 `UnsupportedVersion`，回退到
//! [`Default`]。一次固件升级会丢一次配置，可接受。
//!
//! ## 使用节奏（磨损考虑）
//! Flash 有写入寿命（NOR flash ≈ 10^5 次/扇区）。**不能每条命令都写**。
//! 触发点应该是"配置真正改变的关键事件"：
//!
//! | 事件                         | 是否触发 save |
//! |------------------------------|--------------|
//! | `SetSensitivity` 命令        | 是           |
//! | `SetBatteryMode` 命令        | 是           |
//! | `last_seq` 每递增 100 次     | 是（可选）   |
//! | 每帧 (100Hz) 摇杆读值        | **否**       |
//! | LED 状态变化                 | **否**       |
//!
//! ## 后台落盘协作
//! - 命令处理路径（`dispatch_command` 里）仅调用 [`mark_dirty`] 标脏，
//!   不阻塞（不触碰 flash）
//! - [`persist_worker_in_memory_task`] / [`persist_worker_nvs_task`] 每 500 ms
//!   轮询脏位，脏则调用 [`PersistentStorage::save`] 完成实际 IO

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use critical_section::Mutex;

use crate::config::keyring::KEY_SLOTS;
use crate::protocol::crc::crc16_ibm;
use crate::protocol::replay::AntiReplayWindow;

/// 持久化配置协议版本
pub const PERSIST_VERSION: u8 = 2;

/// 持久化数据总长度（bytes）
///
/// v2 布局（U 选项）：version(1) + battery_simulated(1) + joy_sensitivity(2) +
/// knob_sensitivity(2) + last_seq(4) + replay_windows(4 × 12 = 48) + crc(2) = 60
pub const PERSIST_LEN: usize = 60;

// 编译期布局检查
const _: () = assert!(PERSIST_LEN == 1 + 1 + 2 + 2 + 4 + REPLAY_WINDOWS_BYTES + 2);

/// replay_windows 字段总长（bytes）：KEY_SLOTS × AntiReplayWindow::ENCODED_LEN
const REPLAY_WINDOWS_BYTES: usize = KEY_SLOTS * AntiReplayWindow::ENCODED_LEN;

// 各字段偏移（编译期常量）
const OFFSET_VERSION: usize = 0;
const OFFSET_BAT_SIM: usize = 1;
const OFFSET_JOY: usize = 2;
const OFFSET_KNOB: usize = 4;
const OFFSET_LAST_SEQ: usize = 6;
const OFFSET_REPLAY: usize = 10;
const OFFSET_CRC: usize = OFFSET_REPLAY + REPLAY_WINDOWS_BYTES;

/// 持久化配置数据结构
///
/// 所有字段都是 `Copy`，编码/解码采用固定小端字节序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistentConfig {
  /// 摇杆灵敏度（0..=1000）
  pub joy_sensitivity: u16,
  /// 旋钮灵敏度（0..=1000）
  pub knob_sensitivity: u16,
  /// 电池是否走模拟模式（覆盖 config::battery::SIMULATE）
  pub battery_simulated: bool,
  /// Anti-replay 窗口最大已见 seq（兼容字段：与 `replay_windows[0].last_seq()` 同值）
  ///
  /// 保留此字段主要供调试日志快速读取；真正的恢复请看 [`Self::replay_windows`]。
  pub last_seq: u32,
  /// **U 选项**：per-key-id 抗重放窗口快照（KEY_SLOTS 个 slot）
  ///
  /// 每个元素 12 字节（4B last_seq + 8B bitmap）。启动时 [`Self::apply_replay_windows_to_runtime`]
  /// 会把它们回填到 [`crate::transport::control::REPLAY_WINDOWS`]。
  pub replay_windows: [AntiReplayWindow; KEY_SLOTS],
}

impl Default for PersistentConfig {
  fn default() -> Self {
    Self {
      joy_sensitivity: 1000,
      knob_sensitivity: 1000,
      battery_simulated: true,
      last_seq: 0,
      replay_windows: [AntiReplayWindow::new(); KEY_SLOTS],
    }
  }
}

impl PersistentConfig {
  /// 从当前运行时全局状态构造快照
  ///
  /// 调用方通常在"需要落盘"的时机（灵敏度变化、关机等）调用此函数。
  ///
  /// # 包含的快照字段
  /// - 灵敏度 / 电池模式：从 [`crate::transport::control`] 的 Atomic 变量读取
  /// - replay_windows：**入参**传入（因为需要 critical section 读，由调用方集中拿）
  /// - last_seq：从 `replay_windows[0]` 提取（兼容日志）
  pub fn from_runtime(replay_windows: [AntiReplayWindow; KEY_SLOTS]) -> Self {
    use crate::transport::control::{BATTERY_SIMULATED, JOY_SENSITIVITY, KNOB_SENSITIVITY};

    Self {
      joy_sensitivity: JOY_SENSITIVITY.load(Ordering::Relaxed),
      knob_sensitivity: KNOB_SENSITIVITY.load(Ordering::Relaxed),
      battery_simulated: BATTERY_SIMULATED.load(Ordering::Relaxed),
      last_seq: replay_windows[0].last_seq(),
      replay_windows,
    }
  }

  /// 把配置应用回运行时全局状态（启动 load 之后调用一次）
  ///
  /// **注意**：仅恢复灵敏度 / 电池模式。replay_windows 需要单独调用
  /// [`Self::apply_replay_windows_to_runtime`]（因为它需要 critical section，
  /// 与运行时 Atomic 写入隔离）。
  pub fn apply_to_runtime(&self) {
    use crate::transport::control::{
      BATTERY_SIMULATED, JOY_SENSITIVITY, KNOB_SENSITIVITY, SENSITIVITY_MAX,
    };

    JOY_SENSITIVITY.store(self.joy_sensitivity.min(SENSITIVITY_MAX), Ordering::Relaxed);
    KNOB_SENSITIVITY.store(
      self.knob_sensitivity.min(SENSITIVITY_MAX),
      Ordering::Relaxed,
    );
    BATTERY_SIMULATED.store(self.battery_simulated, Ordering::Relaxed);
  }

  /// 把 `replay_windows` 回填到全局 [`crate::transport::control::REPLAY_WINDOWS`]（U 选项）
  ///
  /// # 为什么与 [`Self::apply_to_runtime`] 拆开？
  /// - `apply_to_runtime` 写 Atomic，无需 critical section
  /// - 本方法写 [`crate::transport::control::REPLAY_WINDOWS`]（`Mutex<RefCell<...>>`），
  ///   内部使用 `critical_section::with`；拆开则只在需要时才付出中断屏蔽代价。
  ///
  /// # 预期调用时机
  /// [`crate::bin::main`] 在启动时 `load_or_default` 之后、开始处理 Command 之前。
  pub fn apply_replay_windows_to_runtime(&self) {
    use crate::transport::control::REPLAY_WINDOWS;
    critical_section::with(|cs| {
      for (i, w) in self.replay_windows.iter().enumerate() {
        *REPLAY_WINDOWS[i].borrow_ref_mut(cs) = *w;
      }
    });
  }

  /// 编码为 [`PERSIST_LEN`] 字节数组（含 CRC）
  pub fn encode(&self) -> [u8; PERSIST_LEN] {
    let mut buf = [0_u8; PERSIST_LEN];
    buf[OFFSET_VERSION] = PERSIST_VERSION;
    buf[OFFSET_BAT_SIM] = u8::from(self.battery_simulated);
    buf[OFFSET_JOY..OFFSET_JOY + 2].copy_from_slice(&self.joy_sensitivity.to_le_bytes());
    buf[OFFSET_KNOB..OFFSET_KNOB + 2].copy_from_slice(&self.knob_sensitivity.to_le_bytes());
    buf[OFFSET_LAST_SEQ..OFFSET_LAST_SEQ + 4].copy_from_slice(&self.last_seq.to_le_bytes());
    // replay_windows[KEY_SLOTS] —— 每个 12 字节（M-6：使用 chunks_exact_mut 消除手工索引）
    let replay_bytes = &mut buf[OFFSET_REPLAY..OFFSET_CRC];
    for (chunk, window) in replay_bytes
      .chunks_exact_mut(AntiReplayWindow::ENCODED_LEN)
      .zip(self.replay_windows.iter())
    {
      chunk.copy_from_slice(&window.encode());
    }
    let crc = crc16_ibm(&buf[..OFFSET_CRC]);
    buf[OFFSET_CRC..OFFSET_CRC + 2].copy_from_slice(&crc.to_le_bytes());
    buf
  }

  /// 从字节切片解码
  ///
  /// # Errors
  /// - [`PersistDecodeError::BadLength`]：长度不等于 [`PERSIST_LEN`]
  /// - [`PersistDecodeError::UnsupportedVersion`]：版本号不匹配（例如 v1 旧数据）
  /// - [`PersistDecodeError::BadCrc`]：CRC 校验失败
  pub fn decode(buf: &[u8]) -> Result<Self, PersistDecodeError> {
    if buf.len() != PERSIST_LEN {
      return Err(PersistDecodeError::BadLength);
    }
    let version = buf[OFFSET_VERSION];
    if version != PERSIST_VERSION {
      return Err(PersistDecodeError::UnsupportedVersion(version));
    }
    let expected_crc = crc16_ibm(&buf[..OFFSET_CRC]);
    let actual_crc = u16::from_le_bytes([buf[OFFSET_CRC], buf[OFFSET_CRC + 1]]);
    if expected_crc != actual_crc {
      return Err(PersistDecodeError::BadCrc {
        expected: expected_crc,
        actual: actual_crc,
      });
    }
    // replay_windows[KEY_SLOTS] —— 逐个 decode（无校验，上层 CRC 已保护）
    // M-6：使用 chunks_exact 消除手工索引
    let mut replay_windows = [AntiReplayWindow::new(); KEY_SLOTS];
    let replay_bytes = &buf[OFFSET_REPLAY..OFFSET_CRC];
    for (chunk, window) in replay_bytes
      .chunks_exact(AntiReplayWindow::ENCODED_LEN)
      .zip(replay_windows.iter_mut())
    {
      let mut slice = [0_u8; AntiReplayWindow::ENCODED_LEN];
      slice.copy_from_slice(chunk);
      *window = AntiReplayWindow::decode(&slice);
    }
    Ok(Self {
      battery_simulated: buf[OFFSET_BAT_SIM] != 0,
      joy_sensitivity: u16::from_le_bytes([buf[OFFSET_JOY], buf[OFFSET_JOY + 1]]),
      knob_sensitivity: u16::from_le_bytes([buf[OFFSET_KNOB], buf[OFFSET_KNOB + 1]]),
      last_seq: u32::from_le_bytes([
        buf[OFFSET_LAST_SEQ],
        buf[OFFSET_LAST_SEQ + 1],
        buf[OFFSET_LAST_SEQ + 2],
        buf[OFFSET_LAST_SEQ + 3],
      ]),
      replay_windows,
    })
  }
}

/// 持久化解码失败原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistDecodeError {
  /// 长度不匹配（数据损坏 or 分区未初始化）
  BadLength,
  /// 版本号不支持（需要迁移逻辑）
  UnsupportedVersion(u8),
  /// CRC 校验失败（数据损坏）
  BadCrc { expected: u16, actual: u16 },
}

impl defmt::Format for PersistDecodeError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::BadLength => defmt::write!(f, "PersistDecodeError::BadLength"),
      Self::UnsupportedVersion(v) => {
        defmt::write!(f, "PersistDecodeError::UnsupportedVersion({})", v)
      }
      Self::BadCrc { expected, actual } => defmt::write!(
        f,
        "PersistDecodeError::BadCrc(exp=0x{:04x}, act=0x{:04x})",
        expected,
        actual
      ),
    }
  }
}

// ============================================================
// PersistentStorage —— 抽象接口
// ============================================================

/// 持久化存储通用接口
///
/// 让上层业务代码不关心底层是 Flash / EEPROM / 内存 mock。
pub trait PersistentStorage {
  /// 存储层错误类型（不同后端可能有不同错误）
  type Error;

  /// 读取当前保存的配置（若无有效数据返回 `Ok(None)`）
  fn load(&mut self) -> Result<Option<PersistentConfig>, Self::Error>;

  /// 写入新的配置快照
  fn save(&mut self, config: &PersistentConfig) -> Result<(), Self::Error>;
}

// ============================================================
// InMemoryStorage —— 单元测试 / 无 flash 时使用
// ============================================================

/// 纯内存实现：`save` 写到内部 `Option<[u8; PERSIST_LEN]>`；`load` 读回
///
/// **重启不保留**——这不是 bug 而是特性：真机接入 NVS 之前，先用内存版跑通
/// 主循环 load/save 触发时机的逻辑，避免 flash 分区问题拖延功能开发。
#[derive(Debug, Default)]
pub struct InMemoryStorage {
  raw: Option<[u8; PERSIST_LEN]>,
}

impl InMemoryStorage {
  /// 构造空存储
  pub const fn new() -> Self {
    Self { raw: None }
  }
}

impl PersistentStorage for InMemoryStorage {
  type Error = PersistDecodeError;

  fn load(&mut self) -> Result<Option<PersistentConfig>, Self::Error> {
    match self.raw {
      None => Ok(None),
      Some(bytes) => PersistentConfig::decode(&bytes).map(Some),
    }
  }

  fn save(&mut self, config: &PersistentConfig) -> Result<(), Self::Error> {
    self.raw = Some(config.encode());
    Ok(())
  }
}

// ============================================================
// 便捷全局单例（供 dispatch_command / main 使用）
// ============================================================

/// **是否需要保存**标志（脏位）
///
/// 命令处理侧只需 `mark_dirty()`；后台/低优先级任务在合适时机看到脏位后落盘。
/// 这样避免命令处理路径直接触及 flash（保持中断/critical section 尽可能短）。
static DIRTY: AtomicBool = AtomicBool::new(false);

/// 摇杆灵敏度快照（DIRTY 触发时拿去 encode）
static SNAP_JOY: AtomicU16 = AtomicU16::new(1000);
/// 旋钮灵敏度快照
static SNAP_KNOB: AtomicU16 = AtomicU16::new(1000);
/// 电池模式快照
static SNAP_BAT_SIM: AtomicBool = AtomicBool::new(true);
/// last_seq 快照（与 `SNAP_REPLAY_WINDOWS[0].last_seq()` 一致——冗余但便于日志）
static SNAP_LAST_SEQ: AtomicU32 = AtomicU32::new(0);

/// **U 选项**：per-key-id 抗重放窗口快照（[`KEY_SLOTS`] 个 slot）
///
/// 不使用 Atomic 因为数据太大（KEY_SLOTS × 12 = 48 字节）；用
/// `critical_section::Mutex<RefCell<...>>` 与全局
/// [`crate::transport::control::REPLAY_WINDOWS`] 保持一致风格。
///
/// # 初始值
/// 全 [`AntiReplayWindow::new`]（last_seq=0, bitmap=0）；尚未标脏时也不会被落盘。
static SNAP_REPLAY_WINDOWS: Mutex<RefCell<[AntiReplayWindow; KEY_SLOTS]>> =
  Mutex::new(RefCell::new([AntiReplayWindow::new(); KEY_SLOTS]));

/// 标记"配置已变，等一次落盘"
///
/// 由 [`crate::transport::control::handle_command`] 处理完 SetSensitivity /
/// SetBatteryMode 后调用；后台任务 [`persist_worker_loop`] 会看到脏位后收集
/// 快照并调用 `save`。
///
/// # 参数
/// - `joy` / `knob` / `battery_simulated`：业务状态快照
/// - `replay_windows`：**U 选项** —— 4 个 slot 的窗口快照（由
///   [`crate::transport::control::snapshot_replay_windows`] 归集）
///
/// # 幂等
/// 多次调用只保留一次待落盘状态（最后一次写入赢）。
pub fn mark_dirty(
  joy: u16,
  knob: u16,
  battery_simulated: bool,
  replay_windows: [AntiReplayWindow; KEY_SLOTS],
) {
  SNAP_JOY.store(joy, Ordering::Relaxed);
  SNAP_KNOB.store(knob, Ordering::Relaxed);
  SNAP_BAT_SIM.store(battery_simulated, Ordering::Relaxed);
  SNAP_LAST_SEQ.store(replay_windows[0].last_seq(), Ordering::Relaxed);
  critical_section::with(|cs| {
    *SNAP_REPLAY_WINDOWS.borrow_ref_mut(cs) = replay_windows;
  });
  DIRTY.store(true, Ordering::Release);
}

/// **U 选项**：仅刷新 replay_windows 快照 + 标脏（业务状态保持先前的快照）
///
/// # 为什么需要专门的 replay-only 入口？
/// [`crate::transport::control::dispatch_command`] 每 N 次命令会需要刷新窗口，
/// 但**不应该重写灵敏度/电池模式**（那些需要保持 [`SetSensitivity`] / [`SetBatteryMode`]
/// 命令写入的最新值）。本函数只更新 replay_windows 一项。
///
/// [`SetSensitivity`]: crate::protocol::CommandBody::SetSensitivity
/// [`SetBatteryMode`]: crate::protocol::CommandBody::SetBatteryMode
pub fn mark_replay_dirty(replay_windows: [AntiReplayWindow; KEY_SLOTS]) {
  SNAP_LAST_SEQ.store(replay_windows[0].last_seq(), Ordering::Relaxed);
  critical_section::with(|cs| {
    *SNAP_REPLAY_WINDOWS.borrow_ref_mut(cs) = replay_windows;
  });
  DIRTY.store(true, Ordering::Release);
}

/// 检查并清除脏位；返回本次需要落盘的快照（若无更新返回 `None`）
///
/// 用 `swap` 保证"检查 + 清除"是原子的，避免并发丢失 mark。
pub fn take_dirty_snapshot() -> Option<PersistentConfig> {
  if !DIRTY.swap(false, Ordering::AcqRel) {
    return None;
  }
  // 取 replay_windows 快照（短暂 critical section）
  let replay_windows = critical_section::with(|cs| *SNAP_REPLAY_WINDOWS.borrow_ref(cs));
  Some(PersistentConfig {
    joy_sensitivity: SNAP_JOY.load(Ordering::Relaxed),
    knob_sensitivity: SNAP_KNOB.load(Ordering::Relaxed),
    battery_simulated: SNAP_BAT_SIM.load(Ordering::Relaxed),
    last_seq: SNAP_LAST_SEQ.load(Ordering::Relaxed),
    replay_windows,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn frame_length_is_60() {
    let cfg = PersistentConfig::default();
    let bytes = cfg.encode();
    assert_eq!(bytes.len(), PERSIST_LEN);
    assert_eq!(bytes.len(), 60);
  }

  #[test]
  fn roundtrip_default() {
    let cfg = PersistentConfig::default();
    let bytes = cfg.encode();
    assert_eq!(PersistentConfig::decode(&bytes), Ok(cfg));
  }

  #[test]
  fn roundtrip_custom_values() {
    let cfg = PersistentConfig {
      joy_sensitivity: 750,
      knob_sensitivity: 400,
      battery_simulated: false,
      last_seq: 0xDEAD_BEEF,
      replay_windows: [AntiReplayWindow::new(); KEY_SLOTS],
    };
    let bytes = cfg.encode();
    assert_eq!(PersistentConfig::decode(&bytes), Ok(cfg));
  }

  #[test]
  fn roundtrip_with_populated_replay_windows() {
    // U 选项核心断言：4 个不同的窗口都能完整 roundtrip
    let cfg = PersistentConfig {
      joy_sensitivity: 500,
      knob_sensitivity: 500,
      battery_simulated: true,
      last_seq: 100,
      replay_windows: [
        AntiReplayWindow::from_parts(100, 0x0000_0000_0000_0001),
        AntiReplayWindow::from_parts(200, 0xAAAA_5555_AAAA_5555),
        AntiReplayWindow::from_parts(0, 0),
        AntiReplayWindow::from_parts(u32::MAX, u64::MAX),
      ],
    };
    let bytes = cfg.encode();
    let decoded = PersistentConfig::decode(&bytes).unwrap();
    assert_eq!(decoded, cfg);
    // 确保逐 slot 校验
    for (i, w) in decoded.replay_windows.iter().enumerate() {
      assert_eq!(w, &cfg.replay_windows[i]);
    }
  }

  #[test]
  fn detect_bad_length() {
    let short = [0_u8; 8];
    assert_eq!(
      PersistentConfig::decode(&short),
      Err(PersistDecodeError::BadLength)
    );
  }

  #[test]
  fn detect_unsupported_version() {
    let mut bytes = PersistentConfig::default().encode();
    bytes[OFFSET_VERSION] = 0xFF;
    // 版本号变化 → CRC 也会变；修正 CRC 使得只有版本号错
    let crc = crc16_ibm(&bytes[..OFFSET_CRC]);
    bytes[OFFSET_CRC..OFFSET_CRC + 2].copy_from_slice(&crc.to_le_bytes());
    assert_eq!(
      PersistentConfig::decode(&bytes),
      Err(PersistDecodeError::UnsupportedVersion(0xFF))
    );
  }

  #[test]
  fn detect_v1_rejected() {
    // U 选项：旧 v1 布局（长度 12）不能直接被 v2 接受
    let v1_bytes = [1_u8; 12]; // 长度不一致，应先报 BadLength
    assert_eq!(
      PersistentConfig::decode(&v1_bytes),
      Err(PersistDecodeError::BadLength)
    );
  }

  #[test]
  fn detect_bad_crc() {
    let mut bytes = PersistentConfig::default().encode();
    bytes[5] ^= 0xFF; // 篡改数据
    assert!(matches!(
      PersistentConfig::decode(&bytes),
      Err(PersistDecodeError::BadCrc { .. })
    ));
  }

  #[test]
  fn in_memory_storage_empty_then_saved() {
    let mut storage = InMemoryStorage::new();
    assert_eq!(storage.load().unwrap(), None);

    let cfg = PersistentConfig {
      joy_sensitivity: 500,
      knob_sensitivity: 800,
      battery_simulated: false,
      last_seq: 42,
      replay_windows: [AntiReplayWindow::from_parts(42, 0b1010_1010); KEY_SLOTS],
    };
    storage.save(&cfg).unwrap();
    assert_eq!(storage.load().unwrap(), Some(cfg));
  }

  #[test]
  fn dirty_flag_workflow() {
    // 起初无脏位
    DIRTY.store(false, Ordering::Relaxed);
    // 清空 replay 快照，避免前面测试遗留影响
    critical_section::with(|cs| {
      *SNAP_REPLAY_WINDOWS.borrow_ref_mut(cs) = [AntiReplayWindow::new(); KEY_SLOTS];
    });
    assert!(take_dirty_snapshot().is_none());

    // 标脏 + 取回
    let windows = [AntiReplayWindow::from_parts(100, 0x1); KEY_SLOTS];
    mark_dirty(800, 900, false, windows);
    let snap = take_dirty_snapshot().expect("dirty snapshot present");
    assert_eq!(snap.joy_sensitivity, 800);
    assert_eq!(snap.knob_sensitivity, 900);
    assert!(!snap.battery_simulated);
    assert_eq!(snap.last_seq, 100);
    assert_eq!(snap.replay_windows[0].last_seq(), 100);

    // 再次 take 应为 None（一次消费）
    assert!(take_dirty_snapshot().is_none());
  }

  #[test]
  fn mark_replay_dirty_preserves_business_fields() {
    // U 选项：mark_replay_dirty 只刷 replay 字段，不抹去灵敏度/电池模式先前写入的值
    // 先写一次完整的 dirty
    let initial_windows = [AntiReplayWindow::from_parts(50, 0x3); KEY_SLOTS];
    mark_dirty(600, 700, true, initial_windows);
    // 再仅刷 replay（不需重写业务字段）
    let updated_windows = [AntiReplayWindow::from_parts(999, 0xF); KEY_SLOTS];
    mark_replay_dirty(updated_windows);
    let snap = take_dirty_snapshot().expect("still dirty after replay update");
    // 业务字段保留
    assert_eq!(snap.joy_sensitivity, 600);
    assert_eq!(snap.knob_sensitivity, 700);
    assert!(snap.battery_simulated);
    // replay 字段已刷新
    assert_eq!(snap.last_seq, 999);
    assert_eq!(snap.replay_windows[0].last_seq(), 999);
  }
}

// ============================================================
// 后台落盘任务
// ============================================================

/// 落盘轮询周期（毫秒）
///
/// 500ms 是一个"用户操作后感知不到延迟 + 命令风暴时不会浪费 flash 寿命"
/// 的折中值。极端场景下（Host 每秒发一次 SetSensitivity）也只会写一次。
const PERSIST_POLL_INTERVAL_MS: u64 = 500;

/// 便捷入口：启动时载入已保存配置，若无则回退到 [`PersistentConfig::default`]
///
/// 此函数**不 panic**：所有 storage 层错误都会被 warn 出来并回退到默认值 ——
/// "找不到配置"应该是一个可恢复的启动路径，而不是硬崩溃。
pub fn load_or_default<S>(storage: &mut S) -> PersistentConfig
where
  S: PersistentStorage,
  S::Error: defmt::Format,
{
  match storage.load() {
    Ok(Some(cfg)) => {
      defmt::info!(
        "[PERSIST] loaded config: joy={} knob={} bat_sim={} last_seq={}",
        cfg.joy_sensitivity,
        cfg.knob_sensitivity,
        cfg.battery_simulated,
        cfg.last_seq
      );
      cfg
    }
    Ok(None) => {
      defmt::info!("[PERSIST] no saved config, using defaults");
      PersistentConfig::default()
    }
    Err(e) => {
      defmt::warn!("[PERSIST] load failed: {}; using defaults", e);
      PersistentConfig::default()
    }
  }
}

/// 后台落盘 worker：定期检查脏位，脏则调用 [`PersistentStorage::save`]
///
/// # 泛型参数
/// `S`：具体存储后端。embassy `#[task]` 要求单态化，因此本函数不直接是
/// `#[embassy_executor::task]`；调用方（`main.rs`）需要**为具体类型**声明
/// 自己的 task 函数——本函数只提供**逻辑**给 task 复用。
///
/// # 使用范例（in main.rs）
/// ```ignore
/// #[embassy_executor::task]
/// async fn persist_worker(storage: &'static Mutex<CriticalSectionRawMutex, InMemoryStorage>) -> ! {
///     persist_worker_loop(storage).await
/// }
/// ```
///
/// 由于我们当前只有 `InMemoryStorage`（不需要 async），这里给一个直接可 spawn
/// 的具体 task 版本，见 [`persist_worker_in_memory_task`]。
pub async fn persist_worker_loop<S>(storage: &mut S) -> !
where
  S: PersistentStorage,
  S::Error: defmt::Format,
{
  use embassy_time::{Duration, Timer};
  loop {
    Timer::after(Duration::from_millis(PERSIST_POLL_INTERVAL_MS)).await;
    if let Some(snapshot) = take_dirty_snapshot() {
      match storage.save(&snapshot) {
        Ok(()) => defmt::info!(
          "[PERSIST] saved: joy={} knob={} bat_sim={} seq={}",
          snapshot.joy_sensitivity,
          snapshot.knob_sensitivity,
          snapshot.battery_simulated,
          snapshot.last_seq
        ),
        Err(e) => defmt::warn!("[PERSIST] save failed: {}", e),
      }
    }
  }
}

/// 具体化的 InMemoryStorage 后台落盘任务
///
/// 单态化后 embassy `#[task]` 才能生成 spawn token。
///
/// # 参数
/// - `storage`：`'static` mut ref，通常从 `StaticCell` 拿出
#[embassy_executor::task]
pub async fn persist_worker_in_memory_task(storage: &'static mut InMemoryStorage) -> ! {
  defmt::info!("[PERSIST] worker started (InMemoryStorage)");
  persist_worker_loop(storage).await
}

// ============================================================
// NvsStorage —— 真机 flash NVS 落盘（P 选项）
// ============================================================

use embedded_storage::Storage;
use esp_hal::peripherals::FLASH;
use esp_storage::{FlashStorage, FlashStorageError};

use crate::config::persist::{SLOT_A_OFFSET, SLOT_B_OFFSET};

/// [`NvsStorage`] 的错误变体（P 选项）
///
/// 包装 [`FlashStorageError`] 并加入我们自己的语义层错误（例如
/// [`PersistentConfig::decode`] 失败）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvsError {
  /// 底层 flash 读/写/擦除失败
  Flash(FlashStorageError),
}

impl From<FlashStorageError> for NvsError {
  fn from(e: FlashStorageError) -> Self {
    Self::Flash(e)
  }
}

impl defmt::Format for NvsError {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      // FlashStorageError 已实现 defmt::Format，透传即可
      Self::Flash(e) => defmt::write!(f, "NvsError::Flash({})", e),
    }
  }
}

/// 真机 NVS 落盘存储实现（P 选项）
///
/// # 双缓冲策略
/// 使用两个 4KB slot（A / B）交替写入：
///
/// ```text
///  slot A (SLOT_A_OFFSET .. +4KB)  ┐
///  slot B (SLOT_B_OFFSET .. +4KB)  ┴─ 交替写入
/// ```
///
/// - **写入**：擦除+写入一个 slot（`Storage::write` 自动做 erase-then-write）
///   完成后翻转 [`Self::next_slot`]。任何时刻只有**一个** slot 处于"正在写"
///   状态；另一个 slot 保留上次成功写入的完整数据。
/// - **读取**：两个 slot 都尝试 decode，取 `last_seq` 更大且 CRC 通过的那份。
///   即使写入过程中断电导致一份损坏，另一份仍可完整恢复。
///
/// # Flash 磨损
/// NOR flash 通常提供 10⁵ 次擦写寿命；配合 [`persist_worker_loop`] 的 500ms
/// 轮询 + 脏位机制，最坏情况下每 500ms 擦写一次 → 单个 slot 寿命 ≈ 14 小时。
/// 双 slot 交替把总寿命翻倍到 ≈ 28 小时的**连续满负荷**。
/// 实际使用中脏位仅在灵敏度 / 电池模式变更时触发（低频事件），寿命远超
/// 手柄硬件本身。
///
/// # 与 ESP-IDF NVS 库的关系
/// 目前直接读写 flash NVS 分区（0x9000..0xE000）的前 8KB；**不使用**
/// ESP-IDF 的键值 NVS 库。若未来引入官方库，需要迁移到自定义 partition。
pub struct NvsStorage {
  /// 底层 flash 抽象
  storage: FlashStorage<'static>,
  /// 下一次 `save` 使用的 slot 索引（0 = A, 1 = B）
  ///
  /// [`Self::load`] 会根据两个 slot 的新旧关系反推此值：读到较新的 slot 后，
  /// 下一次写入应该写到**较旧的** slot（避免覆盖较新的备份）。
  next_slot: u8,
}

impl NvsStorage {
  /// 构造新的 [`NvsStorage`] 实例
  ///
  /// # 参数
  /// - `flash`：从 [`esp_hal::peripherals`] 派发而来的 `FLASH` peripheral
  ///   句柄；未来的所有 flash 操作都通过它中介。peripheral 是独享的（无
  ///   `Copy`），全局只能存在一份 [`NvsStorage`] 实例。
  ///
  /// 内部创建 [`FlashStorage`] 包装。**不做**任何 flash IO，因此调用便宜；
  /// 实际读写发生在 [`PersistentStorage::load`] / [`PersistentStorage::save`] 中。
  #[must_use]
  pub fn new(flash: FLASH<'static>) -> Self {
    let storage = FlashStorage::new(flash);
    Self {
      storage,
      // 默认从 slot B 写（这样第一次 load 读 A + B 都拿到 None 时，
      // 下面 save 会写 B → 下一次改成写 A，交替开始）
      next_slot: 1,
    }
  }

  /// 尝试读取指定 slot 的 [`PersistentConfig`]
  ///
  /// # Errors
  /// [`NvsError::Flash`]：flash 读取本身失败（硬件错误）
  ///
  /// # 返回
  /// - `Ok(Some(cfg))`：slot 里有有效配置
  /// - `Ok(None)`：slot 里数据无效（未初始化 / CRC 错误 / 版本不支持）——
  ///   属于**预期情况**，不应升级为错误
  fn load_slot(&mut self, slot: u8) -> Result<Option<PersistentConfig>, NvsError> {
    let offset = if slot == 0 {
      SLOT_A_OFFSET
    } else {
      SLOT_B_OFFSET
    };
    let mut buf = [0_u8; PERSIST_LEN];
    // Storage::read 提供 embedded-storage 的通用接口（以及 ReadStorage 自动包括）
    embedded_storage::ReadStorage::read(&mut self.storage, offset, &mut buf)?;
    // decode 失败视为"该 slot 无有效数据"（未初始化 flash 内容通常全 0xFF）
    Ok(PersistentConfig::decode(&buf).ok())
  }
}

impl PersistentStorage for NvsStorage {
  type Error = NvsError;

  /// 从 flash 双缓冲加载最新配置
  ///
  /// 策略：
  /// 1. 读取 slot A 与 slot B
  /// 2. 两个都有效时选 `last_seq` 更大的那份，并把 `next_slot` 指向**较旧**的
  /// 3. 只有一个有效时选它，`next_slot` 指向另一份（下次写入不覆盖有效数据）
  /// 4. 都无效时返回 `Ok(None)`，`next_slot` 保留默认值
  fn load(&mut self) -> Result<Option<PersistentConfig>, Self::Error> {
    let slot_a = self.load_slot(0)?;
    let slot_b = self.load_slot(1)?;

    match (slot_a, slot_b) {
      (Some(a), Some(b)) => {
        // 都有效：取较新的那份，把下一次写入指向较旧的（保护较新的副本）
        if a.last_seq >= b.last_seq {
          self.next_slot = 1;
          Ok(Some(a))
        } else {
          self.next_slot = 0;
          Ok(Some(b))
        }
      }
      (Some(a), None) => {
        // 只有 A 有效：下一次写入 B
        self.next_slot = 1;
        Ok(Some(a))
      }
      (None, Some(b)) => {
        // 只有 B 有效：下一次写入 A
        self.next_slot = 0;
        Ok(Some(b))
      }
      (None, None) => Ok(None),
    }
  }

  /// 把配置写入下一个 slot（自动交替）
  ///
  /// # 副作用
  /// - flash 擦除 + 写入（约 30ms 阻塞；本函数不是 async）
  /// - 翻转 [`Self::next_slot`]（成功后）
  ///
  /// # Errors
  /// [`NvsError::Flash`]：底层 flash 擦除或写入失败
  ///
  /// # 中断安全
  /// esp-storage 默认开启 `critical-section` feature，擦写期间禁用中断，
  /// 无需调用方额外加锁。
  fn save(&mut self, config: &PersistentConfig) -> Result<(), Self::Error> {
    let bytes = config.encode();
    let offset = if self.next_slot == 0 {
      SLOT_A_OFFSET
    } else {
      SLOT_B_OFFSET
    };
    // Storage::write 自动做 erase-then-write（内部会读整个 sector → 修改
    // 目标字节 → erase → 写回）
    self.storage.write(offset, &bytes)?;
    // M-5 观测（NOR flash 磨损）：每次成功写入递增全局计数器，dashboard 可拉取。
    // 每次擦写循环 ~30ms 且直接消耗 NOR flash 寿命额度，长期频率必须监控。
    crate::metrics::record_flash_write();
    // 翻转 slot（0 ↔ 1）
    self.next_slot ^= 1;
    Ok(())
  }
}

/// 具体化的 [`NvsStorage`] 后台落盘任务
///
/// 与 [`persist_worker_in_memory_task`] 功能相同，但底层是 flash 写入。
///
/// # 参数
/// - `storage`：`'static` mut ref，通常从 [`static_cell::StaticCell`] 拿出
#[embassy_executor::task]
pub async fn persist_worker_nvs_task(storage: &'static mut NvsStorage) -> ! {
  defmt::info!("[PERSIST] worker started (NvsStorage: flash NVS partition)");
  persist_worker_loop(storage).await
}
