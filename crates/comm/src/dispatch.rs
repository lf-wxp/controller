//! # 命令帧 / 广播帧派发共享逻辑
//!
//! `Notifier`（双身份模式）与 `Receiver` 在收到入站 wire 数据后都要走同一套流程：
//!
//! 1. **`Command` 帧**（`COMMAND_MAGIC`）：
//!    - `Announce` → 内置回一条 [`CommandResponse::AnnounceReply`]
//!    - `AssignId` → 若 mac 匹配则写入 `my_id`
//!    - 其它业务 Command → 抗重放校验 → 用户 handler → 自动 Ack / Err / Respond / NoReply
//! 2. **`Frame` 帧**（`FRAME_MAGIC`）：
//!    - 解码 → **dest_mask 过滤**（仅当自己被寻址时才投递）→ 用户 frame handler
//!    - `frame_handler = None` 时零成本静默丢弃
//!
//! 为避免"两处派发树各写一份"的维护风险，把这套逻辑抽到本模块——唯一的调用方
//! 是同一 crate 的 [`crate::notifier`] 与 [`crate::receiver`]，因此接口保持
//! `pub(crate)` 即可。
//!
//! # 参数打包
//! 为避免长参数列表触发 `clippy::too_many_arguments`，把不可变的运行时上下文
//! 打包成 [`DispatchCtx`]（`Copy`）传入。

use core::sync::atomic::Ordering;

use protocol::{
  COMMAND_LEN, COMMAND_MAGIC, Command, CommandBody, CommandDecodeError, CommandResponse, FRAME_LEN,
  FRAME_MAGIC, KeyId, RESPONSE_LEN, RESPONSE_MAGIC, ResponseBody, decode_command, decode_frame,
  init_session_nonce, peek_nonce_hello, session_nonce,
};

use crate::notifier::signals::enqueue_response;
use crate::receiver::{CommandOutcome, UNASSIGNED_ID};

/// 命令帧派发所需的静态上下文
///
/// 派发上下文与接收 loop 的公开配置**字段完全相同**（同为 `keyring / replay /
/// response_signal / role_tag / my_mac / my_id / handler / src / frame_handler`），
/// 因此二者合一：直接复用公开的 [`ReceiverRecvConfig`](crate::receiver::ReceiverRecvConfig)
/// 作为内部派发上下文，避免维护两份镜像结构体 + 一次逐字段搬运。
///
/// - `Receiver` 路径：`run_receive_loop` 把 `cfg` 原样传进来。
/// - 双身份 `Notifier` 路径：从 `CommandHandlerConfig` + 共享 static 现拼一个。
///
/// 全部字段都是 `&'static` 引用或 `Copy` 值；`Copy`，可随意传递。
pub(crate) use crate::receiver::ReceiverRecvConfig as DispatchCtx;

/// 顶层派发入口：按 magic 分流到 Command / Frame / Response 处理路径
///
/// # 匹配规则
/// - `data.len() == COMMAND_LEN` 且 magic == `COMMAND_MAGIC` → [`dispatch_command_frame`]
/// - `data.len() == FRAME_LEN` 且 magic == `FRAME_MAGIC` → [`dispatch_frame`]
/// - `data.len() == RESPONSE_LEN` 且 magic == `RESPONSE_MAGIC` → [`dispatch_response_frame`]
///   （仅消费 `NonceHello` 以同步 session nonce；其余 Response 变体静默丢弃）
/// - 其它：静默丢弃（未知 magic、长度不符、太短读不出 magic 都在此汇合）
///
/// # 谁调用本函数
/// 仅 [`crate::receiver::run_receive_loop`]（Endpoint 侧）。Coordinator 侧的
/// [`crate::notifier::run_receive_loop`] 有独立的 match（自己处理 Response 的
/// AnnounceReply upsert / AssignId），**不**走本函数——因此本函数对 `NonceHello`
/// 的采纳只作用于 Endpoint，Coordinator 作为 nonce 的**发布方**不会误采纳他人 nonce。
pub(crate) fn dispatch_packet(data: &[u8], ctx: DispatchCtx) {
  if data.len() < 2 {
    #[cfg(feature = "defmt")]
    defmt::trace!("dispatch: drop tiny packet ({=usize} bytes)", data.len());
    return;
  }
  let magic = u16::from_le_bytes([data[0], data[1]]);
  match magic {
    COMMAND_MAGIC if data.len() == COMMAND_LEN => dispatch_command_frame(data, ctx),
    FRAME_MAGIC if data.len() == FRAME_LEN => dispatch_frame(data, ctx),
    RESPONSE_MAGIC if data.len() == RESPONSE_LEN => dispatch_response_frame(data),
    _ => {
      // 未知 magic 或长度不匹配：静默丢弃（对端 broadcast 回环等）
      #[cfg(feature = "defmt")]
      defmt::trace!(
        "dispatch: drop packet (magic={=u16:04x}, len={=usize})",
        magic,
        data.len()
      );
    }
  }
}

/// 处理入站 Response —— **仅** 消费 `NonceHello` 以同步 session nonce（K3 bootstrap）
///
/// # 背景（跨设备 nonce 同步）
/// Coordinator 用 [`session_nonce`](protocol::session_nonce) 作为 HMAC
/// 前缀签发 Command，并周期广播 `NonceHello`。Endpoint 必须采纳同一个 nonce，
/// 才能：(1) 验签 Coordinator 下发的 Command（含 `Announce` / `AssignId`）；
/// (2) 用同一 nonce 签自己出站的 Response（`AnnounceReply` / `Ack`），让 Coordinator
/// 能验签通过。
///
/// # 为什么免鉴权读取
/// `NonceHello` 的 HMAC 以其自身携带的 nonce 为前缀，Endpoint 在拿到 nonce 前
/// 无法验签（鸡蛋悖论）。这里用 [`peek_nonce_hello`](protocol::peek_nonce_hello)
/// 只校验 magic/version/kind/CRC 后取出 nonce，安全权衡见该函数文档。
///
/// # 幂等
/// Coordinator 每 5s 重播同一 nonce；仅当与当前值不同才写入，避免无谓的
/// `Release` 存储。其余 Response 变体（`Ack` / `Error` / `BatterySnapshot` /
/// `AnnounceReply`）对 Endpoint 无意义，静默丢弃。
fn dispatch_response_frame(data: &[u8]) {
  let Some(nonce) = peek_nonce_hello(data) else {
    // 非 NonceHello（或损坏）的 Response：Endpoint 不消费，静默丢弃
    return;
  };
  if session_nonce() != nonce {
    init_session_nonce(nonce);
    #[cfg(feature = "defmt")]
    defmt::debug!(
      "dispatch: adopted session nonce 0x{=u32:08x} from NonceHello",
      nonce
    );
  }
}

/// 完整派发一条 Command 字节：解码 → Announce/AssignId 内置处理 → 业务命令派发
///
/// # 参数
/// - `data`：wire 字节；必须为 [`COMMAND_LEN`] 长度且以 [`COMMAND_MAGIC`] 开头
/// - `ctx`：静态上下文，见 [`DispatchCtx`]
///
/// # 派发顺序与抗重放的关系（**重要**）
/// [`handle_builtin`]（`Announce` / `AssignId`）在 [`dispatch_business_command`]
/// **之前**执行，而抗重放校验只在业务分支里。也就是说**内置命令有意绕过
/// anti-replay**，理由见 [`handle_builtin`] 的"抗重放豁免"章节——这不是疏漏，
/// 请勿把它"修正"成"内置命令也过 replay"，否则会破坏 Coordinator 重启后的
/// 重新发现（见该函数文档）。
///
/// # 静默丢弃场景
/// - 长度 / magic 不符
/// - HMAC / 版本 / 其它解码错误（含 `AssignId` 的 `receiver_id >= 32`——由
///   [`decode_command`] 直接判 `InvalidPayload`，故越界 id 到不了 [`handle_builtin`]）
/// - 业务命令抗重放校验失败
/// - handler 返回 [`CommandOutcome::NoReply`]
pub(crate) fn dispatch_command_frame(data: &[u8], ctx: DispatchCtx) {
  if data.len() != COMMAND_LEN {
    return;
  }
  let magic = u16::from_le_bytes([data[0], data[1]]);
  if magic != COMMAND_MAGIC {
    return;
  }
  let cmd = match decode_command(data) {
    Ok(c) => c,
    Err(CommandDecodeError::BadMagic | CommandDecodeError::BadLength) => {
      #[cfg(feature = "defmt")]
      defmt::trace!("dispatch_command: drop (bad magic/length)");
      return;
    }
    Err(_e) => {
      // HMAC / 版本 / 其它解码错误
      #[cfg(feature = "defmt")]
      defmt::warn!("dispatch_command: decode error {}", _e);
      return;
    }
  };

  if handle_builtin(&cmd, &ctx) {
    return;
  }

  dispatch_business_command(&cmd, &ctx);
}

/// 派发一条 Frame 字节：解码 → dest_mask 过滤 → 用户 frame_handler
///
/// # 过滤规则
/// 只有满足**任一**条件时才投递给业务闭包：
/// 1. `frame_handler` 已注册（否则整条路径零成本 short-circuit）
/// 2. 本机 `my_id == UNASSIGNED_ID`（尚未被 controller 分配 id）——此时**总是**
///    投递，让业务能观察到广播帧、通过其它渠道（比如 UI）快速判断链路联通性
/// 3. 或者本机已分配 id 且 [`Frame::is_addressed_to`](protocol::Frame::is_addressed_to)
///    返回 `true`（含 `dest_mask == u32::MAX` 的广播场景）
///
/// # 为什么"未分配 id 也投递"？
/// 若 `UNASSIGNED_ID` 期间就过滤掉所有 Frame，会产生"刚上电的 receiver 直到
/// 被 AssignId 前完全感知不到 controller 存在"的死区；这对调试与降级容错
/// 都不友好。业务侧若严格要求"分配 id 后才响应"，可以在 handler 里再判断
/// 一次 `my_id`。
pub(crate) fn dispatch_frame(data: &[u8], ctx: DispatchCtx) {
  // 零成本 short-circuit：未注册 frame_handler 时立刻返回，避免无谓 decode
  let Some(frame_handler) = ctx.frame_handler else {
    return;
  };

  let Ok(frame) = decode_frame(data) else {
    #[cfg(feature = "defmt")]
    defmt::trace!("dispatch_frame: drop (decode failed)");
    return;
  };

  let my_id = ctx.my_id.load(Ordering::Relaxed);
  let addressed = my_id == UNASSIGNED_ID || frame.is_addressed_to(my_id);
  if !addressed {
    #[cfg(feature = "defmt")]
    defmt::trace!(
      "dispatch_frame: drop (not addressed, my_id={=u8}, dest_mask={=u32:08x})",
      my_id,
      frame.dest_mask
    );
    return;
  }

  frame_handler(ctx.src, &frame);
}

/// 处理 Announce / AssignId 两类内置命令
///
/// # 返回
/// - `true`：本次是内置命令，调用方无需再进业务派发
/// - `false`：非内置命令，调用方应继续走 [`dispatch_business_command`]
///
/// # 抗重放豁免（**有意为之，勿改**）
/// 本函数在业务派发**之前**执行，且**不**调用 [`ReplayGuard::check`]，因此
/// `Announce` / `AssignId` 不受 anti-replay 约束。这是刻意的设计，原因是二者是
/// **会话自举（bootstrap）通道**，必须在"Endpoint 窗口已推进、但 Coordinator
/// seq 计数器刚被重置"时仍能工作：
///
/// - Endpoint 侧的 [`ReplayGuard`] 每收到一条可解码的**广播**命令就推进窗口
///   （[`dispatch_business_command`] 先 `check` 再交 handler，即便 handler
///   返回 `NoReply` 窗口也已前移）；Endpoint 通常**不持久化**该窗口。
/// - Coordinator 的出站 seq 计数器（[`Keyring`]）**不持久化**，重启后从 1 重来。
/// - 于是 Coordinator 重启后广播的 `Announce`（seq 很小）相对 Endpoint 那个已经
///   推到 last_seq=N 的窗口就是 `TooOld`。若让内置命令也过 replay，Endpoint 会
///   拒收 `Announce` → 永不再回 `AnnounceReply` → **重新发现彻底失效**。
///
/// # 残余风险与为何可接受
/// 豁免意味着攻击者可以**抓包重放**一条曾经合法的 `AssignId`（HMAC 仍拦住
/// **伪造**，只是拦不住**重放**）。但影响有限：
/// - `AssignId` 对**稳定的** registry 是幂等的（同一 MAC → 同一 `receiver_id`）。
/// - 最坏情况是把某台 Endpoint 的 `my_id` 冲回一个旧值，造成短暂的 `dest_mask`
///   寻址错位；下一轮 discover 会用当前映射重发 `AssignId` **自愈**。
///
/// 需要"内置命令也抗重放"的部署（例如 Endpoint 持久化窗口 + Coordinator
/// 持久化 seq 计数器）应在**应用层**另建会话/epoch 机制，而不是删掉这里的豁免。
///
/// # `receiver_id` 范围
/// 无需在此再校验 `receiver_id < 32`：[`decode_command`] 解码 `AssignId` 时已把
/// `receiver_id >= 32` 判为 `InvalidPayload` 并丢弃，越界 id 到不了这里；
/// [`Frame::is_addressed_to`](protocol::Frame::is_addressed_to) 也对越界 id 恒返回
/// `false`，构成第二道兜底。
fn handle_builtin(cmd: &Command, ctx: &DispatchCtx) -> bool {
  match cmd.kind {
    CommandBody::Announce => {
      let resp = build_announce_reply(ctx.role_tag, ctx.my_mac, ctx.keyring.active());
      enqueue_response(ctx.response_signal, resp);
      true
    }
    CommandBody::AssignId { mac, receiver_id } => {
      // `receiver_id` 已由 `decode_command` 保证 < 32（否则解码即失败），此处直接写入。
      if mac == ctx.my_mac {
        ctx.my_id.store(receiver_id, Ordering::Relaxed);
      }
      true
    }
    _ => false,
  }
}

/// 业务 Command 派发：抗重放 → handler → 自动回执
fn dispatch_business_command(cmd: &Command, ctx: &DispatchCtx) {
  if ctx.replay.check(cmd.key_id, cmd.seq).is_err() {
    #[cfg(feature = "defmt")]
    defmt::warn!(
      "dispatch: replay rejected (key_id={}, seq={=u32})",
      cmd.key_id,
      cmd.seq
    );
    return;
  }
  match (ctx.handler)(ctx.src, cmd) {
    CommandOutcome::Ok => {
      enqueue_response(
        ctx.response_signal,
        CommandResponse::ack_with_key(cmd.seq, cmd.key_id),
      );
    }
    CommandOutcome::Err(code) => {
      enqueue_response(
        ctx.response_signal,
        CommandResponse::err_with_key(cmd.seq, cmd.key_id, code),
      );
    }
    CommandOutcome::Respond(resp) => {
      enqueue_response(ctx.response_signal, resp);
    }
    CommandOutcome::NoReply => {}
  }
}

/// 构造一个 AnnounceReply（`req_seq = 0`，RSSI 未知）
///
/// 与手柄原代码保持一致：不携带 RSSI（用哨兵值 `i8::MIN`），Host 侧渲染时应
/// 判断此哨兵并显示为"未知"。
pub(crate) fn build_announce_reply(
  role_tag: [u8; 3],
  my_mac: [u8; 6],
  key_id: KeyId,
) -> CommandResponse {
  CommandResponse {
    req_seq: 0,
    key_id,
    body: ResponseBody::AnnounceReply {
      mac: my_mac,
      rssi_dbm: i8::MIN,
      role_tag,
    },
  }
}
