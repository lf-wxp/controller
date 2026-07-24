//! # comm-hello-world
//!
//! `comm` 的 **"引入即用"** 端到端演示。无需任何真实硬件：
//! 借助 `comm` 的 `loopback` feature 在同一进程里
//! 同时扮演 **Notifier**（手柄侧）和 **Receiver**（LED 侧），
//! 走完完整的通信编排流程。
//!
//! ## 幕次
//!
//! ```text
//!  幕1  Notifier.discover()  ─ Announce ─▶  Receiver 自动回 AnnounceReply
//!         ▲                                  │
//!         └──────── AssignId 自动下发 ◀──────┘
//!
//!  幕2  Notifier.send_command(LedBlink) ─▶  Receiver handler 触发
//!                                            │
//!                             自动回 Ack ◀───┘
//!
//!  幕3  Notifier.send_frame(GamepadState) ─▶ 通过 loopback 广播出去
//!         (演示"发送编排通道"已就绪；接收端主要消费 Command，
//!          Frame 只在 controller ↔ receiver 高频状态场景使用)
//! ```
//!
//! ## 运行
//!
//! ```bash
//! cd crates/examples/comm-hello-world
//! cargo run
//! ```
//!
//! 或从项目根：
//!
//! ```bash
//! cargo run -p comm-hello-world
//! ```

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use comm::loopback::{LoopbackRecvEnd, LoopbackSendEnd, pair};
use comm::notifier::signals::{CommandOutChannel, FrameSignal, ResponseChannel};
use comm::{
  Command, CommandBody, CommandOutcome, CommandSource, ErrorCode, Frame, GamepadState, Keyring,
  Notifier, PeerRegistry, Receiver, ReplayGuard, Selector,
};
use futures_executor::LocalPool;
use futures_util::task::LocalSpawnExt;

// ============================================================
// 基本参数（模拟两台设备的 MAC / role）
// ============================================================

const MAC_NOTIFIER: [u8; 6] = [0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0x01];
const MAC_RECEIVER: [u8; 6] = [0xBB, 0xBB, 0xBB, 0xBB, 0xBB, 0x02];
const RECEIVER_ROLE: [u8; 3] = *b"led";

// ============================================================
// 全局观测计数器（handler 内部记录被调用次数）
// ============================================================

static HANDLER_INVOCATIONS: AtomicU32 = AtomicU32::new(0);
static HANDLER_LAST_LED_COUNT: AtomicU32 = AtomicU32::new(0);

/// 用户业务 handler：只处理 LedBlink，其它一律返回 Unsupported
///
/// 参数 `_src` 表示命令来源（Ble/EspNow/Local）；本 demo 是 loopback link，
/// 由调用方传入 [`CommandSource::EspNow`] 作为默认来源，业务侧不区分。
fn handle_command(_src: CommandSource, cmd: &Command) -> CommandOutcome {
  match cmd.kind {
    CommandBody::LedBlink { count, .. } => {
      HANDLER_LAST_LED_COUNT.store(u32::from(count), Ordering::Relaxed);
      HANDLER_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
      CommandOutcome::Ok
    }
    _ => CommandOutcome::Err(ErrorCode::Unsupported),
  }
}

// ============================================================
// 双端 'static 状态：靠 Box::leak 一次性分配（生产环境请换 static_cell）
// ============================================================

struct NotifierState {
  keyring: &'static Keyring,
  peers: &'static PeerRegistry,
  replay: &'static ReplayGuard,
  selector: &'static Selector,
  frame_sig: &'static FrameSignal,
  cmd_sig: &'static CommandOutChannel,
  resp_sig: &'static ResponseChannel,
}

impl NotifierState {
  fn leak() -> Self {
    Self {
      keyring: Box::leak(Box::new(Keyring::new())),
      peers: Box::leak(Box::new(PeerRegistry::new())),
      replay: Box::leak(Box::new(ReplayGuard::new())),
      selector: Box::leak(Box::new(Selector::broadcast_all())),
      frame_sig: Box::leak(Box::new(FrameSignal::new())),
      cmd_sig: Box::leak(Box::new(CommandOutChannel::new())),
      resp_sig: Box::leak(Box::new(ResponseChannel::new())),
    }
  }
}

struct ReceiverState {
  keyring: &'static Keyring,
  replay: &'static ReplayGuard,
  resp_sig: &'static ResponseChannel,
  frame_sig: &'static FrameSignal,
  cmd_sig: &'static CommandOutChannel,
  my_id: &'static AtomicU8,
}

impl ReceiverState {
  fn leak() -> Self {
    Self {
      keyring: Box::leak(Box::new(Keyring::new())),
      replay: Box::leak(Box::new(ReplayGuard::new())),
      resp_sig: Box::leak(Box::new(ResponseChannel::new())),
      frame_sig: Box::leak(Box::new(FrameSignal::new())),
      cmd_sig: Box::leak(Box::new(CommandOutChannel::new())),
      my_id: Box::leak(Box::new(AtomicU8::new(u8::MAX))),
    }
  }
}

// ============================================================
// spawn 4 个后台 loop：双端各自的 broadcast / recv
//
// 全部走**门面自带的 run 方法**——门面 link 无关，两条 loop 各吃一个 link 端，
// 直接消费门面内部的 &'static 字段，调用方无需再手抄一堆参数。
// ============================================================

fn spawn_all_loops(
  pool: &LocalPool,
  notifier: &'static Notifier,
  receiver: &'static Receiver,
  a_send: LoopbackSendEnd,
  a_recv: LoopbackRecvEnd,
  b_send: LoopbackSendEnd,
  b_recv: LoopbackRecvEnd,
) {
  let spawner = pool.spawner();

  // ---- Notifier 端：持有 peers → Frame 单目标时自动单播 ----
  spawner
    .spawn_local(async move { notifier.run_broadcast_loop(a_send).await })
    .expect("spawn notifier broadcast_loop");
  spawner
    .spawn_local(async move { notifier.run_receive_loop(a_recv).await })
    .expect("spawn notifier recv_loop");

  // ---- Receiver 端：无 peers → Frame 恒广播 ----
  spawner
    .spawn_local(async move { receiver.run_broadcast_loop(b_send).await })
    .expect("spawn receiver broadcast_loop");
  spawner
    .spawn_local(async move { receiver.run_receive_loop(b_recv).await })
    .expect("spawn receiver recv_loop");
}

// ============================================================
// 演示辅助：反复 yield 直到条件满足
// ============================================================

async fn wait_for(mut cond: impl FnMut() -> bool, max_iters: usize, label: &'static str) {
  for _ in 0..max_iters {
    if cond() {
      return;
    }
    embassy_futures::yield_now().await;
  }
  panic!("condition `{label}` not met within {max_iters} yields");
}

// ============================================================
// main
// ============================================================

fn main() {
  println!("╔═══════════════════════════════════════════════════════════════╗");
  println!("║  comm-hello-world — comm 引入即用体验演示          ║");
  println!("╚═══════════════════════════════════════════════════════════════╝");
  println!();

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_NOTIFIER, MAC_RECEIVER);

  // ────────────────────────────────────────────────────────────────
  // 门面：build 一次，`&'static` 化后**既跑后台 loop 又当生产者句柄**
  //
  // 门面 link 无关（不含 link 字段）；keyring / peers / signals 等 &'static 组件
  // 都由 build 收进门面。之后：
  // - 后台 loop：`notifier.run_broadcast_loop(send)` / `run_receive_loop(recv)`
  // - 主循环生产：`notifier.discover()` / `send_command()` / `send_frame()`
  // ────────────────────────────────────────────────────────────────
  let notifier: &'static Notifier = Box::leak(Box::new(
    Notifier::builder()
      .keyring(ns.keyring)
      .peers(ns.peers)
      .replay(ns.replay)
      .selector(ns.selector)
      .frame_signal(ns.frame_sig)
      .command_signal(ns.cmd_sig)
      .response_signal(ns.resp_sig)
      .build(),
  ));
  let receiver: &'static Receiver = Box::leak(Box::new(
    Receiver::builder()
      .keyring(rs.keyring)
      .replay(rs.replay)
      .response_signal(rs.resp_sig)
      .frame_signal(rs.frame_sig)
      .command_signal(rs.cmd_sig)
      .role_tag(RECEIVER_ROLE)
      .mac(MAC_RECEIVER)
      .my_id(rs.my_id)
      .command_handler(handle_command)
      .build(),
  ));
  println!("✓ Notifier / Receiver 门面组装完毕（link 无关，随后各喂一个 loopback 端）");
  println!();

  // ────────────────────────────────────────────────────────────────
  // 真正的运行时：4 个后台 loop（全部由门面自带的 run 方法驱动）
  // ────────────────────────────────────────────────────────────────
  let mut pool = LocalPool::new();
  spawn_all_loops(&pool, notifier, receiver, a_send, a_recv, b_send, b_recv);

  pool.run_until(async move {
    // ================================================================
    // 幕1：发现流程
    // ================================================================
    println!("① Notifier.discover() → Receiver 应自动回 AnnounceReply → Notifier 自动回 AssignId");
    notifier.discover();
    wait_for(
      || receiver.assigned_id() != u8::MAX && notifier.peers().len() == 1,
      10_000,
      "discovery",
    )
    .await;
    let assigned = receiver.assigned_id();
    println!("   ✓ 接收端拿到 receiver_id = {assigned}");
    println!("   ✓ 发送端 peer 目录已入库 1 条");
    println!();

    // ================================================================
    // 幕2：命令 + 自动 Ack
    // ================================================================
    println!("② Notifier.send_command(LedBlink count=3) → Receiver handler 触发");
    notifier.send_command(CommandBody::LedBlink {
      led_idx: 0,
      count: 3,
      period_ms: 100,
    });
    wait_for(
      || HANDLER_INVOCATIONS.load(Ordering::Relaxed) >= 1,
      10_000,
      "handler_invoked",
    )
    .await;
    println!(
      "   ✓ handler 被调用 {} 次，最近一次 count={}",
      HANDLER_INVOCATIONS.load(Ordering::Relaxed),
      HANDLER_LAST_LED_COUNT.load(Ordering::Relaxed),
    );
    println!();

    // ================================================================
    // 幕3：状态帧（GamepadState）广播
    // ================================================================
    println!("③ Notifier.send_frame(GamepadState) → 通过编排通道广播");
    {
      use protocol::ButtonBits;
      let mut state = GamepadState::EMPTY;
      state.set_button(ButtonBits::Btn1, true);
      state.joy_x = 12_345;
      let seq = ns.keyring.next_seq();
      let frame = Frame::new(seq, state);
      notifier.send_frame(&frame);
    }
    // 状态帧接收端本 demo 未挂消费者，只需给 loop 一点时间把它 flush 出去
    for _ in 0..200 {
      embassy_futures::yield_now().await;
    }
    println!("   ✓ 一帧 GamepadState 已通过 FrameSignal 交给 broadcast_loop");
    println!();

    // ================================================================
    // 汇总
    // ================================================================
    println!("──── 汇总 ────");
    let snap = notifier.peers();
    println!("Notifier.peers.len()        = {}", snap.len());
    if let Some(peer) = snap.first() {
      println!("  peer[0].mac               = {:02X?}", peer.mac);
      println!(
        "  peer[0].role_tag          = {}",
        core::str::from_utf8(peer.role_bytes()).unwrap_or("<non-utf8>"),
      );
    }
    println!("Receiver.assigned_id        = {}", receiver.assigned_id());
    println!(
      "Handler invocations         = {}",
      HANDLER_INVOCATIONS.load(Ordering::Relaxed)
    );
    // 出站有界队列的丢弃可观测性（comm::metrics）——健康时应恒为 0
    let drops = comm::metrics::snapshot();
    println!(
      "Dropped commands/responses  = {} / {}  (clean={})",
      drops.commands,
      drops.responses,
      drops.is_clean(),
    );
    println!();
    println!("✅ 演示完成");
  });
}
