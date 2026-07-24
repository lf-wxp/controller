//! # LoopbackLink —— 内存 mpsc 实现的 [`CommLink`]
//!
//! 用于 **host 端集成测试** 和 **样例演示**：让 notifier 和 receiver 通过内存
//! channel 通信，无需真实硬件。
//!
//! ## 拓扑
//! ```text
//!  ┌───────────────────────────────┐        ┌───────────────────────────────┐
//!  │ side_a (mac_a)                │        │ side_b (mac_b)                │
//!  │   ┌ tx_ab ───► rx_ab ┐        │        │        ┌ rx_ab                │
//!  │   │                  │        │        │        │                      │
//!  │   └────────  A sends │        │        │ B recvs└──                    │
//!  │        ┌ rx_ba ◄──── tx_ba ─┐          │              tx_ba ◄──┐       │
//!  │   A recvs   ──── ┘          │          │              B sends──┘       │
//!  └───────────────────────────────┘        └───────────────────────────────┘
//! ```
//!
//! 每一"端"再被拆成 **一对 send/recv endpoint**，因为 [`CommLink`] 的
//! `send` 与 `recv` 都要 `&mut self`，无法在两个 task 之间共享一个 endpoint。
//! [`pair`] 返回 4 个 endpoint：
//! `(a_send, a_recv, b_send, b_recv)`。
//!
//! ## 只在 host 编译
//! 靠 `std::sync::mpsc` 实现；`#[cfg(feature = "loopback")]` 门控。

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use crate::link::{CommLink, Packet};

/// mpsc 传递的 wire payload：`(src_mac, bytes)`
///
/// 抽这个 type alias 一为了可读（开头注释满屏的 `([u8; 6], Vec<u8>)`
/// 很难一眼看出语义），二为了避开 clippy::type_complexity（
/// `Arc<Mutex<Receiver<(...)>>>` 嵌套 4 层会被判定为“过于复杂”）。
type WirePayload = ([u8; 6], std::vec::Vec<u8>);

/// LoopbackLink 一端的错误类型
#[derive(Debug)]
pub enum LoopbackError {
  /// 对端 channel 已关闭
  Disconnected,
}

// ============================================================
// send-only endpoint
// ============================================================

/// 只用于 `send` 的 LoopbackLink 一端
///
/// `recv()` 永远返回 `Disconnected`；因此该 endpoint **不能**塞进
/// `run_receive_loop`，只能塞进 `run_broadcast_loop`。
pub struct LoopbackSendEnd {
  my_mac: [u8; 6],
  tx: Sender<WirePayload>,
}

impl CommLink for LoopbackSendEnd {
  const MAX_FRAME_LEN: usize = 64;
  type SendError = LoopbackError;
  type RecvError = LoopbackError;
  type Addr = [u8; 6];
  const BROADCAST: Self::Addr = [0xFF; 6];

  async fn send(&mut self, _dst: Self::Addr, bytes: &[u8]) -> Result<(), Self::SendError> {
    self
      .tx
      .send((self.my_mac, bytes.to_vec()))
      .map_err(|_| LoopbackError::Disconnected)
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    // send-only：直接报错让上层 loop 静默进入下一轮
    Err(LoopbackError::Disconnected)
  }
}

// ============================================================
// recv-only endpoint
// ============================================================

/// 只用于 `recv` 的 LoopbackLink 一端
///
/// `send()` 永远返回 `Disconnected`；因此该 endpoint **不能**塞进
/// `run_broadcast_loop`，只能塞进 `run_receive_loop`。
pub struct LoopbackRecvEnd {
  rx: Arc<Mutex<Receiver<WirePayload>>>,
  scratch: std::vec::Vec<u8>,
  scratch_src: [u8; 6],
}

impl CommLink for LoopbackRecvEnd {
  const MAX_FRAME_LEN: usize = 64;
  type SendError = LoopbackError;
  type RecvError = LoopbackError;
  type Addr = [u8; 6];
  const BROADCAST: Self::Addr = [0xFF; 6];

  async fn send(&mut self, _dst: Self::Addr, _bytes: &[u8]) -> Result<(), Self::SendError> {
    // recv-only：直接报错
    Err(LoopbackError::Disconnected)
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    // 关键：std mpsc 的 `recv` 是阻塞式的，无法与 embassy async 无缝配合；
    // 但 host 集成测试里我们用 futures_executor 单线程 block_on，这里 poll 的
    // 语义是"给 executor 让位"—— 我们退而求其次用 try_recv + spin 让 loop
    // 让出控制权。用 embassy_futures::yield_now 保证不阻塞其它任务。
    loop {
      let try_result = {
        // `loopback` 仅在 host 集成测试启用；Mutex poisoned 等价于另一测试
        // 线程已 panic —— 直接重招传播可提醒开发者，无需自己处理。
        let rx = self.rx.lock().expect("loopback mutex not poisoned");
        rx.try_recv()
      };
      match try_result {
        Ok((src, bytes)) => {
          self.scratch = bytes;
          self.scratch_src = src;
          return Ok(Packet {
            src: self.scratch_src,
            data: &self.scratch,
          });
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {
          embassy_futures::yield_now().await;
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
          return Err(LoopbackError::Disconnected);
        }
      }
    }
  }
}

// ============================================================
// pair 工厂
// ============================================================

/// 创建 4 个互联的 endpoint：`(a_send, a_recv, b_send, b_recv)`
///
/// - `a_send` / `a_recv`：Notifier 端；`a_send` 塞进 broadcast_loop，
///   `a_recv` 塞进 receive_loop
/// - `b_send` / `b_recv`：Receiver 端；同理
///
/// 语义：`a_send → b_recv`，`b_send → a_recv`；广播地址不影响路由（点对点回环）。
#[must_use]
pub fn pair(
  mac_a: [u8; 6],
  mac_b: [u8; 6],
) -> (
  LoopbackSendEnd,
  LoopbackRecvEnd,
  LoopbackSendEnd,
  LoopbackRecvEnd,
) {
  let (tx_ab, rx_ab) = channel();
  let (tx_ba, rx_ba) = channel();
  let a_send = LoopbackSendEnd {
    my_mac: mac_a,
    tx: tx_ab, // A 写入 → B 读取
  };
  let a_recv = LoopbackRecvEnd {
    rx: Arc::new(Mutex::new(rx_ba)), // A 读取 ← B 写入
    scratch: std::vec::Vec::with_capacity(64),
    scratch_src: [0; 6],
  };
  let b_send = LoopbackSendEnd {
    my_mac: mac_b,
    tx: tx_ba,
  };
  let b_recv = LoopbackRecvEnd {
    rx: Arc::new(Mutex::new(rx_ab)),
    scratch: std::vec::Vec::with_capacity(64),
    scratch_src: [0; 6],
  };
  (a_send, a_recv, b_send, b_recv)
}
