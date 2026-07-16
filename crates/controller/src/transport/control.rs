//! # Control 命令处理 —— Host → 手柄 的反向控制
//!
//! ## 本模块的职责
//! 1. **持有手柄侧运行时可调参数**：灵敏度 / 电池模拟开关 / AUTO_ACK / 抗重放窗口
//! 2. **两条命令通路的分派入口**：
//!    - [`dispatch_command_from_ble`]：BLE GATT Write 收到原始字节后调用
//!    - [`dispatch_command_from_esp_now`]：ESP-NOW 通道由 [`comm::notifier::run_receive_loop`]
//!      在完成 decode + 抗重放 + AnnounceReply 分派后回调
//! 3. **命令执行核心** [`execute_command`]：两条通路共用；根据 [`CommandBody`] 分派到
//!    对应副作用（LED / Toast / 灵敏度 / 电池模式 / Nop），返回 `Result<(), ErrorCode>`
//! 4. **BLE 侧 Ack 广播** [`broadcast_response`]：把 Ack 写入 BLE + ESP-NOW 双 Signal
//!
//! ## 两条通路的分工
//! ```text
//!  BLE Write ──► dispatch_command_from_ble(raw)
//!                 ├─ decode_command
//!                 ├─ REPLAY.check
//!                 ├─ maybe_persist_replay
//!                 ├─ execute_command
//!                 └─ AUTO_ACK ? broadcast_response(BLE + ESP-NOW 双写)
//!
//!  ESP-NOW ─► [comm 内部完成 decode / replay / AnnounceReply upsert]
//!             └─► dispatch_command_from_esp_now(src, &cmd)
//!                  ├─ execute_command
//!                  ├─ AUTO_ACK ? ble_hid::signal_response(BLE 补一发 Ack)
//!                  └─ 返回 CommandOutcome → comm 自动发 ESP-NOW Ack
//! ```
//!
//! **共享的仅是** [`execute_command`] + 静态状态（REPLAY / AUTO_ACK / 灵敏度…）。
//! 解码 + 抗重放的**语义**由 [`comm::ReplayGuard`] 提供，本模块只是它的消费者；
//! 关于 per-key-id 抗重放窗口的实现细节请参阅 comm 侧文档。

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};

use comm::ReplayGuard;
pub use comm::CommandSource;
use defmt::{info, warn};

use crate::config::keyring::KEY_SLOTS;
use crate::hal::led_effects::signal_led_effect;
use crate::protocol::{
  AntiReplayWindow, Command, CommandBody, CommandDecodeError, CommandResponse, ErrorCode, KeyId,
  decode_command,
};
use crate::transport::ble_hid;
use crate::transport::esp_now;
use crate::ui::{signal_toast, touch_host_heartbeat};

// ============================================================
// 区 1：运行时全局状态
// ============================================================

/// 摇杆灵敏度（0..=1000 定点数，1000 = 100%）
pub static JOY_SENSITIVITY: AtomicU16 = AtomicU16::new(DEFAULT_SENSITIVITY);
/// 旋钮灵敏度（0..=1000 定点数）
pub static KNOB_SENSITIVITY: AtomicU16 = AtomicU16::new(DEFAULT_SENSITIVITY);
/// 电池是否走模拟模式（`true` = 模拟递减；覆盖 `config::battery::SIMULATE`）
pub static BATTERY_SIMULATED: AtomicBool = AtomicBool::new(true);
/// 是否在完成每条命令后自动回 Ack（默认开启；调试时可通过命令关闭）
pub static AUTO_ACK: AtomicBool = AtomicBool::new(true);

/// 灵敏度默认值 —— 收到不合法命令时回退到这个值
pub const DEFAULT_SENSITIVITY: u16 = 1000;
/// 灵敏度最大值（同时用作定点分母）
pub const SENSITIVITY_MAX: u16 = 1000;

/// **U 选项**：抗重放窗口落盘触发间隔
///
/// [`dispatch_command_from_ble`] 每 N 条命令 anti-replay 通过后触发一次
/// [`crate::hal::persist::mark_replay_dirty`]，避免高频命令磨损 flash。
pub const REPLAY_PERSIST_INTERVAL: u32 = 100;

/// 抗重放滑动窗口（K2 + O 选项：per-key-id）
///
/// 手柄侧全局单例，供 BLE / ESP-NOW 两条命令入口共享 —— 攻击者从任一路抓到的 seq
/// 都会立刻被对方入口拒收。内部实现见 [`comm::ReplayGuard`]。
///
/// 上电时 [`crate::hal::persist::PersistentConfig::apply_replay_windows_to_runtime`]
/// 会用 flash 里的旧快照覆盖本实例，从掉电点继续拒绝重放。
pub static REPLAY: ReplayGuard = ReplayGuard::new();

/// **M-2**：上次触发 replay-only 落盘时该 slot 已见的最大 seq
///
/// per-key-id 记录"上次因 replay-window 变化触发 flash 写入时的 last_seq"。
/// 只有 `current_last_seq - LAST_PERSISTED_LAST_SEQ >= REPLAY_PERSIST_INTERVAL`
/// 才再次触发写入，防止：
///
/// 1. 恶意攻击者构造 `seq = 100/200/300...` 的合法命令加速 NOR flash 磨损
/// 2. seq 因 anti-replay 位图乱序回落导致 `seq % 100 == 0` 反复触发
static LAST_PERSISTED_LAST_SEQ: [AtomicU32; KEY_SLOTS] = [const { AtomicU32::new(0) }; KEY_SLOTS];

// ============================================================
// 区 2：状态访问器
// ============================================================

/// 设置摇杆灵敏度（自动 clamp 到 0..=1000）
#[inline]
pub fn set_joy_sensitivity(scale: u16) {
  JOY_SENSITIVITY.store(scale.min(SENSITIVITY_MAX), Ordering::Relaxed);
}

/// 设置旋钮灵敏度（自动 clamp 到 0..=1000）
#[inline]
pub fn set_knob_sensitivity(scale: u16) {
  KNOB_SENSITIVITY.store(scale.min(SENSITIVITY_MAX), Ordering::Relaxed);
}

/// 读取摇杆灵敏度快照
#[inline]
#[must_use]
pub fn joy_sensitivity() -> u16 {
  JOY_SENSITIVITY.load(Ordering::Relaxed)
}

/// 读取旋钮灵敏度快照
#[inline]
#[must_use]
pub fn knob_sensitivity() -> u16 {
  KNOB_SENSITIVITY.load(Ordering::Relaxed)
}

/// 一次取出全部 [`KEY_SLOTS`] 个抗重放窗口快照，供持久化子系统落盘
///
/// 在单一 critical section 内取全部 slot，避免"取 slot 0 与取 slot 1 之间发生写入"
/// 导致的快照不一致。
#[must_use]
pub fn snapshot_replay_windows() -> [AntiReplayWindow; KEY_SLOTS] {
  REPLAY.snapshot()
}

// ============================================================
// 区 3：BLE 通道命令入口
// ============================================================

/// **BLE 通道**的命令分派入口
///
/// BLE GATT Write 事件收到原始字节后调用此函数即可，无需自己解码。
///
/// # 处理流程
/// 1. `decode_command`：长度 / magic / version / CRC / HMAC 全部校验
/// 2. `REPLAY.check`：per-key-id 抗重放窗口校验
/// 3. `maybe_persist_replay`：M-2 每 [`REPLAY_PERSIST_INTERVAL`] 触发一次 flash 落盘
/// 4. `execute_command`：执行业务动作
/// 5. `AUTO_ACK` 若开启：`broadcast_response` 双写 BLE + ESP-NOW 通道
///
/// # 静默/告警策略
/// - `BadMagic` / `BadLength`：静默忽略（BLE Prepare Write 等）
/// - `AuthFailed` / `ReplayError` / 其它 decode error：`warn!`（可能是攻击）
pub fn dispatch_command_from_ble(raw: &[u8]) {
  let cmd = match decode_command(raw) {
    Ok(c) => c,
    Err(CommandDecodeError::BadMagic | CommandDecodeError::BadLength) => return,
    Err(e) => {
      warn!("[CTRL/BLE] decode error: {}", e);
      return;
    }
  };

  if let Err(e) = REPLAY.check(cmd.key_id, cmd.seq) {
    warn!(
      "[CTRL/BLE] replay rejected: kid={} seq={} reason={}",
      cmd.key_id, cmd.seq, e
    );
    return;
  }
  maybe_persist_replay(cmd.key_id);

  touch_host_heartbeat();
  let result = execute_command(CommandSource::Ble, &cmd);

  if AUTO_ACK.load(Ordering::Relaxed) {
    broadcast_response(build_response(&cmd, result));
  }
}

// ============================================================
// 区 4：ESP-NOW 通道命令入口（comm handler）
// ============================================================

/// **ESP-NOW 通道**的命令分派入口 —— 挂给
/// [`comm::notifier::CommandHandlerConfig::handler`]
///
/// # 前置保证（由 [`comm::notifier::run_receive_loop`] 完成）
/// - `decode_command` 已通过（合法 magic / version / CRC / HMAC）
/// - `REPLAY.check` 已通过（comm 内部使用同一个 [`REPLAY`] 实例）
/// - `Announce` / `AssignId` 在 comm 上游已被拦截并自动处理，不会到达此函数
///
/// # 与 BLE 侧的差异
/// - **不做 decode / replay**：comm 已完成
/// - **不做 flash 落盘**：只有 BLE 通道触发 `maybe_persist_replay`；
///   ESP-NOW 通道的 replay 推进会在下一次 BLE 命令到达时随之落盘
/// - **Ack 分工**：BLE Ack 由本函数手写 `ble_hid::signal_response(...)`；
///   ESP-NOW Ack 由本函数返回 [`CommOutcome`] 让 comm 自动广播
///
/// [`CommOutcome`]: comm::CommandOutcome
pub fn dispatch_command_from_esp_now(src: CommandSource, cmd: &Command) -> comm::CommandOutcome {
  touch_host_heartbeat();
  let result = execute_command(src, cmd);

  if !AUTO_ACK.load(Ordering::Relaxed) {
    return comm::CommandOutcome::NoReply;
  }

  // BLE 通道：手写 —— comm 管不到 BLE
  ble_hid::signal_response(build_response(cmd, result));

  // ESP-NOW 通道：返回 Outcome，让 comm 自动往 RESP_SIG 塞 Ack
  match result {
    Ok(()) => comm::CommandOutcome::Ok,
    Err(code) => comm::CommandOutcome::Err(code),
  }
}

// ============================================================
// 区 5：两条通路共用的核心
// ============================================================

/// 已通过校验的命令的业务分派中枢（两条通路共用）
///
/// **不允许长阻塞**：BLE / comm task 栈上直接调用；LED / Toast 等耗时动作
/// 通过 Signal 交给专门的 task 处理。
///
/// # 返回
/// - `Ok(())`：执行成功；调用方回 Ack
/// - `Err(code)`：参数不合法或不支持；调用方回 Err(code)
fn execute_command(src: CommandSource, cmd: &Command) -> Result<(), ErrorCode> {
  match cmd.kind {
    CommandBody::Nop => {
      info!("[CTRL] Nop (heartbeat) seq={} from {}", cmd.seq, src);
      Ok(())
    }
    CommandBody::LedBlink {
      led_idx,
      count,
      period_ms,
    } => execute_led_blink(src, cmd.seq, led_idx, count, period_ms),
    CommandBody::SetSensitivity {
      joy_scale,
      knob_scale,
    } => execute_set_sensitivity(src, cmd.seq, joy_scale, knob_scale),
    CommandBody::ShowToast { len, bytes } => execute_show_toast(src, cmd.seq, len, &bytes),
    CommandBody::SetBatteryMode { simulate } => execute_set_battery_mode(src, cmd.seq, simulate),
    // Announce / AssignId 是"手柄 → 接收方"方向：controller 是发送方，收到即异常
    CommandBody::Announce => {
      info!(
        "[CTRL] Announce received on controller from {} (ignored, controller is not a receiver)",
        src
      );
      Err(ErrorCode::Unsupported)
    }
    CommandBody::AssignId { .. } => {
      info!(
        "[CTRL] AssignId received on controller from {} (ignored, controller is not a receiver)",
        src
      );
      Err(ErrorCode::Unsupported)
    }
  }
}

/// `LedBlink` 执行体：参数校验 + 副作用信号
fn execute_led_blink(
  src: CommandSource,
  seq: u32,
  led_idx: u8,
  count: u8,
  period_ms: u16,
) -> Result<(), ErrorCode> {
  if led_idx > 1 {
    warn!("[CTRL] LedBlink invalid led_idx={} from {}", led_idx, src);
    return Err(ErrorCode::InvalidArgument);
  }
  if count == 0 || period_ms < 20 {
    return Err(ErrorCode::InvalidArgument);
  }
  info!(
    "[CTRL] LedBlink seq={} from {}: led={} count={} period={}ms",
    seq, src, led_idx, count, period_ms
  );
  signal_led_effect(led_idx, count, period_ms);
  Ok(())
}

/// `SetSensitivity` 执行体：clamp + 更新 + 落盘
fn execute_set_sensitivity(
  src: CommandSource,
  seq: u32,
  joy_scale: u16,
  knob_scale: u16,
) -> Result<(), ErrorCode> {
  let joy_clamped = joy_scale.min(SENSITIVITY_MAX);
  let knob_clamped = knob_scale.min(SENSITIVITY_MAX);
  set_joy_sensitivity(joy_clamped);
  set_knob_sensitivity(knob_clamped);
  info!(
    "[CTRL] SetSensitivity seq={} from {}: joy={} knob={}",
    seq, src, joy_clamped, knob_clamped
  );
  mark_persist_dirty();
  Ok(())
}

/// `ShowToast` 执行体：截长 + 发信号
fn execute_show_toast(src: CommandSource, seq: u32, len: u8, bytes: &[u8; 5]) -> Result<(), ErrorCode> {
  let len = len.min(5);
  let msg = &bytes[..len as usize];
  signal_toast(msg);
  info!(
    "[CTRL] ShowToast seq={} from {}: len={} bytes={:?}",
    seq, src, len, msg
  );
  Ok(())
}

/// `SetBatteryMode` 执行体：更新 + 落盘
fn execute_set_battery_mode(src: CommandSource, seq: u32, simulate: bool) -> Result<(), ErrorCode> {
  BATTERY_SIMULATED.store(simulate, Ordering::Relaxed);
  info!(
    "[CTRL] SetBatteryMode seq={} from {}: simulate={}",
    seq, src, simulate
  );
  mark_persist_dirty();
  Ok(())
}

/// 从 execute 结果构造 Ack / Err Response（O 选项：`key_id` 直接取自 Command）
#[inline]
fn build_response(cmd: &Command, result: Result<(), ErrorCode>) -> CommandResponse {
  match result {
    Ok(()) => CommandResponse::ack_with_key(cmd.seq, cmd.key_id),
    Err(code) => CommandResponse::err_with_key(cmd.seq, cmd.key_id, code),
  }
}

/// M-2：判断当前 key_id 的 replay 窗口是否已推进足够多 seq，若是则触发 flash 落盘
///
/// 采用 CAS 保证并发下同一进度只会触发一次 `mark_replay_dirty`。
fn maybe_persist_replay(key_id: KeyId) {
  let slot = key_id.as_u8() as usize;
  debug_assert!(slot < KEY_SLOTS, "key_id must be within KEY_SLOTS");

  let current_last_seq = REPLAY.last_seq(key_id);
  let last_persisted = LAST_PERSISTED_LAST_SEQ[slot].load(Ordering::Relaxed);
  if current_last_seq.wrapping_sub(last_persisted) < REPLAY_PERSIST_INTERVAL {
    return;
  }

  // CAS：只有把 last_persisted 推进到 current_last_seq 的那个调用者才真正触发写 flash；
  // 竞态失败说明另一路径已在写，直接放弃即可（幂等）。
  if LAST_PERSISTED_LAST_SEQ[slot]
    .compare_exchange(
      last_persisted,
      current_last_seq,
      Ordering::Relaxed,
      Ordering::Relaxed,
    )
    .is_ok()
  {
    crate::hal::persist::mark_replay_dirty(snapshot_replay_windows());
  }
}

/// 把当前灵敏度 / 电池模式 / replay_windows 快照到持久化脏缓冲区
///
/// 触发时机：执行完 `SetSensitivity` / `SetBatteryMode` 后。
fn mark_persist_dirty() {
  crate::hal::persist::mark_dirty(
    JOY_SENSITIVITY.load(Ordering::Relaxed),
    KNOB_SENSITIVITY.load(Ordering::Relaxed),
    BATTERY_SIMULATED.load(Ordering::Relaxed),
    snapshot_replay_windows(),
  );
}

// ============================================================
// 区 6：Ack 双写广播（BLE 通路使用）
// ============================================================

/// 双链路广播一条 [`CommandResponse`]（N 选项）
///
/// 同时写入 ESP-NOW 与 BLE 两个 Signal —— 两边链路彼此解耦，任一链路断开时
/// 另一链路仍能正常送达。
///
/// # 使用者
/// - [`dispatch_command_from_ble`]：BLE 侧 AUTO_ACK 通路
/// - 未来需要"主动通知 Host"的场景（如电池低电告警）也应走此入口
///
/// # 覆盖观测
/// 直接写 [`esp_now::RESP_SIG`] 前先检查 `signaled()` 判断是否发生覆盖，
/// 递增 [`crate::metrics::record_response_overwrite`]。
pub fn broadcast_response(resp: CommandResponse) {
  if esp_now::RESP_SIG.signaled() {
    crate::metrics::record_response_overwrite();
  }
  esp_now::RESP_SIG.signal(resp);
  ble_hid::signal_response(resp);
}
