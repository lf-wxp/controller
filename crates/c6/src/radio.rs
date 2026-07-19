//! ESP-NOW 接收 / 广播（**基于 [`comm`] 门面**）
//!
//! 本模块承担 receiver 侧的空口全部工作，实现下沉给 [`comm::Receiver`] +
//! [`comm::notifier::run_receive_loop`] + [`comm::notifier::run_broadcast_loop`]：
//!
//! - **Frame（`0xC71E`）**：comm 解码 + CRC + `dest_mask` 过滤后回调 [`on_frame`]，
//!   更新 [`VM_WATCH`] 里的 [`ViewModel`]。
//! - **Command（`0xCB01`）**：`Announce` / `AssignId` 由 comm 自动响应；其它 kind
//!   （LedBlink / ShowToast / ...）都是发给手柄本身的，[`on_command`] 静默返回
//!   [`CommandOutcome::NoReply`]。
//! - **抗重放 / HMAC / keyring / receiver_id 分配**：全部由 comm 内部完成。
//!
//! ## 为什么 handler 是 `fn` 指针？
//! comm 的 [`CommandHandler`] / [`FrameHandler`] 都是 `fn(...)`（不是 `Fn`），
//! 不能捕获环境；因此本模块**用一组 `static`** 装载共享状态（[`KEYRING`] /
//! [`REPLAY`] / [`MY_ID`] / [`VM_WATCH`]），handler 内部 `use` 它们即可。
//! `Watch::new()` 与 `Signal::new()` 都是 `const fn`，直接放进 `static` 无需
//! [`static_cell::StaticCell`] 惰性初始化。
//!
//! ## 两个后台 task
//! ```text
//!  ┌──────────────────────────┐   ┌────────────────────────────┐
//!  │ recv_loop_task           │   │ broadcast_loop_task        │
//!  │  EspNowRecvLink          │   │  EspNowSendLink            │
//!  │   ↓ recv_async           │   │   ↑ send_async             │
//!  │  comm::run_receive_loop  │   │  comm::run_broadcast_loop  │
//!  │   ↓ dispatch             │   │   ↓ select3                │
//!  │  on_frame / on_command   │   │  FRAME_SIG / CMD / RESP    │
//!  └──────────────────────────┘   └────────────────────────────┘
//! ```
//!
//! [`CommandHandler`]: comm::receiver::CommandHandler
//! [`FrameHandler`]: comm::receiver::FrameHandler

use core::sync::atomic::AtomicU8;

use comm::notifier::signals::{CommandOutSignal, FrameSignal, ResponseSignal};
use comm::receiver::{CommandHandler, FrameHandler, run_broadcast_loop, run_receive_loop};
use comm::{Command, CommandOutcome, CommandSource, Frame, Keyring, ReplayGuard};
use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::watch::Watch;

use crate::display::ViewModel;
use crate::link::{EspNowRecvLink, EspNowSendLink};
use crate::peer::{INITIAL_RECEIVER_ID, ROLE_TAG};

// ============================================================
// 对外类型
// ============================================================

/// 允许订阅的最大 consumer 数（仅一个渲染 task，1 足矣）。
pub const WATCH_CONSUMERS: usize = 1;

/// 全局共享的 [`ViewModel`] 通道：`on_frame` 写入，render loop 订阅。
pub type StateWatch = Watch<CriticalSectionRawMutex, ViewModel, WATCH_CONSUMERS>;

// ============================================================
// static 全局状态（供 `fn` handler 访问）
// ============================================================

/// keyring：管理 KeyId + tx_counter；comm 使用它验签 / 生成 seq。
static KEYRING: Keyring = Keyring::new();

/// per-key-id 抗重放窗；comm 内部 `dispatch_packet` 每收到合法 Command 即
/// `check_and_update`，验证失败静默丢弃。
static REPLAY: ReplayGuard = ReplayGuard::new();

/// 本机 `receiver_id`：占位 [`INITIAL_RECEIVER_ID`]（= `comm` 的 `UNASSIGNED_ID`
/// / `u8::MAX`）；comm 收到匹配 mac 的 `AssignId` 会自动 `.store(new_id, Relaxed)`
/// 覆写成 `0..=31` 的合法 id。用 `u8::MAX` 而非 `0` 作占位，见 [`INITIAL_RECEIVER_ID`] 文档。
static MY_ID: AtomicU8 = AtomicU8::new(INITIAL_RECEIVER_ID);

/// Frame 出站 Signal（本 receiver 不主动 send_frame，但 comm builder 强制填）。
static FRAME_SIG: FrameSignal = FrameSignal::new();

/// Command 出站 Signal（本 receiver 不主动发 Command）。
static CMD_SIG: CommandOutSignal = CommandOutSignal::new();

/// Response 出站 Signal（AnnounceReply / Ack 通过它推给 broadcast loop）。
static RESP_SIG: ResponseSignal = ResponseSignal::new();

/// 状态视图广播：`Watch::new()` 是 `const fn`，可直接 `static`。
///
/// - 写者：[`on_frame`]
/// - 读者：`main` 里的渲染 loop，通过 `VM_WATCH.receiver()` 订阅
pub static VM_WATCH: StateWatch = Watch::new();

// ============================================================
// handlers（fn 指针 —— 不能捕获环境）
// ============================================================

/// Frame handler：comm 在 dispatch 侧已完成解码 / CRC / `dest_mask` 过滤，
/// 只有"发给本机（或广播命中）"的 Frame 才会进来。
///
/// 更新累计计数（`ok_count` / `gap_count`）与最新 `state`，然后通过 [`VM_WATCH`]
/// 广播给渲染 task。
fn on_frame(_src: CommandSource, frame: &Frame) {
  let sender = VM_WATCH.sender();
  // 用 try_get 拿最近一次 vm 做增量更新；若 Watch 里还没值就从空态开始
  let mut vm = sender.try_get().unwrap_or_else(ViewModel::empty);

  if vm.have_data {
    let expected = vm.last_seq.wrapping_add(1);
    if frame.header.seq != expected {
      vm.gap_count = vm.gap_count.saturating_add(1);
    }
  }
  vm.have_data = true;
  vm.last_seq = frame.header.seq;
  vm.ok_count = vm.ok_count.saturating_add(1);
  vm.state = frame.payload;
  vm.receiver_id = MY_ID.load(core::sync::atomic::Ordering::Relaxed);
  vm.assigned = vm.receiver_id != INITIAL_RECEIVER_ID;
  sender.send(vm);
}

/// Command handler：`Announce` / `AssignId` 已被 comm 自动处理（回
/// AnnounceReply / 写 `MY_ID`），到达这里的都是"发给手柄本身"的控制命令
/// （LedBlink / ShowToast / SetSensitivity / ...）—— receiver 不消费，
/// 直接 [`CommandOutcome::NoReply`]。
fn on_command(_src: CommandSource, _cmd: &Command) -> CommandOutcome {
  CommandOutcome::NoReply
}

// ============================================================
// 后台 task
// ============================================================

/// 接收 loop：从 [`EspNowRecvLink`] 一直 recv，comm 内部完成 dispatch。
#[embassy_executor::task]
async fn recv_loop_task(link: EspNowRecvLink, my_mac: [u8; 6]) -> ! {
  run_receive_loop(
    link,
    &KEYRING,
    &REPLAY,
    &RESP_SIG,
    ROLE_TAG,
    my_mac,
    &MY_ID,
    on_command as CommandHandler,
    CommandSource::EspNow,
    Some(on_frame as FrameHandler),
  )
  .await
}

/// 广播 loop：three-way select FRAME/CMD/RESP，任一有信号即通过
/// [`EspNowSendLink`] 广播出去。
#[embassy_executor::task]
async fn broadcast_loop_task(link: EspNowSendLink) -> ! {
  run_broadcast_loop(link, &FRAME_SIG, &CMD_SIG, &RESP_SIG).await
}

// ============================================================
// 对外入口
// ============================================================

/// 启动 comm receiver：spawn 两个 embassy task。
///
/// # 参数
/// - `spawner`：embassy 执行器
/// - `my_mac`：本机 MAC-48（`AnnounceReply.mac` + `AssignId.mac == my_mac` 用）
/// - `recv_link` / `send_link`：`esp_now.split()` 拆出的两半
///
/// # 返回
/// - `Ok(&VM_WATCH)`：把 [`VM_WATCH`] 引用返回，供渲染 loop `.receiver()`
/// - `Err(spawn error)`：spawn 失败（一般是同名 task 重复 spawn）
///
/// # 使用
/// ```ignore
/// let watch = c6::radio::start(&spawner, own_mac, recv_link, send_link)
///     .expect("radio start");
/// let mut rx = watch.receiver().expect("watch receiver");
/// ```
pub fn start(
  spawner: &Spawner,
  my_mac: [u8; 6],
  recv_link: EspNowRecvLink,
  send_link: EspNowSendLink,
) -> Result<&'static StateWatch, embassy_executor::SpawnError> {
  info!(
    "esp-now (comm-backed) starting, own_mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
    my_mac[0], my_mac[1], my_mac[2], my_mac[3], my_mac[4], my_mac[5]
  );
  // embassy-executor 0.10: `Spawner::spawn` 返回 `()`；task 生成器本身返回
  // `Result<SpawnToken, SpawnError>`，因此 `?` 加在 task 调用上
  spawner.spawn(recv_loop_task(recv_link, my_mac)?);
  spawner.spawn(broadcast_loop_task(send_link)?);
  Ok(&VM_WATCH)
}
