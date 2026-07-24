//! # `comm::metrics` —— 出站有界队列的丢弃可观测性
//!
//! ## 背景
//! Command / Response 走**深度 [`OUTBOUND_QUEUE_DEPTH`] 的有界 FIFO**
//! （[`CommandOutChannel`] / [`ResponseChannel`]）。生产者一律 `try_send`
//! 非阻塞入队——**队列满时丢弃当前这条**（fire-and-forget，不阻塞主循环）。
//! 丢弃默认是**静默**的，长时间跑很难发现"其实一直在丢包"。
//!
//! 本模块用两枚**进程级 `AtomicU32`** 把丢弃次数暴露出来，供上层做健康度巡检
//! （打日志 / 上报 metrics / 点灯告警）。计数是**全局聚合**：同进程内多个
//! `Notifier` / `Receiver` 实例的丢弃会累加到同一枚计数器——嵌入式部署里
//! 单进程通常只有一套 comm 拓扑，聚合足够；需要 per-instance 精度的话请在自己
//! crate 里包一层带计数的 `Channel`。
//!
//! ## 开销
//! 只在**真正丢弃**（`try_send` 返回 `Err`）时 `fetch_add(Relaxed)`，正常路径
//! 零额外开销；读取 / reset 同为 `Relaxed`，`no_std` 友好、无锁。
//!
//! [`OUTBOUND_QUEUE_DEPTH`]: crate::notifier::signals::OUTBOUND_QUEUE_DEPTH
//! [`CommandOutChannel`]: crate::notifier::signals::CommandOutChannel
//! [`ResponseChannel`]: crate::notifier::signals::ResponseChannel

use core::sync::atomic::{AtomicU32, Ordering};

static DROPPED_COMMANDS: AtomicU32 = AtomicU32::new(0);
static DROPPED_RESPONSES: AtomicU32 = AtomicU32::new(0);

/// 累计被丢弃的**出站 Command** 条数（队列满导致 `try_send` 失败）
#[must_use]
pub fn dropped_commands() -> u32 {
  DROPPED_COMMANDS.load(Ordering::Relaxed)
}

/// 累计被丢弃的**出站 Response** 条数（队列满导致 `try_send` 失败）
#[must_use]
pub fn dropped_responses() -> u32 {
  DROPPED_RESPONSES.load(Ordering::Relaxed)
}

/// 一次性读取两枚计数器的快照
#[must_use]
pub fn snapshot() -> DropCounts {
  DropCounts {
    commands: dropped_commands(),
    responses: dropped_responses(),
  }
}

/// 把两枚计数器清零（一般用于巡检窗口切换 / 测试隔离）
pub fn reset() {
  DROPPED_COMMANDS.store(0, Ordering::Relaxed);
  DROPPED_RESPONSES.store(0, Ordering::Relaxed);
}

/// [`snapshot`] 的返回值：某一刻的累计丢弃计数
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropCounts {
  /// 累计丢弃的出站 Command 条数
  pub commands: u32,
  /// 累计丢弃的出站 Response 条数
  pub responses: u32,
}

impl DropCounts {
  /// 两枚计数是否都为 0（无任何丢弃）
  #[must_use]
  pub const fn is_clean(&self) -> bool {
    self.commands == 0 && self.responses == 0
  }
}

/// 记录一次 Command 丢弃（crate 内部，由 `enqueue_command` 调用）
pub(crate) fn record_dropped_command() {
  DROPPED_COMMANDS.fetch_add(1, Ordering::Relaxed);
}

/// 记录一次 Response 丢弃（crate 内部，由 `enqueue_response` 调用）
pub(crate) fn record_dropped_response() {
  DROPPED_RESPONSES.fetch_add(1, Ordering::Relaxed);
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
  use super::*;
  use crate::notifier::signals::{
    CommandOutChannel, OUTBOUND_QUEUE_DEPTH, OutboundCommand, ResponseChannel, enqueue_command,
    enqueue_response,
  };
  use protocol::{COMMAND_LEN, CommandResponse};

  /// 队列填满后，`enqueue_*` 的丢弃应被计入对应计数器。
  ///
  /// # 为什么合并成一个 test
  /// 两枚计数器是**进程级全局**。合并进单个 test 顺序执行，避免与其它并行
  /// 单测抢占计数器导致 delta 断言抖动；断言用 before/after 差值而非绝对值，
  /// 进一步隔离并发影响（本 crate 其它单测不会把有界队列塞满，故不产生丢弃）。
  #[test]
  fn enqueue_helpers_count_drops_on_full_queue() {
    // ---- Command 通路 ----
    static CMD_CHAN: CommandOutChannel = CommandOutChannel::new();
    let before_cmd = dropped_commands();
    let cmd = OutboundCommand::broadcast([0u8; COMMAND_LEN]);
    let cmd_overflow = 3_u32;
    // 前 DEPTH 条成功入队，其后每条都因队列满被丢弃并计数
    for _ in 0..(OUTBOUND_QUEUE_DEPTH as u32 + cmd_overflow) {
      enqueue_command(&CMD_CHAN, cmd);
    }
    assert_eq!(
      dropped_commands() - before_cmd,
      cmd_overflow,
      "队列满后每条 Command 丢弃都应计数"
    );

    // ---- Response 通路 ----
    static RESP_CHAN: ResponseChannel = ResponseChannel::new();
    let before_resp = dropped_responses();
    let resp = CommandResponse::nonce_hello(1);
    let resp_overflow = 2_u32;
    for _ in 0..(OUTBOUND_QUEUE_DEPTH as u32 + resp_overflow) {
      enqueue_response(&RESP_CHAN, resp);
    }
    assert_eq!(
      dropped_responses() - before_resp,
      resp_overflow,
      "队列满后每条 Response 丢弃都应计数"
    );

    // ---- snapshot 与 getter 一致 ----
    let snap = snapshot();
    assert_eq!(snap.commands, dropped_commands());
    assert_eq!(snap.responses, dropped_responses());
    // 至少发生过本 test 制造的丢弃，故一定不 clean
    assert!(!snap.is_clean());
  }
}
