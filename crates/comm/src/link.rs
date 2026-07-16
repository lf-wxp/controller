//! # `CommLink` —— 唯一的物理链路抽象
//!
//! 本 crate 唯一暴露给硬件层的 trait。任何"能发广播帧 + 能收广播帧"的
//! 物理链路都可以实现它，然后交给 [`Notifier`](crate::Notifier) /
//! [`Receiver`](crate::Receiver) 编排使用：
//!
//! - ESP-NOW（`esp-radio::EspNowSender/Receiver`）
//! - UART / 串口
//! - TCP 广播（服务器场景）
//! - 内存 mpsc（[`loopback`](crate::loopback) feature，用于 host 端测试）
//!
//! ## 设计原则
//! - **最小接口面**：只有 `send` / `recv` 两个方法（`anti-over-abstraction`）
//! - **零拷贝入站**：`recv` 借用 impl 内部缓冲，避免 heap 分配
//! - **async fn in trait**：Rust 1.75+ 稳定；embassy 0.10 已可用
//! - **广播地址常量**：由 impl 侧提供，`Notifier` / `Receiver` 直接引用

use core::future::Future;

/// 物理链路错误（分发送 / 接收两向）
///
/// 泛型让不同实现携带自己的错误类型；上层的 [`NotifierError`](crate::NotifierError) /
/// [`ReceiverError`](crate::ReceiverError) 会把它们再包一层。
#[derive(Debug)]
pub enum LinkError<S, R> {
  /// 发送失败
  Send(S),
  /// 接收失败
  Recv(R),
}

#[cfg(feature = "defmt")]
impl<S: defmt::Format, R: defmt::Format> defmt::Format for LinkError<S, R> {
  fn format(&self, f: defmt::Formatter<'_>) {
    match self {
      Self::Send(e) => defmt::write!(f, "LinkError::Send({})", e),
      Self::Recv(e) => defmt::write!(f, "LinkError::Recv({})", e),
    }
  }
}

/// 收到的一帧
///
/// # 字段
/// - `src`：源地址（比如 ESP-NOW 的 MAC-48）
/// - `data`：帧字节，借用 impl 内部缓冲；下一次 `recv()` 前有效
///
/// # 生命周期
/// 显式生命周期参数让借用关系一目了然：数据活多久等价于 impl 内部缓冲活多久。
#[derive(Debug)]
pub struct Packet<'a, A> {
  /// 源地址
  pub src: A,
  /// 帧字节内容
  pub data: &'a [u8],
}

/// 一条"可发广播 / 可收广播"的双向物理链路
///
/// # 实现者约定
/// - `send` 应该"入队即返回"；如果底层慢，请在实现内部走队列，避免长时间阻塞
///   `.await`（embassy 单线程执行器不喜欢阻塞）
/// - `recv` 的返回切片指向 impl 内部缓冲；下一次 `recv()` 调用后切片失效 ——
///   调用方（[`Notifier`](crate::Notifier) / [`Receiver`](crate::Receiver)）保证不跨
///   `.await` 持有该切片
/// - `BROADCAST` 用于广播；单播的话由 impl 侧决定是否支持（`Addr` 类型自选）
///
/// # 为什么用 `impl Future` 而不是 `async fn`
/// - `async fn` in trait 从 rust 1.75 起稳定；但当前仓库 rustc 1.88 已支持
/// - 显式 `impl Future` 让借用生命周期更清晰，方便实现方定制
pub trait CommLink {
  /// 单帧最大字节数（用于内部缓冲 sizing）
  const MAX_FRAME_LEN: usize;

  /// 发送错误类型
  type SendError: core::fmt::Debug;
  /// 接收错误类型
  type RecvError: core::fmt::Debug;
  /// 地址类型（ESP-NOW: `[u8; 6]`；loopback: `()`；TCP: `SocketAddr`…）
  type Addr: Copy + core::fmt::Debug + PartialEq + Eq;

  /// 广播地址（发到本网段所有能听到的接收端）
  const BROADCAST: Self::Addr;

  /// 向指定地址发送字节
  ///
  /// # 参数
  /// - `dst`：目标地址；[`Self::BROADCAST`] 表示广播
  /// - `bytes`：已经由 comm crate 编码好的 wire 字节
  fn send(
    &mut self,
    dst: Self::Addr,
    bytes: &[u8],
  ) -> impl Future<Output = Result<(), Self::SendError>>;

  /// 阻塞式接收一帧
  ///
  /// # 语义
  /// - 永不返回空帧；有数据来才唤醒
  /// - 返回的 `Packet::data` 借用 impl 内部缓冲；调用方在下一次 `recv` 前
  ///   完成拷贝或转手到别处
  fn recv(&mut self) -> impl Future<Output = Result<Packet<'_, Self::Addr>, Self::RecvError>>;
}

// ============================================================
// DummyLink —— 仅测试使用
// ============================================================

/// 仅测试可见的空 `CommLink` 实现
///
/// # 用途
/// 让 [`crate::receiver::test_receiver_from_parts`] 能在不真正持有物理链路
/// 的前提下构造一个 [`crate::Receiver`] 实体，专门用于验证其
/// `&self` 方法（`report` / `send_frame` / `send_command`）。
///
/// # 语义
/// - `send` 永远返回 `Ok(())`（吞掉数据）
/// - `recv` 永远 pending（返回一个不完成的 future）——因此**严禁**把持有
///   `DummyLink` 的实体 spawn 进 `run_receive_loop`
///
/// # feature 门控
/// 仅在 `test-utils` 开启时可见。生产代码不允许依赖本类型。
#[cfg(feature = "test-utils")]
#[derive(Debug, Clone, Copy)]
pub struct DummyLink;

#[cfg(feature = "test-utils")]
impl CommLink for DummyLink {
  const MAX_FRAME_LEN: usize = 64;
  type SendError = core::convert::Infallible;
  type RecvError = core::convert::Infallible;
  type Addr = ();
  const BROADCAST: Self::Addr = ();

  async fn send(&mut self, _dst: Self::Addr, _bytes: &[u8]) -> Result<(), Self::SendError> {
    Ok(())
  }

  async fn recv(&mut self) -> Result<Packet<'_, Self::Addr>, Self::RecvError> {
    // 永远 pending —— 用 embassy_futures::yield_now 循环让位，不真返回
    loop {
      embassy_futures::yield_now().await;
    }
  }
}
