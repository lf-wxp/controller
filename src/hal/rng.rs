//! # 硬件随机数封装（Q 选项）
//!
//! ## 目的
//! 为需要"启动阶段一次性熵源"的模块（K3 [`SESSION_NONCE`] 初始化、未来的 CSPRNG
//! seeding、密钥派生等）提供统一入口。屏蔽 esp-hal `Rng` peripheral 的细节，
//! 让业务侧只关心"我要一个 32-bit 熵"。
//!
//! ## 熵源分阶段
//! ESP32 的 RNG 硬件寄存器熵源随 Wi-Fi/BLE 状态变化：
//!
//! | 阶段                       | 熵源                            | 强度      |
//! |----------------------------|--------------------------------|-----------|
//! | Wi-Fi/BLE 未初始化         | 内部 PRNG（系统时钟 seed）      | 弱（可预测）|
//! | Wi-Fi/BLE 初始化后         | RF phy 采样 + PRNG 混合         | 真随机    |
//!
//! ## 使用时机
//! [`init_seed`] 通常在启动阶段**Wi-Fi/BLE 初始化之后**调用一次，用作
//! [`crate::protocol::init_session_nonce`] 的 seed；调用后 [`Rng`] 实例可以
//! 丢弃或保留给未来其它需要熵的模块。
//!
//! ## 混合策略
//! 即使 Wi-Fi/BLE 未启用，[`init_seed`] 也会把 [`embassy_time::Instant::now`]
//! 的低 32 位与 RNG 输出做 XOR 混合——两个弱熵源叠加至少能让"两次冷启动 seed
//! 相同"的概率大幅降低。
//!
//! ## 为什么不消费 peripheral？
//! esp-hal 1.1 的 [`Rng`] 是**零大小类型**（`pub struct Rng;`），调用
//! [`Rng::new()`] 不需要 peripheral，也不会独占硬件——多个模块可以各自 `new`
//! 一份用，硬件寄存器本身就是全局共享的。
//!
//! [`SESSION_NONCE`]: crate::protocol::auth::SESSION_NONCE
//! [`Rng`]: esp_hal::rng::Rng

use embassy_time::Instant;
use esp_hal::rng::Rng;

/// 生成一次性 32-bit seed，混合 [`Rng`] 硬件输出与 [`Instant::now`] 时钟抖动
///
/// # 返回
/// 一个混合熵源的 `u32`；调用方通常直接喂给
/// [`crate::protocol::init_session_nonce`]。
///
/// # 混合公式
/// ```text
///   seed = rng.random() ^ (Instant::now().as_ticks() as u32)
/// ```
///
/// # 熵源保证
/// - 若 Wi-Fi/BLE 已启用：`rng.random()` 已经是真随机 → 结果真随机
/// - 若 Wi-Fi/BLE 未启用：`rng.random()` 是 PRNG，但叠加了启动至此时的
///   时钟抖动（受调度、外设初始化时长影响），至少每次冷启动都不同
///
/// # 使用示例
/// ```ignore
/// let seed = controller::hal::rng::init_seed();
/// controller::protocol::init_session_nonce(seed);
/// ```
#[must_use]
pub fn init_seed() -> u32 {
  let rng = Rng::new();
  let hw_random = rng.random();
  // Instant::as_ticks() 返回 u64；截断到低 32 位即可（我们只需要 32 bit 熵）
  let clock_jitter = Instant::now().as_ticks() as u32;
  hw_random ^ clock_jitter
}

/// 便捷入口：连续读多个 32-bit 熵（未来 key rotation / CSPRNG seeding 会用到）
///
/// 内部会重复调用 [`Rng::random`]，每次都从硬件寄存器现取，不缓存。
///
/// # 参数
/// - `dst`：待填充的 u32 slice；每个元素独立填一次
///
/// # 使用示例
/// ```ignore
/// let mut keys = [0_u32; 4];
/// controller::hal::rng::fill(&mut keys);
/// ```
pub fn fill(dst: &mut [u32]) {
  let rng = Rng::new();
  for slot in dst.iter_mut() {
    *slot = rng.random();
  }
}

#[cfg(test)]
mod tests {
  // 本模块的核心 API 依赖硬件寄存器，在宿主机 `cargo test` 环境无法真正运行。
  // 这里保留一个空的 mod 结构，未来在真机集成测试（`tests/` 目录 + `#[embedded_test]`）
  // 补入随机性检验（比如连续 100 次采样不全相等）。
  //
  // 宿主机侧的替代验证：见 [`crate::protocol::auth`] 单元测试通过 `SESSION_NONCE`
  // 手工 set 不同值来验证 nonce 影响 HMAC 计算——不依赖真硬件 RNG。
}
