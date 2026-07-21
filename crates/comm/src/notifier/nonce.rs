//! # Nonce 广播模块（K3 通用能力）
//!
//! ## 作用
//! 把"生成一次性 seed → 装入 protocol crate 的 `SESSION_NONCE` →
//! 周期性用 [`ResponseBody::NonceHello`] 广播"这条链路完整封装到 comm crate
//! 里，让所有想复用 notifier 能力的项目**引入 crate 即可开箱即用**，
//! 无需自己在应用层写：
//!
//! - 一份 seed 生成函数（原来在 `crates/controller/src/hal/rng.rs`）
//! - 一份 `#[embassy_executor::task]` 定时广播任务（原来在
//!   `crates/controller/src/transport/esp_now/mod.rs`）
//! - 一处 spawn + 一处 init_session_nonce 调用（原来分散在 `main.rs`）
//!
//! ## 使用体验
//!
//! 用户在自己 crate 里只需要：
//!
//! 1. 提供一个实现 [`EntropySource`] 的对象（比如 `esp_hal::rng::Rng`
//!    的封装，见 [`SimpleEntropy`]）
//! 2. 调用 [`init_session`] 一次
//! 3. 在自己的 `#[embassy_executor::task]` 中调用 [`run_nonce_broadcast_loop`]
//!
//! ```ignore
//! use comm::notifier::signals::ResponseSignal;
//! use comm::notifier::nonce::{
//!     init_session, run_nonce_broadcast_loop, DEFAULT_NONCE_BROADCAST_INTERVAL,
//! };
//!
//! static RESP_SIG: ResponseSignal = ResponseSignal::new();
//!
//! #[embassy_executor::task]
//! async fn nonce_task() -> ! {
//!     run_nonce_broadcast_loop(&RESP_SIG, DEFAULT_NONCE_BROADCAST_INTERVAL).await
//! }
//!
//! // 启动阶段：
//! let mut entropy = MyEntropy::new();     // 用户自定义（比如包一层 esp_hal::rng::Rng）
//! init_session(&mut entropy);
//! spawner.spawn(nonce_task()).unwrap();
//! ```
//!
//! ## 与协议层的关系
//! - 协议层 (`protocol::auth`) 只提供**存储**（[`SESSION_NONCE`]）
//!   与**读写 API**（[`init_session_nonce`] / [`session_nonce`]）
//! - 本模块只做**编排**（"什么时候写"、"什么时候广播"）
//! - 广播报文的构造仍然复用 [`CommandResponse::nonce_hello`]，避免协议漂移
//!
//! [`SESSION_NONCE`]: protocol::SESSION_NONCE
//! [`init_session_nonce`]: protocol::init_session_nonce
//! [`session_nonce`]: protocol::session_nonce
//! [`ResponseBody::NonceHello`]: protocol::ResponseBody::NonceHello

use embassy_time::{Duration, Timer};
use protocol::{CommandResponse, init_session_nonce, session_nonce};

use super::signals::ResponseSignal;

// ============================================================
// 常量
// ============================================================

/// 默认 nonce 广播周期
///
/// **5 秒**是一个"够快让 Host 上线后 <= 5s 感知 nonce、够慢不至于挤占
/// Ack 流量"的经验值。若应用有特殊需求可调用 [`run_nonce_broadcast_loop`]
/// 时自己传 `Duration`。
pub const DEFAULT_NONCE_BROADCAST_INTERVAL: Duration = Duration::from_millis(5_000);

// ============================================================
// EntropySource trait —— 唯一的可插拔点
// ============================================================

/// 一次性熵源抽象（用于 [`init_session`]）
///
/// # 为什么不直接用 `rand_core::RngCore`?
/// - `rand_core` 会把整个 rand 生态拽进来，与 no_std / 手柄的极简依赖策略冲突
/// - 我们只需要**一次** 32-bit 熵；简化到 `read_u32(&mut self) -> u32` 就够
/// - 各平台自定义实现极其轻量：
///   - ESP32：包一层 `esp_hal::rng::Rng::random()`
///   - host 测试：给一个固定/伪随机 impl
///   - RP2040/STM32：包一层 HAL 自带的 RNG 外设
///
/// # 实现建议
/// 若熵源本身可能弱（比如未初始化 Wi-Fi 前的 esp32 PRNG），推荐在 impl 内部
/// 与其它抖动源（`Instant::now()` 低位、未初始化 SRAM 等）XOR 混合，见
/// 手柄 crate 里 `hal::rng::SimpleEntropy` 的示例。
pub trait EntropySource {
  /// 读取一个 32-bit 熵值
  ///
  /// # 契约
  /// - 允许不加密安全强度（本 crate 只用作 nonce seed，非 CSPRNG）
  /// - **不允许**每次都返回 0；否则 [`init_session_nonce`] 内部的哨兵值
  ///   保护会把结果强制拉高到 1，跨设备 nonce 全部撞车
  fn read_u32(&mut self) -> u32;
}

// ============================================================
// init_session —— 启动阶段一次性初始化
// ============================================================

/// 从任意 [`EntropySource`] 采一次熵，装入 [`SESSION_NONCE`]
///
/// **必须**在 spawn 任何依赖 HMAC 的 task 之前调用一次；重复调用会覆盖旧值
/// （出于安全考虑，应用侧应保证只调一次）。
///
/// # 参数
/// - `entropy`：任意 [`EntropySource`] 实现；采样一次后可丢弃
///
/// # 返回
/// 实际写入的 nonce 值（便于日志打印）
///
/// [`SESSION_NONCE`]: protocol::SESSION_NONCE
pub fn init_session<E: EntropySource + ?Sized>(entropy: &mut E) -> u32 {
  let seed = entropy.read_u32();
  init_session_nonce(seed);
  session_nonce()
}

// ============================================================
// run_nonce_broadcast_loop —— 后台任务主体
// ============================================================

/// 周期性把 [`ResponseBody::NonceHello`] 塞入 [`ResponseSignal`]
///
/// 用户在自己 crate 里包一层 `#[embassy_executor::task]`（embassy 的
/// task 宏禁止泛型 async fn，所以本 crate 无法直接标注）：
///
/// ```ignore
/// #[embassy_executor::task]
/// async fn nonce_task() -> ! {
///     comm::notifier::nonce::run_nonce_broadcast_loop(
///         &RESP_SIG,
///         comm::notifier::nonce::DEFAULT_NONCE_BROADCAST_INTERVAL,
///     ).await
/// }
/// ```
///
/// # 广播路径
/// `Signal` 是覆盖式；notifier 的 `broadcast_loop` 会消费这个 Signal
/// 并通过 `CommLink::send(BROADCAST, ...)` 发出。若前一条 NonceHello 或
/// Ack 还没被消费，会被最新的 NonceHello 覆盖 —— 无副作用，下一轮 5s
/// 后还会再发一次，Host 侧最终能拿到最新 nonce。
///
/// # 首次触发
/// 函数入口先立刻广播一次（避免首个 `Timer::after` 造成 5s 静默）；此后
/// 严格按 `interval` 周期广播。
///
/// # 参数
/// - `resp`：`&'static ResponseSignal`，由用户在应用 crate 里 `static`
///   声明后传入
/// - `interval`：广播周期；日常用 [`DEFAULT_NONCE_BROADCAST_INTERVAL`]
///
/// [`ResponseBody::NonceHello`]: protocol::ResponseBody::NonceHello
pub async fn run_nonce_broadcast_loop(resp: &'static ResponseSignal, interval: Duration) -> ! {
  // 首次立即广播一次
  broadcast_once(resp);
  loop {
    Timer::after(interval).await;
    broadcast_once(resp);
  }
}

/// 读取当前 [`session_nonce`] 并塞入 [`ResponseSignal`]
///
/// 拆成独立 `#[inline]` 函数便于未来加日志钩子 / 单元测试。
#[inline]
fn broadcast_once(resp: &'static ResponseSignal) {
  let nonce = session_nonce();
  resp.signal(CommandResponse::nonce_hello(nonce));
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
  use super::*;

  /// 简单可预测熵源，仅用于测试
  struct FixedEntropy(u32);

  impl EntropySource for FixedEntropy {
    fn read_u32(&mut self) -> u32 {
      self.0
    }
  }

  #[test]
  fn init_session_writes_nonce() {
    let mut entropy = FixedEntropy(0xDEAD_BEEF);
    let written = init_session(&mut entropy);
    assert_eq!(written, 0xDEAD_BEEF);
    assert_eq!(session_nonce(), 0xDEAD_BEEF);
  }

  #[test]
  fn init_session_zero_seed_avoids_sentinel() {
    // 0 是 SESSION_NONCE 的"未初始化哨兵"；protocol 层会强制拉高到 1
    let mut entropy = FixedEntropy(0);
    let written = init_session(&mut entropy);
    assert_ne!(written, 0);
  }

  #[test]
  fn broadcast_once_signals_current_nonce() {
    // 先固定一个 nonce
    init_session_nonce(0x1234_5678);
    // 使用 static ResponseSignal 拿 &'static —— Signal::new 是 const fn，
    // 不会累积 Box::leak；测试进程也不会长期占内存。
    static SIG: ResponseSignal = ResponseSignal::new();
    broadcast_once(&SIG);
    // Signal 的 API 只支持 async wait；此处只验证不 panic + 状态被 signaled
    assert!(SIG.signaled());
  }
}
