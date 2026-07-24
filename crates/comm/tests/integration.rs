//! # host 端集成测试
//!
//! 用 [`LoopbackLink`](comm::loopback) 造一对 endpoint，让
//! [`Notifier`] 和 [`Receiver`] 通过内存 mpsc 通信，端到端验证：
//!
//! 1. **发现流程**：Notifier.discover() → Receiver 自动回 AnnounceReply →
//!    Notifier 自动回 AssignId → Receiver 的 my_id 被写入
//! 2. **命令回执**：Notifier.send_command(LedBlink) → Receiver handler 被调用
//!    → 自动回 Ack
//! 3. **抗重放**：重复注入 seq 相同的命令 → handler 只被调 1 次
//! 4. **selector 协同**：peer 入库后 selector 能选中它
//!
//! ## 执行模型
//! `futures_executor::LocalPool` 单线程；主 test 逻辑写成 async fn，
//! 通过 `pool.run_until(main_fut)` 驱动 —— pool 会同时轮询后台 loop 任务。

#![cfg(all(feature = "loopback", feature = "test-utils"))]

use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use comm::loopback::{LoopbackRecvEnd, LoopbackSendEnd, pair};
use comm::notifier::signals::{CommandOutSignal, FrameSignal, ResponseSignal};
use comm::notifier::{
  ResponseHandler, run_broadcast_loop, run_receive_loop as run_notifier_recv_loop,
};
use comm::receiver::{FrameHandler, run_receive_loop as run_receiver_recv_loop};
use comm::{
  CommandBody, CommandOutcome, CommandSource, ErrorCode, Frame, GamepadState, Keyring,
  PeerRegistry, ReplayGuard, Selector,
};
use futures_executor::LocalPool;
use futures_util::task::LocalSpawnExt;

const MAC_A: [u8; 6] = [0xAA; 6];
const MAC_B: [u8; 6] = [0xBB; 6];

// ============================================================
// 进程级测试锁：隔离"改写全局 SESSION_NONCE 的测试"与"依赖 HMAC 的测试"
// ============================================================
//
// # 背景（既有测试基建的并发缺陷）
// `protocol::SESSION_NONCE` 是**进程级全局**（`static AtomicU32`），HMAC 的
// 计算与校验都会读它（`compute_hmac_tag` 把 nonce 作为前缀混入）。host 集成
// 测试里 notifier 与 receiver 跑在同一进程、共享这枚全局 nonce。
//
// 默认多线程 harness 下，`receiver_adopts_nonce_from_nonce_hello_broadcast`
// 会在运行中途改写 SESSION_NONCE；若此刻另一测试正处于"编码命令（用旧 nonce
// 算 HMAC）"与"接收侧校验（用被改写后的新 nonce 重算 HMAC）"之间，两侧 nonce
// 不一致 → HMAC 校验失败 → 命令被丢弃 → 该测试超时/断言失败 → 偶发 flaky。
// （单线程 `--test-threads=1` 不会触发，因为测试串行执行。）
//
// # 修复：读写锁把"改写者"与"HMAC 依赖者"互斥
// - 改写 nonce 的测试取**写锁**（独占，运行期间不允许任何 HMAC 测试并发）。
// - 依赖 HMAC 校验（Announce/AssignId 握手、命令、Response 上报）的测试取
//   **读锁**：彼此之间仍可并行（互不改写 nonce，只要 nonce 在单个测试内保持
//   稳定，HMAC 两侧就一致），但与改写者互斥。
// - 纯 Frame 测试（只有 CRC、无 HMAC，见测试 5/8/11/12）不读 nonce，无需上锁，
//   保持完全并行。
static SESSION_NONCE_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// 依赖 HMAC 校验的测试在函数开头调用，取读锁并持有到函数结束。
///
/// 用 `unwrap_or_else(PoisonError::into_inner)` 忽略锁中毒：某个测试 panic
/// 只是让整轮 `cargo test` 失败，不应让后续测试因锁中毒而 panic 掩盖真实断言。
fn hmac_test_guard() -> std::sync::RwLockReadGuard<'static, ()> {
  SESSION_NONCE_LOCK
    .read()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 改写全局 nonce 的测试在函数开头调用，取写锁（独占）并持有到函数结束。
fn nonce_mutator_guard() -> std::sync::RwLockWriteGuard<'static, ()> {
  SESSION_NONCE_LOCK
    .write()
    .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ============================================================
// fixture：所有 `'static` 状态一次性 leak 出来
// ============================================================

struct NotifierState {
  keyring: &'static Keyring,
  peers: &'static PeerRegistry,
  replay: &'static ReplayGuard,
  selector: &'static Selector,
  frame_sig: &'static FrameSignal,
  cmd_sig: &'static CommandOutSignal,
  resp_sig: &'static ResponseSignal,
}

impl NotifierState {
  fn leak() -> Self {
    Self {
      keyring: Box::leak(Box::new(Keyring::new())),
      peers: Box::leak(Box::new(PeerRegistry::new())),
      replay: Box::leak(Box::new(ReplayGuard::new())),
      selector: Box::leak(Box::new(Selector::broadcast_all())),
      frame_sig: Box::leak(Box::new(FrameSignal::new())),
      cmd_sig: Box::leak(Box::new(CommandOutSignal::new())),
      resp_sig: Box::leak(Box::new(ResponseSignal::new())),
    }
  }
}

struct ReceiverState {
  keyring: &'static Keyring,
  replay: &'static ReplayGuard,
  resp_sig: &'static ResponseSignal,
  frame_sig: &'static FrameSignal,
  cmd_sig: &'static CommandOutSignal,
  my_id: &'static AtomicU8,
}

impl ReceiverState {
  fn leak() -> Self {
    Self {
      keyring: Box::leak(Box::new(Keyring::new())),
      replay: Box::leak(Box::new(ReplayGuard::new())),
      resp_sig: Box::leak(Box::new(ResponseSignal::new())),
      frame_sig: Box::leak(Box::new(FrameSignal::new())),
      cmd_sig: Box::leak(Box::new(CommandOutSignal::new())),
      my_id: Box::leak(Box::new(AtomicU8::new(u8::MAX))),
    }
  }
}

// ============================================================
// 通用：spawn 双端 4 个 loop task
// ============================================================

// 参数多不可避：双端各 2 个 endpoint（send/recv）+ 2 个 state 引用 + 1 个 command handler +
// 1 个可选 frame handler + 1 个可选 response_handler + 1 个 pool = 10 个。若强行抽成
// 结构体反而需要定义一堆只在测试中使用的类型，得不偿失。本 helper 仅限集成测试
// 内部，不导出，局部豁免 clippy 阈值。
#[allow(clippy::too_many_arguments)]
fn spawn_all_loops(
  pool: &LocalPool,
  ns: &NotifierState,
  rs: &ReceiverState,
  a_send: LoopbackSendEnd,
  a_recv: LoopbackRecvEnd,
  b_send: LoopbackSendEnd,
  b_recv: LoopbackRecvEnd,
  handler: fn(CommandSource, &comm::Command) -> CommandOutcome,
  frame_handler: Option<FrameHandler>,
  response_handler: Option<ResponseHandler>,
) {
  let spawner = pool.spawner();

  // Notifier 端
  let ns_frame = ns.frame_sig;
  let ns_cmd = ns.cmd_sig;
  let ns_resp = ns.resp_sig;
  let ns_peers = ns.peers;
  spawner
    .spawn_local(async move {
      // Notifier 侧传 Some(&PEERS)：单目标 Frame 自动单播
      run_broadcast_loop(a_send, Some(ns_peers), ns_frame, ns_cmd, ns_resp).await;
    })
    .expect("spawn notifier broadcast_loop");

  let ns_keyring = ns.keyring;
  let ns_replay = ns.replay;
  spawner
    .spawn_local(async move {
      run_notifier_recv_loop(
        a_recv,
        ns_peers,
        ns_cmd,
        ns_keyring,
        ns_replay,
        ns_resp,
        None,
        response_handler,
      )
      .await;
    })
    .expect("spawn notifier recv_loop");

  // Receiver 端
  let rs_frame = rs.frame_sig;
  let rs_cmd = rs.cmd_sig;
  let rs_resp = rs.resp_sig;
  spawner
    .spawn_local(async move {
      // 复用与 Notifier 相同的 run_broadcast_loop：三路 select（Frame/Response/Command）
      // 已涵盖 receiver 主动 report / send_frame / send_command 的出站需求。
      // Receiver 无 PeerRegistry → 传 None → Frame 恒广播。
      run_broadcast_loop(b_send, None, rs_frame, rs_cmd, rs_resp).await;
    })
    .expect("spawn receiver broadcast_loop");

  let rs_keyring = rs.keyring;
  let rs_replay = rs.replay;
  let rs_my_id = rs.my_id;
  spawner
    .spawn_local(async move {
      run_receiver_recv_loop(
        b_recv,
        rs_keyring,
        rs_replay,
        rs_resp,
        *b"led",
        MAC_B,
        rs_my_id,
        handler,
        CommandSource::EspNow,
        frame_handler,
      )
      .await;
    })
    .expect("spawn receiver recv_loop");
}

/// 在 async 环境中反复 yield，直到条件成立或超过 `max_iters`
async fn wait_for(mut cond: impl FnMut() -> bool, max_iters: usize) {
  for _ in 0..max_iters {
    if cond() {
      return;
    }
    embassy_futures::yield_now().await;
  }
  panic!("condition not met within {max_iters} yields");
}

// ============================================================
// 测试 1：发现 → AssignId
// ============================================================

#[test]
fn discovery_and_assign_id_flow() {
  let _nonce_guard = hmac_test_guard();
  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let handler: fn(CommandSource, &comm::Command) -> CommandOutcome =
    |_src, _cmd| CommandOutcome::Ok;
  spawn_all_loops(
    &pool, &ns, &rs, a_send, a_recv, b_send, b_recv, handler, None, None,
  );

  pool.run_until(async move {
    // 触发 discover：往 CMD_SIG 塞一条 Announce
    {
      use protocol::{Command, CommandBody as CB, encode_command};
      let seq = ns.keyring.next_seq();
      let cmd = Command::with_key(seq, ns.keyring.active(), CB::Announce);
      ns.cmd_sig
        .signal(comm::notifier::signals::OutboundCommand::broadcast(
          encode_command(&cmd),
        ));
    }

    // 期望：Receiver 拿到 id (my_id != u8::MAX) 且 Notifier 侧 peer 目录有 1 条
    wait_for(
      || rs.my_id.load(Ordering::Relaxed) != u8::MAX && ns.peers.len() == 1,
      10_000,
    )
    .await;
  });

  // 断言最终状态
  assert_eq!(rs.my_id.load(Ordering::Relaxed), 0, "receiver 应拿到 id=0");
  assert_eq!(ns.peers.len(), 1, "notifier 侧 peer 目录应有 1 条");
  let snap = ns.peers.snapshot();
  assert_eq!(snap[0].mac, MAC_B);
  assert_eq!(snap[0].role_bytes(), b"led");
}

// ============================================================
// 测试 2：send_command → handler → Ack
// ============================================================

static HANDLER_INVOCATIONS: AtomicU32 = AtomicU32::new(0);
static HANDLER_LAST_COUNT: AtomicU32 = AtomicU32::new(0);

#[test]
fn command_flow_triggers_handler_and_returns_ack() {
  let _nonce_guard = hmac_test_guard();
  HANDLER_INVOCATIONS.store(0, Ordering::Relaxed);
  HANDLER_LAST_COUNT.store(0, Ordering::Relaxed);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let handler: fn(CommandSource, &comm::Command) -> CommandOutcome = |_src, cmd| match cmd.kind {
    CommandBody::LedBlink { count, .. } => {
      HANDLER_LAST_COUNT.store(u32::from(count), Ordering::Relaxed);
      HANDLER_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
      CommandOutcome::Ok
    }
    _ => CommandOutcome::Err(ErrorCode::Unsupported),
  };
  spawn_all_loops(
    &pool, &ns, &rs, a_send, a_recv, b_send, b_recv, handler, None, None,
  );

  pool.run_until(async move {
    // 直接注入一条 LedBlink（跳过发现流程，用同 default key）
    {
      use protocol::{Command, CommandBody as CB, encode_command};
      let seq = ns.keyring.next_seq();
      let cmd = Command::with_key(
        seq,
        ns.keyring.active(),
        CB::LedBlink {
          led_idx: 0,
          count: 3,
          period_ms: 100,
        },
      );
      ns.cmd_sig
        .signal(comm::notifier::signals::OutboundCommand::broadcast(
          encode_command(&cmd),
        ));
    }
    wait_for(|| HANDLER_INVOCATIONS.load(Ordering::Relaxed) >= 1, 10_000).await;
  });

  assert_eq!(HANDLER_INVOCATIONS.load(Ordering::Relaxed), 1);
  assert_eq!(HANDLER_LAST_COUNT.load(Ordering::Relaxed), 3);
}

// ============================================================
// 测试 3：抗重放
// ============================================================

static REPLAY_HANDLER_INVOCATIONS: AtomicU32 = AtomicU32::new(0);

#[test]
fn anti_replay_rejects_duplicate_seq() {
  let _nonce_guard = hmac_test_guard();
  REPLAY_HANDLER_INVOCATIONS.store(0, Ordering::Relaxed);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let handler: fn(CommandSource, &comm::Command) -> CommandOutcome = |_src, _cmd| {
    REPLAY_HANDLER_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
    CommandOutcome::Ok
  };
  spawn_all_loops(
    &pool, &ns, &rs, a_send, a_recv, b_send, b_recv, handler, None, None,
  );

  pool.run_until(async move {
    use protocol::{Command, CommandBody as CB, encode_command};
    let cmd = Command::with_key(
      42,
      ns.keyring.active(),
      CB::LedBlink {
        led_idx: 0,
        count: 1,
        period_ms: 50,
      },
    );
    let bytes = encode_command(&cmd);
    ns.cmd_sig
      .signal(comm::notifier::signals::OutboundCommand::broadcast(bytes));
    // 等第一条到达
    wait_for(
      || REPLAY_HANDLER_INVOCATIONS.load(Ordering::Relaxed) >= 1,
      10_000,
    )
    .await;
    // 灌一条完全一样的（重放）—— 需要让 CMD_SIG 已被 broadcast_loop 消费
    // 再 signal 才能触发第二次发送
    for _ in 0..50 {
      embassy_futures::yield_now().await;
    }
    ns.cmd_sig
      .signal(comm::notifier::signals::OutboundCommand::broadcast(bytes));
    // 再多跑一些 tick，让第二条走完 wire；handler 不应被再次调用
    for _ in 0..2_000 {
      embassy_futures::yield_now().await;
    }
  });

  assert_eq!(
    REPLAY_HANDLER_INVOCATIONS.load(Ordering::Relaxed),
    1,
    "重放帧应被 anti-replay 拒绝"
  );
}

// ============================================================
// 测试 4：selector 与 peer_registry 协同
// ============================================================

#[test]
fn selector_reflects_discovered_peer() {
  let _nonce_guard = hmac_test_guard();
  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let handler: fn(CommandSource, &comm::Command) -> CommandOutcome = |_src, _| CommandOutcome::Ok;
  spawn_all_loops(
    &pool, &ns, &rs, a_send, a_recv, b_send, b_recv, handler, None, None,
  );

  pool.run_until(async move {
    use protocol::{Command, CommandBody as CB, encode_command};
    let seq = ns.keyring.next_seq();
    let cmd = Command::with_key(seq, ns.keyring.active(), CB::Announce);
    ns.cmd_sig
      .signal(comm::notifier::signals::OutboundCommand::broadcast(
        encode_command(&cmd),
      ));
    wait_for(|| !ns.peers.is_empty(), 10_000).await;
  });

  // 初始 selector = broadcast_all，peer 0 应处于选中态
  assert!(ns.selector.is_active_selected(0));
  // 切成"只留 receiver_id=0"
  ns.selector.set_active(1 << 0);
  assert!(ns.selector.is_active_selected(0));
  assert!(!ns.selector.is_active_selected(1));
}

// ============================================================
// 测试 5：Frame 端到端投递
// ============================================================
//
// 覆盖上一轮审查发现的问题：Receiver 侧原先没有 Frame 消费路径，
// notifier.send_frame() 广播出去的 GamepadState 被静默丢弃。
// 修复后：ReceiverBuilder::frame_handler / run_receive_loop 的 frame_handler
// 参数可让业务侧观测入站 Frame，且自动做 dest_mask 过滤。

static FRAME_INVOCATIONS: AtomicU32 = AtomicU32::new(0);
static FRAME_LAST_BUTTONS: AtomicU32 = AtomicU32::new(0);

#[test]
fn frame_flow_delivers_to_receiver() {
  FRAME_INVOCATIONS.store(0, Ordering::Relaxed);
  FRAME_LAST_BUTTONS.store(0, Ordering::Relaxed);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let cmd_handler: fn(CommandSource, &comm::Command) -> CommandOutcome =
    |_src, _cmd| CommandOutcome::Ok;
  let frame_handler: FrameHandler = |_src, frame| {
    FRAME_LAST_BUTTONS.store(u32::from(frame.payload.buttons), Ordering::Relaxed);
    FRAME_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
  };
  spawn_all_loops(
    &pool,
    &ns,
    &rs,
    a_send,
    a_recv,
    b_send,
    b_recv,
    cmd_handler,
    Some(frame_handler),
    None,
  );

  pool.run_until(async move {
    // 广播一条带自定义 buttons 的 Frame（dest_mask = 广播）
    let mut state = GamepadState::EMPTY;
    state.buttons = 0xBEEF;
    let frame = Frame::with_dest(1, state, u32::MAX);
    ns.frame_sig.signal(frame);

    wait_for(|| FRAME_INVOCATIONS.load(Ordering::Relaxed) >= 1, 10_000).await;
  });

  assert_eq!(
    FRAME_INVOCATIONS.load(Ordering::Relaxed),
    1,
    "frame_handler 应被调用一次"
  );
  assert_eq!(
    FRAME_LAST_BUTTONS.load(Ordering::Relaxed),
    0xBEEF,
    "payload.buttons 应完整穿透 encode→decode"
  );
}

// ============================================================
// 测试 6：dest_mask 过滤 —— 未寻址本机时 handler 不被调用
// ============================================================
//
// receiver 已被分配 id=0（通过前置 Announce），随后发送一条 dest_mask
// 只指向 receiver_id=3 的 Frame —— 此时 frame_handler **不应**被触发。

static FRAME_FILTER_INVOCATIONS: AtomicU32 = AtomicU32::new(0);

#[test]
fn frame_dest_mask_filters_by_id() {
  // 本测试先走 Announce/AssignId（HMAC）握手拿 id=0，故需读锁隔离 nonce 改写者。
  let _nonce_guard = hmac_test_guard();
  FRAME_FILTER_INVOCATIONS.store(0, Ordering::Relaxed);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let cmd_handler: fn(CommandSource, &comm::Command) -> CommandOutcome =
    |_src, _cmd| CommandOutcome::Ok;
  let frame_handler: FrameHandler = |_src, _frame| {
    FRAME_FILTER_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
  };
  spawn_all_loops(
    &pool,
    &ns,
    &rs,
    a_send,
    a_recv,
    b_send,
    b_recv,
    cmd_handler,
    Some(frame_handler),
    None,
  );

  pool.run_until(async move {
    use protocol::{Command, CommandBody as CB, encode_command};

    // 先走一遍 discover，让 receiver 拿到 id=0
    let seq = ns.keyring.next_seq();
    let cmd = Command::with_key(seq, ns.keyring.active(), CB::Announce);
    ns.cmd_sig
      .signal(comm::notifier::signals::OutboundCommand::broadcast(
        encode_command(&cmd),
      ));
    wait_for(|| rs.my_id.load(Ordering::Relaxed) != u8::MAX, 10_000).await;
    assert_eq!(rs.my_id.load(Ordering::Relaxed), 0);

    // 发送 dest_mask 只寻址 receiver_id=3 的 Frame（本机 id=0，不应命中）
    let frame = Frame::with_dest(1, GamepadState::EMPTY, 1u32 << 3);
    ns.frame_sig.signal(frame);

    // 再多转几圈，确保 wire 上的 Frame 已经到达 receiver 并走完派发
    for _ in 0..2_000 {
      embassy_futures::yield_now().await;
    }
  });

  assert_eq!(
    FRAME_FILTER_INVOCATIONS.load(Ordering::Relaxed),
    0,
    "dest_mask 未覆盖本机 id 时 frame_handler 应被过滤掉"
  );
}

// ============================================================
// 测试 7：Receiver.report() → Notifier 侧 response_handler 收到
// ============================================================
//
// 覆盖 P0：Endpoint-initiated publishing。Receiver 侧调 report(BatterySnapshot{ 85 })
// 后，Notifier 侧新增的 response_handler 应能收到并观察到 payload。

use core::sync::atomic::AtomicBool;

use protocol::ResponseBody;

static REPORT_HANDLER_INVOCATIONS: AtomicU32 = AtomicU32::new(0);
static REPORT_LAST_PERCENT: AtomicU32 = AtomicU32::new(0);
static REPORT_SAW_ANNOUNCE_REPLY: AtomicBool = AtomicBool::new(false);

#[test]
fn receiver_report_reaches_notifier() {
  // 走 discover 握手 + Response 上报，两条路径都依赖 HMAC → 读锁隔离。
  let _nonce_guard = hmac_test_guard();
  REPORT_HANDLER_INVOCATIONS.store(0, Ordering::Relaxed);
  REPORT_LAST_PERCENT.store(0, Ordering::Relaxed);
  REPORT_SAW_ANNOUNCE_REPLY.store(false, Ordering::Relaxed);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let cmd_handler: fn(CommandSource, &comm::Command) -> CommandOutcome =
    |_src, _cmd| CommandOutcome::Ok;
  // response_handler：转发 receiver 主动上报的 Response
  let response_handler: ResponseHandler = |resp| match resp.body {
    ResponseBody::BatterySnapshot { percent } => {
      REPORT_LAST_PERCENT.store(u32::from(percent), Ordering::Relaxed);
      REPORT_HANDLER_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
    ResponseBody::AnnounceReply { .. } => {
      // 若能看到 AnnounceReply 说明"handle_incoming_response 把 AnnounceReply 也转出来了"，
      // 这在设计上是不允许的——AnnounceReply 是 comm 内部机制。此断言用于回归防守。
      REPORT_SAW_ANNOUNCE_REPLY.store(true, Ordering::Relaxed);
    }
    _ => {}
  };
  spawn_all_loops(
    &pool,
    &ns,
    &rs,
    a_send,
    a_recv,
    b_send,
    b_recv,
    cmd_handler,
    None,
    Some(response_handler),
  );

  pool.run_until(async move {
    // 先走一遍 discover，让链路彻底跑通（也顺便验证 AnnounceReply 不会漏进 response_handler）
    {
      use protocol::{Command, CommandBody as CB, encode_command};
      let seq = ns.keyring.next_seq();
      let cmd = Command::with_key(seq, ns.keyring.active(), CB::Announce);
      ns.cmd_sig
        .signal(comm::notifier::signals::OutboundCommand::broadcast(
          encode_command(&cmd),
        ));
    }
    wait_for(|| rs.my_id.load(Ordering::Relaxed) != u8::MAX, 10_000).await;

    // Receiver 主动上报电量 —— 走**生产代码路径** `Receiver::report()`
    //
    // # 消除测试盲区
    // 上一版直接 `rs.resp_sig.signal(...)` 绕过了 Receiver::report()，导致万一
    // report() 的 `req_seq=0` / `key_id=keyring.active()` 逻辑被改坏，测试仍会
    // 绿。改造后：通过 `test_receiver_from_parts` 构造真实 Receiver 实体（同一
    // 组 signals），调用 `.report()` 走完整 API 路径。
    let receiver: comm::Receiver<comm::link::DummyLink> = comm::receiver::test_receiver_from_parts(
      rs.keyring,
      rs.replay,
      rs.resp_sig,
      rs.frame_sig,
      rs.cmd_sig,
      *b"led",
      MAC_B,
      rs.my_id,
      cmd_handler,
    );
    receiver.report(ResponseBody::BatterySnapshot { percent: 85 });

    wait_for(
      || REPORT_HANDLER_INVOCATIONS.load(Ordering::Relaxed) >= 1,
      10_000,
    )
    .await;
  });

  assert_eq!(
    REPORT_HANDLER_INVOCATIONS.load(Ordering::Relaxed),
    1,
    "response_handler 应被调用一次"
  );
  assert_eq!(
    REPORT_LAST_PERCENT.load(Ordering::Relaxed),
    85,
    "上报的 percent 应完整穿透 encode→decode"
  );
  assert!(
    !REPORT_SAW_ANNOUNCE_REPLY.load(Ordering::Relaxed),
    "AnnounceReply 属于 comm 内部机制，不应转发给业务 response_handler"
  );
}

// ============================================================
// 测试 8：Receiver.send_frame() → Notifier（双身份）frame_handler 收到
// ============================================================
//
// 覆盖 P1：Endpoint 主动广播 Frame。receiver 侧往 frame_sig 塞一条 Frame，
// notifier 端启用"双身份"（with_command_handler + with_frame_handler）以订阅
// 入站 Frame —— frame_handler 应被回调。

static UPSTREAM_FRAME_INVOCATIONS: AtomicU32 = AtomicU32::new(0);
static UPSTREAM_FRAME_LAST_BUTTONS: AtomicU32 = AtomicU32::new(0);

#[test]
fn receiver_send_frame_reaches_notifier_dual_role() {
  UPSTREAM_FRAME_INVOCATIONS.store(0, Ordering::Relaxed);
  UPSTREAM_FRAME_LAST_BUTTONS.store(0, Ordering::Relaxed);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();

  // 为让 Notifier 侧订阅 Frame，需要走"双身份" run_notifier_recv_loop。
  // spawn_all_loops 目前是"纯 notifier + receiver-only"的默认拓扑，本测试
  // 手工拆开 spawn，让 notifier 也带上 frame_handler。
  let cmd_handler: fn(CommandSource, &comm::Command) -> CommandOutcome =
    |_src, _cmd| CommandOutcome::Ok;
  let frame_handler: FrameHandler = |_src, frame| {
    UPSTREAM_FRAME_LAST_BUTTONS.store(u32::from(frame.payload.buttons), Ordering::Relaxed);
    UPSTREAM_FRAME_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
  };

  // Notifier 端：broadcast_loop + 双身份 recv_loop
  let spawner = pool.spawner();
  {
    let ns_frame = ns.frame_sig;
    let ns_cmd = ns.cmd_sig;
    let ns_resp = ns.resp_sig;
    let ns_peers = ns.peers;
    spawner
      .spawn_local(async move {
        run_broadcast_loop(a_send, Some(ns_peers), ns_frame, ns_cmd, ns_resp).await;
      })
      .expect("spawn notifier broadcast_loop");
  }
  // notifier 侧的 my_id / my_mac 只是为了满足 CommandHandlerConfig 参数——
  // 本测试并不真的让 notifier 处理 Command，只关心它订阅 Frame。
  let ns_my_id: &'static AtomicU8 = Box::leak(Box::new(AtomicU8::new(u8::MAX)));
  let handler_config = comm::notifier::CommandHandlerConfig {
    handler: cmd_handler,
    role_tag: *b"hst",
    my_mac: MAC_A,
    my_id: ns_my_id,
    src: CommandSource::EspNow,
    frame_handler: Some(frame_handler),
  };
  {
    let ns_peers = ns.peers;
    let ns_cmd = ns.cmd_sig;
    let ns_keyring = ns.keyring;
    let ns_replay = ns.replay;
    let ns_resp = ns.resp_sig;
    spawner
      .spawn_local(async move {
        run_notifier_recv_loop(
          a_recv,
          ns_peers,
          ns_cmd,
          ns_keyring,
          ns_replay,
          ns_resp,
          Some(handler_config),
          None,
        )
        .await;
      })
      .expect("spawn notifier recv_loop (dual-role)");
  }

  // Receiver 端：broadcast_loop + recv_loop（standard）
  {
    let rs_frame = rs.frame_sig;
    let rs_cmd = rs.cmd_sig;
    let rs_resp = rs.resp_sig;
    spawner
      .spawn_local(async move {
        run_broadcast_loop(b_send, None, rs_frame, rs_cmd, rs_resp).await;
      })
      .expect("spawn receiver broadcast_loop");
  }
  {
    let rs_keyring = rs.keyring;
    let rs_replay = rs.replay;
    let rs_resp = rs.resp_sig;
    let rs_my_id = rs.my_id;
    spawner
      .spawn_local(async move {
        run_receiver_recv_loop(
          b_recv,
          rs_keyring,
          rs_replay,
          rs_resp,
          *b"led",
          MAC_B,
          rs_my_id,
          cmd_handler,
          CommandSource::EspNow,
          None,
        )
        .await;
      })
      .expect("spawn receiver recv_loop");
  }

  pool.run_until(async move {
    // Receiver 主动广播 Frame —— 生产代码路径是 Receiver::send_frame(&frame)；
    // 这里直接往同一个 signal 塞入等价数据（语义 1:1 对应）。
    let mut state = GamepadState::EMPTY;
    state.buttons = 0xCAFE;
    let frame = Frame::with_dest(1, state, u32::MAX);
    rs.frame_sig.signal(frame);

    wait_for(
      || UPSTREAM_FRAME_INVOCATIONS.load(Ordering::Relaxed) >= 1,
      10_000,
    )
    .await;
  });

  assert_eq!(
    UPSTREAM_FRAME_INVOCATIONS.load(Ordering::Relaxed),
    1,
    "notifier 端 frame_handler 应被 receiver 主动上行的 Frame 触发一次"
  );
  assert_eq!(
    UPSTREAM_FRAME_LAST_BUTTONS.load(Ordering::Relaxed),
    0xCAFE,
    "receiver→notifier 方向的 Frame payload 应完整穿透 encode→decode"
  );
}

// ============================================================
// 测试 9：AssignId 丢失后可自愈（Updated 路径重发）
// ============================================================
//
// 回归防守：若 notifier 只在 peer 首次入库（Inserted）时下发一次 AssignId，
// 一旦那条 AssignId 因覆盖式信号被覆盖 / 射频丢包而丢失，peer 会永久停留在
// UNASSIGNED_ID（后续 AnnounceReply 只返回 Updated，不再补发）。
//
// 修复后：每次 AnnounceReply 都重发 AssignId（幂等）。本测试模拟"第一条
// AssignId 丢失"——手动把 receiver 的 my_id 复位为 UNASSIGNED，再触发一次
// discover，验证第二轮（Updated 路径）会重新把 id 分配回来。

#[test]
fn assign_id_resends_on_updated_and_self_heals() {
  let _nonce_guard = hmac_test_guard();
  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let handler: fn(CommandSource, &comm::Command) -> CommandOutcome = |_src, _| CommandOutcome::Ok;
  spawn_all_loops(
    &pool, &ns, &rs, a_send, a_recv, b_send, b_recv, handler, None, None,
  );

  // 所有需要的字段都是 `&'static`（Copy），先取出来供 `async move` 与结尾断言共用。
  let ns_keyring = ns.keyring;
  let ns_cmd = ns.cmd_sig;
  let ns_peers = ns.peers;
  let rs_my_id = rs.my_id;

  let send_announce = move || {
    use protocol::{Command, CommandBody as CB, encode_command};
    let seq = ns_keyring.next_seq();
    let cmd = Command::with_key(seq, ns_keyring.active(), CB::Announce);
    ns_cmd.signal(comm::notifier::signals::OutboundCommand::broadcast(
      encode_command(&cmd),
    ));
  };

  pool.run_until(async move {
    // 第一轮发现：peer 入库（Inserted）→ 拿到 id=0
    send_announce();
    wait_for(|| rs_my_id.load(Ordering::Relaxed) != u8::MAX, 10_000).await;
    assert_eq!(rs_my_id.load(Ordering::Relaxed), 0, "首轮应分配 id=0");
    assert_eq!(ns_peers.len(), 1, "peer 已入库");

    // 模拟"AssignId 丢失"：把 receiver 的 my_id 复位为 UNASSIGNED
    rs_my_id.store(u8::MAX, Ordering::Relaxed);

    // 第二轮发现：peer 已在 registry 里，upsert 返回 Updated；
    // 修复后应仍重发 AssignId，让 receiver 重新拿回 id。
    send_announce();
    wait_for(|| rs_my_id.load(Ordering::Relaxed) != u8::MAX, 10_000).await;
  });

  assert_eq!(
    rs_my_id.load(Ordering::Relaxed),
    0,
    "Updated 路径应重发 AssignId，receiver 自愈回 id=0"
  );
  assert_eq!(ns_peers.len(), 1, "peer 数量不应因重发变化");
}

// ============================================================
// 测试 10：Receiver 从 NonceHello 广播同步 session nonce（K3 bootstrap）
// ============================================================
//
// 覆盖上一轮审查发现的 comm 缺口：receiver 侧原先丢弃所有 Response 帧，
// 无法采纳 Coordinator 广播的 NonceHello，导致跨设备 nonce 无法同步、
// HMAC 校验只能靠 debug-auth-bypass 绕过。
//
// 修复后：receiver 的 dispatch 会免鉴权读取 NonceHello 里的 nonce 并写入
// 全局 SESSION_NONCE。本测试把一条 NonceHello 通过 notifier→receiver 链路
// 送出，验证 receiver 侧把 session nonce 更新成广播值。
//
// 注：host 测试里 notifier / receiver 共享同一进程内的全局 SESSION_NONCE，
// 因此这里断言的是"全局 nonce 被 dispatch 改成广播值"——先置一个不同的
// 初值，再验证它被 NonceHello 覆盖。

use protocol::{CommandResponse, init_session_nonce, session_nonce};

#[test]
fn receiver_adopts_nonce_from_nonce_hello_broadcast() {
  const START_NONCE: u32 = 0x1111_1111;
  const BROADCAST_NONCE: u32 = 0x2222_2222;

  // 本测试改写进程级全局 SESSION_NONCE，取写锁独占，避免与依赖 HMAC 的测试并发
  // （否则会在它们"编码"与"校验"之间篡改 nonce，导致偶发 HMAC 校验失败）。
  let _nonce_guard = nonce_mutator_guard();

  // 先把全局 nonce 置成一个与广播值不同的初值。
  init_session_nonce(START_NONCE);
  assert_eq!(session_nonce(), START_NONCE);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let handler: fn(CommandSource, &comm::Command) -> CommandOutcome = |_src, _| CommandOutcome::Ok;
  spawn_all_loops(
    &pool, &ns, &rs, a_send, a_recv, b_send, b_recv, handler, None, None,
  );

  let ns_resp = ns.resp_sig;

  pool.run_until(async move {
    // Notifier 侧广播一条 NonceHello（走 broadcast_loop → wire → receiver dispatch）
    ns_resp.signal(CommandResponse::nonce_hello(BROADCAST_NONCE));
    wait_for(|| session_nonce() == BROADCAST_NONCE, 10_000).await;
  });

  assert_eq!(
    session_nonce(),
    BROADCAST_NONCE,
    "receiver 的 dispatch 应免鉴权采纳 NonceHello 里的 nonce"
  );
}

// ============================================================
// 测试 11：单目标 Frame → 自动单播仍送达（dest_mask 随帧携带，receiver 过滤通过）
// ============================================================
//
// 覆盖新增的"Frame 自动寻址"：notifier.peers 记录了目标 MAC 后，dest_mask
// 恰好选中单个 receiver 的 Frame 会被 run_broadcast_loop 升级为单播。
//
// # 为什么不走 discover 握手
// Frame 只有 CRC、**无 HMAC**，因此本测试直接把 peer 塞进 registry、把 receiver
// 的 my_id 写死，绕开 Announce/AssignId（那条通路依赖进程级全局 SESSION_NONCE，
// 与 test 10 并行时会互相踩踏——那是既有测试基建的独立问题，见文末报告）。
// 这样本测试对并行顺序不敏感、可确定性通过。
//
// # loopback 无法区分单播 vs 广播的说明
// LoopbackSendEnd::send 忽略 `dst`，无论广播地址还是单播 MAC 都投递给配对端；
// 因此本集成测试**无法**在路由层面断言"确实走了单播地址"。这里断言的是
// **行为正确性**：单播路径下帧仍携带原 dest_mask，故 receiver 的 dest_mask
// 过滤（本机 id=0，bit0 置位）依然放行、payload 完整送达。单播 vs 广播的
// **决策逻辑**由 `notifier::tests::*`（frame_dest 纯函数单测）直接覆盖。

static AUTO_UNICAST_FRAME_INVOCATIONS: AtomicU32 = AtomicU32::new(0);
static AUTO_UNICAST_FRAME_BUTTONS: AtomicU32 = AtomicU32::new(0);

#[test]
fn single_target_frame_auto_unicasts_and_delivers() {
  use embassy_time::Instant;

  AUTO_UNICAST_FRAME_INVOCATIONS.store(0, Ordering::Relaxed);
  AUTO_UNICAST_FRAME_BUTTONS.store(0, Ordering::Relaxed);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  // 直接登记 peer（MAC_B → receiver_id=0）作为单播反查前提，并把 receiver 的 my_id
  // 写成 0，跳过 HMAC 依赖的 discover 握手。
  let _ = ns.peers.upsert(MAC_B, *b"led", -20, Instant::from_ticks(0));
  rs.my_id.store(0, Ordering::Relaxed);

  let mut pool = LocalPool::new();
  let cmd_handler: fn(CommandSource, &comm::Command) -> CommandOutcome =
    |_src, _cmd| CommandOutcome::Ok;
  let frame_handler: FrameHandler = |_src, frame| {
    AUTO_UNICAST_FRAME_BUTTONS.store(u32::from(frame.payload.buttons), Ordering::Relaxed);
    AUTO_UNICAST_FRAME_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
  };
  spawn_all_loops(
    &pool,
    &ns,
    &rs,
    a_send,
    a_recv,
    b_send,
    b_recv,
    cmd_handler,
    Some(frame_handler),
    None,
  );

  pool.run_until(async move {
    // dest_mask 恰好选中 receiver_id=0 → run_broadcast_loop 自动单播到 MAC_B
    let mut state = GamepadState::EMPTY;
    state.buttons = 0x1234;
    let frame = Frame::with_dest(1, state, 1u32 << 0);
    ns.frame_sig.signal(frame);

    wait_for(
      || AUTO_UNICAST_FRAME_INVOCATIONS.load(Ordering::Relaxed) >= 1,
      10_000,
    )
    .await;
  });

  assert_eq!(
    AUTO_UNICAST_FRAME_INVOCATIONS.load(Ordering::Relaxed),
    1,
    "单目标 Frame 经自动单播后仍应送达目标 peer"
  );
  assert_eq!(
    AUTO_UNICAST_FRAME_BUTTONS.load(Ordering::Relaxed),
    0x1234,
    "单播帧应携带完整 payload，且 dest_mask 使 receiver 过滤放行"
  );
}

// ============================================================
// 测试 12：多目标 Frame → 广播路径仍送达
// ============================================================
//
// dest_mask 同时选中 bit0 + bit1（≥2 目标）时，run_broadcast_loop 不做 fan-out，
// 保持广播；本机 id=0 被寻址 → frame_handler 仍应触发。

static MULTI_TARGET_FRAME_INVOCATIONS: AtomicU32 = AtomicU32::new(0);

#[test]
fn multi_target_frame_broadcasts_and_delivers() {
  use embassy_time::Instant;

  MULTI_TARGET_FRAME_INVOCATIONS.store(0, Ordering::Relaxed);

  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  // 两个 peer 都已知（MAC_B→0，另一个虚构 MAC→1），但多目标仍应广播、不 fan-out。
  // 同样跳过 discover 握手（见 single_target 测试注释），确保并行确定性。
  let _ = ns.peers.upsert(MAC_B, *b"led", -20, Instant::from_ticks(0));
  let _ = ns
    .peers
    .upsert([0xCC; 6], *b"srv", -30, Instant::from_ticks(0));
  rs.my_id.store(0, Ordering::Relaxed);

  let mut pool = LocalPool::new();
  let cmd_handler: fn(CommandSource, &comm::Command) -> CommandOutcome =
    |_src, _cmd| CommandOutcome::Ok;
  let frame_handler: FrameHandler = |_src, _frame| {
    MULTI_TARGET_FRAME_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
  };
  spawn_all_loops(
    &pool,
    &ns,
    &rs,
    a_send,
    a_recv,
    b_send,
    b_recv,
    cmd_handler,
    Some(frame_handler),
    None,
  );

  pool.run_until(async move {
    // 多目标（bit0 + bit1，两者 MAC 均已知）→ 仍广播；本机 id=0 命中
    let frame = Frame::with_dest(1, GamepadState::EMPTY, (1u32 << 0) | (1u32 << 1));
    ns.frame_sig.signal(frame);

    wait_for(
      || MULTI_TARGET_FRAME_INVOCATIONS.load(Ordering::Relaxed) >= 1,
      10_000,
    )
    .await;
  });

  assert_eq!(
    MULTI_TARGET_FRAME_INVOCATIONS.load(Ordering::Relaxed),
    1,
    "多目标 Frame 应走广播且送达被寻址的本机"
  );
}

// ============================================================
// 测试：Phase 2 —— 定向单播命令 send_command_to
// ============================================================
//
// 不 spawn loop：send_command_to 只做 registry 反查 + 写覆盖式 CommandOutSignal，
// 直接用 `try_take()` 观察写出的 OutboundCommand 即可验证寻址正确。用 DummyLink
// 构造 Notifier（本方法不触碰 link），共享真实的 peers / keyring / cmd_sig。
#[test]
fn send_command_to_unicasts_registered_peer_and_reports_no_target_otherwise() {
  use comm::link::DummyLink;
  use comm::notifier::signals::CommandDest;
  use comm::notifier::{Notifier, NotifierError};
  use embassy_time::Instant;
  use protocol::COMMAND_MAGIC;

  let keyring = Box::leak(Box::new(Keyring::new()));
  let peers = Box::leak(Box::new(PeerRegistry::new()));
  let replay = Box::leak(Box::new(ReplayGuard::new()));
  let selector = Box::leak(Box::new(Selector::broadcast_all()));
  let frame_sig = Box::leak(Box::new(FrameSignal::new()));
  let cmd_sig = Box::leak(Box::new(CommandOutSignal::new()));
  let resp_sig = Box::leak(Box::new(ResponseSignal::new()));

  let notifier: Notifier<DummyLink> = Notifier::builder()
    .link(DummyLink)
    .keyring(keyring)
    .peers(peers)
    .replay(replay)
    .selector(selector)
    .frame_signal(frame_sig)
    .command_signal(cmd_sig)
    .response_signal(resp_sig)
    .build();

  // 空 registry：定向发送应报 NoTarget，且不写出任何命令
  assert_eq!(
    notifier.send_command_to(0, CommandBody::Nop),
    Err(NotifierError::NoTarget)
  );
  assert!(
    cmd_sig.try_take().is_none(),
    "NoTarget 不应向出站通道写入任何命令"
  );

  // 注册一个 peer（首个入库 → receiver_id = 0）
  let mac = [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01];
  let _ = peers.upsert(mac, *b"led", -20, Instant::from_ticks(0));

  // 定向发送 → 应写出一条 Unicast(mac) 的 OutboundCommand
  notifier
    .send_command_to(
      0,
      CommandBody::LedBlink {
        led_idx: 0,
        count: 3,
        period_ms: 100,
      },
    )
    .expect("已注册 peer 应是合法目标");

  let out = cmd_sig
    .try_take()
    .expect("send_command_to 应向出站通道写入一条命令");
  assert_eq!(
    out.dest,
    CommandDest::Unicast(mac),
    "目标寻址应为该 peer 的单播 MAC"
  );
  assert_eq!(
    u16::from_le_bytes([out.bytes[0], out.bytes[1]]),
    COMMAND_MAGIC,
    "写出的字节应是合法编码的 Command 帧"
  );

  // send_command_to_mac 直发路径同样产生 Unicast
  notifier.send_command_to_mac(mac, CommandBody::Nop);
  let out2 = cmd_sig.try_take().expect("send_command_to_mac 应写入命令");
  assert_eq!(out2.dest, CommandDest::Unicast(mac));
}
