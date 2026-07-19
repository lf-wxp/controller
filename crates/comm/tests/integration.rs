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
  spawner
    .spawn_local(async move {
      run_broadcast_loop(a_send, ns_frame, ns_cmd, ns_resp).await;
    })
    .expect("spawn notifier broadcast_loop");

  let ns_peers = ns.peers;
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
      // 已涵盖 receiver 主动 report / send_frame / send_command 的出站需求
      run_broadcast_loop(b_send, rs_frame, rs_cmd, rs_resp).await;
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
      use controller_protocol::{Command, CommandBody as CB, encode_command};
      let seq = ns.keyring.next_seq();
      let cmd = Command::with_key(seq, ns.keyring.active(), CB::Announce);
      ns.cmd_sig.signal(encode_command(&cmd));
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
      use controller_protocol::{Command, CommandBody as CB, encode_command};
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
      ns.cmd_sig.signal(encode_command(&cmd));
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
    use controller_protocol::{Command, CommandBody as CB, encode_command};
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
    ns.cmd_sig.signal(bytes);
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
    ns.cmd_sig.signal(bytes);
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
  let ns = NotifierState::leak();
  let rs = ReceiverState::leak();
  let (a_send, a_recv, b_send, b_recv) = pair(MAC_A, MAC_B);

  let mut pool = LocalPool::new();
  let handler: fn(CommandSource, &comm::Command) -> CommandOutcome = |_src, _| CommandOutcome::Ok;
  spawn_all_loops(
    &pool, &ns, &rs, a_send, a_recv, b_send, b_recv, handler, None, None,
  );

  pool.run_until(async move {
    use controller_protocol::{Command, CommandBody as CB, encode_command};
    let seq = ns.keyring.next_seq();
    let cmd = Command::with_key(seq, ns.keyring.active(), CB::Announce);
    ns.cmd_sig.signal(encode_command(&cmd));
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
    use controller_protocol::{Command, CommandBody as CB, encode_command};

    // 先走一遍 discover，让 receiver 拿到 id=0
    let seq = ns.keyring.next_seq();
    let cmd = Command::with_key(seq, ns.keyring.active(), CB::Announce);
    ns.cmd_sig.signal(encode_command(&cmd));
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

use controller_protocol::ResponseBody;

static REPORT_HANDLER_INVOCATIONS: AtomicU32 = AtomicU32::new(0);
static REPORT_LAST_PERCENT: AtomicU32 = AtomicU32::new(0);
static REPORT_SAW_ANNOUNCE_REPLY: AtomicBool = AtomicBool::new(false);

#[test]
fn receiver_report_reaches_notifier() {
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
      use controller_protocol::{Command, CommandBody as CB, encode_command};
      let seq = ns.keyring.next_seq();
      let cmd = Command::with_key(seq, ns.keyring.active(), CB::Announce);
      ns.cmd_sig.signal(encode_command(&cmd));
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
    spawner
      .spawn_local(async move {
        run_broadcast_loop(a_send, ns_frame, ns_cmd, ns_resp).await;
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
        run_broadcast_loop(b_send, rs_frame, rs_cmd, rs_resp).await;
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
    use controller_protocol::{Command, CommandBody as CB, encode_command};
    let seq = ns_keyring.next_seq();
    let cmd = Command::with_key(seq, ns_keyring.active(), CB::Announce);
    ns_cmd.signal(encode_command(&cmd));
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

use controller_protocol::{CommandResponse, init_session_nonce, session_nonce};

#[test]
fn receiver_adopts_nonce_from_nonce_hello_broadcast() {
  const START_NONCE: u32 = 0x1111_1111;
  const BROADCAST_NONCE: u32 = 0x2222_2222;

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
