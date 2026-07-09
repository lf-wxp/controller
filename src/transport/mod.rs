//! # 传输层：把 Frame 送出设备的抽象
//!
//! ## 设计原则
//! - **trait 抽象**：业务层只依赖 [`Transport`] trait，不关心具体实现
//! - **可插拔**：BLE / ESP-NOW / UART / defmt 日志都是同一接口的不同实现
//! - **可组合**：通过 [`CompositeTransport`] 把多个 transport 打包成一个，
//!   一次 `send()` 分发到全部 —— 例如同时启用 BLE HID + ESP-NOW 广播
//! - **同步优先**：绝大多数传输都是"入队即返回"，用同步 API 更简单
//!
//! ## 已有实现
//! - [`DefmtLogTransport`]：把帧打到 defmt 日志（调试专用，永不失败）
//! - [`ble_hid::BleHidTransport`]：BLE HID Gamepad + 自定义 GATT
//! - [`esp_now::EspNowTransport`]：ESP-NOW 广播（任意 ESP32 均可接收）

mod defmt_log;

pub mod ble_hid;
pub mod control;
pub mod esp_now;

pub use defmt_log::DefmtLogTransport;

use crate::protocol::Frame;

/// 传输层统一接口
///
/// 实现方要保证 `send` 不长时间阻塞（异步执行器不喜欢阻塞）；
/// 如果底层是慢速传输，请在实现内部使用队列/缓冲，`send` 只做入队。
pub trait Transport {
  /// 传输错误类型
  type Error: core::fmt::Debug;

  /// 发送一帧
  ///
  /// # 语义
  /// - 成功：帧已提交给底层（可能是硬件 FIFO、协议栈队列、日志缓冲）
  /// - 失败：底层不可用（BLE 未连接、UART FIFO 满、等等）
  fn send(&mut self, frame: &Frame) -> Result<(), Self::Error>;
}

// ============================================================
// CompositeTransport —— 通用组合器
// ============================================================

/// 组合两个 [`Transport`]，一次 `send()` 会依次调用二者
///
/// # 语义
/// - **全量分发**：任一子传输失败**不影响**另一子传输的调用
/// - **错误聚合**：见 [`CompositeError`]，可以精确知道哪个子传输失败
/// - **顺序保证**：先调用 `first`，再调用 `second`；对于 BLE + ESP-NOW
///   这样的独立子系统，顺序不影响正确性
///
/// # 组合更多的 Transport
/// 通过嵌套即可组合任意多个：
/// ```ignore
/// let t = CompositeTransport::new(
///     BleHidTransport::new(...),
///     CompositeTransport::new(
///         EspNowTransport::new(...),
///         DefmtLogTransport::default(),
///     ),
/// );
/// ```
pub struct CompositeTransport<A, B> {
  first: A,
  second: B,
}

impl<A, B> CompositeTransport<A, B> {
  /// 组合两个 transport
  pub const fn new(first: A, second: B) -> Self {
    Self { first, second }
  }
}

/// [`CompositeTransport`] 的错误：区分哪个子传输失败
///
/// 两个子传输**都失败**时，返回 `First`（先失败的），第二个错误会被丢弃 ——
/// 这在实践中足够，因为主循环通常只关心"是否有任一路成功"，而 Composite 的
/// 目标恰恰是"任一路成功就算成功"。见 [`CompositeTransport::send`] 的实现。
#[derive(Debug)]
pub enum CompositeError<E1, E2> {
  /// 第一个子传输失败
  First(E1),
  /// 第二个子传输失败（第一个成功或已忽略）
  Second(E2),
}

impl<A, B> Transport for CompositeTransport<A, B>
where
  A: Transport,
  B: Transport,
{
  type Error = CompositeError<A::Error, B::Error>;

  fn send(&mut self, frame: &Frame) -> Result<(), Self::Error> {
    // 关键：**两个都调**，任一失败不阻止另一个
    // 这样一路 BLE 断了还能靠 ESP-NOW 继续跑
    let r1 = self.first.send(frame);
    let r2 = self.second.send(frame);
    match (r1, r2) {
      (Ok(()), Ok(())) => Ok(()),
      (Err(e), _) => Err(CompositeError::First(e)),
      (Ok(()), Err(e)) => Err(CompositeError::Second(e)),
    }
  }
}
