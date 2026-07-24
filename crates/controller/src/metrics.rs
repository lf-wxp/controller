//! # 全局运行时观测计数器（M-5 相关加固）
//!
//! 本模块集中 controller 侧**幂等、可观测**的原子计数器，供持久化子系统在运行时
//! 上报"发生但被容忍"的事件：
//!
//! - [`flash_write_count`]：NVS flash 实际发生的写入次数（M-5）
//!
//! ## Response 丢弃观测已下沉到 comm
//! 两条主链路（BLE / ESP-NOW）的出站 Response 现在**同型**——都是 comm 的有界队列，
//! 满队丢弃统一计入 [`comm::metrics::dropped_responses`]。因此 controller 不再维护
//! 本地的"Response 覆盖计数"，避免同一次丢弃两处记账、也消除了口径分裂。
//!
//! ## 为什么集中？
//! - **单一职责**：flash 磨损是"不影响功能但需要长期观测"的指标
//! - **零成本**：仅在 worker 路径上一次 `fetch_add(1, Relaxed)`，
//!   无跨线程数据依赖 → 用 `Relaxed` 已足够
//! - **可暴露给 dashboard**：Dashboard 端可通过一条特殊的 diagnostic Command
//!   拉取这些计数器，实现无侵入的健康监测
//!
//! ## Ordering 选型
//! 全部使用 `Relaxed`。理由：
//! - 计数器**只被单调递增**（fetch_add），从不与其它内存位置构成 happens-before 关系
//! - 读取路径（dashboard 拉取）只关心"最终能看到一个大致准确的值"，
//!   哪怕短暂延迟 1 个计数也无实质影响
//! - `Relaxed` 在 xtensa esp32 上编译为单条 `l32ai` / `s32ri` 指令，
//!   无 memory barrier 开销

use core::sync::atomic::{AtomicU32, Ordering};

// ============================================================
// M-5：NVS flash 写次数计数
// ============================================================

/// NVS flash 实际写入次数计数器
///
/// 每当 `NvsStorage::save()` 成功完成一次 flash 擦-写循环时递增。
///
/// # 何时值会显著上升？
/// - `REPLAY_PERSIST_INTERVAL`（M-2 修复后）触发的 replay-only 落盘
/// - 用户操作触发的设置变更（灵敏度、电池模拟开关等）
///
/// # 阈值建议（NOR flash ~10 万次擦写寿命）
/// - **每分钟 < 1 次**：稳态可运行 > 1900 小时（约 80 天连续满负荷）
/// - **每分钟 > 10 次**：需要引入"批量合并 + 定时器强制刷"逻辑（M-5 长期项）
static FLASH_WRITE_COUNT: AtomicU32 = AtomicU32::new(0);

/// 记录一次 flash 写入事件（供 `NvsStorage::save` 内部调用）
#[inline]
pub fn record_flash_write() {
  FLASH_WRITE_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// 读取当前 flash 写入累计次数
#[inline]
#[must_use]
pub fn flash_write_count() -> u32 {
  FLASH_WRITE_COUNT.load(Ordering::Relaxed)
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn flash_write_counter_is_monotonic() {
    let before = flash_write_count();
    record_flash_write();
    assert_eq!(flash_write_count(), before + 1);
  }
}
