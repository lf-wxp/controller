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

use core::sync::atomic::{AtomicU8, Ordering};

use controller_protocol::{
  COMMAND_LEN, COMMAND_MAGIC, Command, CommandBody, CommandDecodeError, CommandResponse, FRAME_LEN,
  FRAME_MAGIC, KeyId, ResponseBody, decode_command, decode_frame,
};

use crate::keyring::Keyring;
use crate::notifier::signals::ResponseSignal;
use crate::receiver::{CommandHandler, CommandOutcome, CommandSource, FrameHandler, UNASSIGNED_ID};
use crate::replay::ReplayGuard;

/// 命令帧派发所需的静态上下文
///
/// # 生命周期
/// 全部字段都是 `&'static` 引用或 `Copy` 值；结构体自身 `Copy`，可以随意传递。
///
/// # `frame_handler`
/// `Option<FrameHandler>`：为 `None` 时 [`dispatch_frame`] 静默丢弃 Frame（零成本）；
/// 为 `Some` 时会做 `dest_mask` 过滤，仅当命中本机 `my_id` 或帧携带广播 mask
/// 时才投递给业务闭包。
#[derive(Clone, Copy)]
pub(crate) struct DispatchCtx {
  pub(crate) keyring: &'static Keyring,
  pub(crate) replay: &'static ReplayGuard,
  pub(crate) response_signal: &'static ResponseSignal,
  pub(crate) role_tag: [u8; 3],
  pub(crate) my_mac: [u8; 6],
  pub(crate) my_id: &'static AtomicU8,
  pub(crate) handler: CommandHandler,
  pub(crate) src: CommandSource,
  /// 可选的 Frame handler；`None` 表示本 endpoint 不消费入站 `Frame`
  pub(crate) frame_handler: Option<FrameHandler>,
}

/// 顶层派发入口：按 magic 分流到 Command / Frame 处理路径
///
/// # 匹配规则
/// - `data.len() == COMMAND_LEN` 且 magic == `COMMAND_MAGIC` → [`dispatch_command_frame`]
/// - `data.len() == FRAME_LEN` 且 magic == `FRAME_MAGIC` → [`dispatch_frame`]
/// - 其它：静默丢弃（未知 magic、长度不符、太短读不出 magic 都在此汇合）
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
    _ => {
      // 未知 magic 或长度不匹配：静默丢弃（可能是 Response 帧或对端 broadcast 回环）
      #[cfg(feature = "defmt")]
      defmt::trace!(
        "dispatch: drop packet (magic={=u16:04x}, len={=usize})",
        magic,
        data.len()
      );
    }
  }
}

/// 完整派发一条 Command 字节：解码 → Announce/AssignId 内置处理 → 业务命令派发
///
/// # 参数
/// - `data`：wire 字节；必须为 [`COMMAND_LEN`] 长度且以 [`COMMAND_MAGIC`] 开头
/// - `ctx`：静态上下文，见 [`DispatchCtx`]
///
/// # 静默丢弃场景
/// - 长度 / magic 不符
/// - HMAC / 版本 / 其它解码错误
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
/// 3. 或者本机已分配 id 且 [`Frame::is_addressed_to`](controller_protocol::Frame::is_addressed_to)
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
fn handle_builtin(cmd: &Command, ctx: &DispatchCtx) -> bool {
  match cmd.kind {
    CommandBody::Announce => {
      let resp = build_announce_reply(ctx.role_tag, ctx.my_mac, ctx.keyring.active());
      ctx.response_signal.signal(resp);
      true
    }
    CommandBody::AssignId { mac, receiver_id } => {
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
      ctx
        .response_signal
        .signal(CommandResponse::ack_with_key(cmd.seq, cmd.key_id));
    }
    CommandOutcome::Err(code) => {
      ctx
        .response_signal
        .signal(CommandResponse::err_with_key(cmd.seq, cmd.key_id, code));
    }
    CommandOutcome::Respond(resp) => {
      ctx.response_signal.signal(resp);
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
