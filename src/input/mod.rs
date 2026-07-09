//! # 输入聚合层
//!
//! 把 [`hal`](crate::hal) 里的**通用组件**聚合成一个**面向本硬件配置**的采样器。
//!
//! ## 职责
//! - 拥有所有硬件组件（按钮/开关/摇杆/旋钮）
//! - 提供一个 [`InputSampler::poll`] 方法，返回打包好的 [`GamepadState`]
//! - **不做**协议编码、也**不做**输出反馈；那是上层的事
//!
//! ## 使用
//! ```ignore
//! let mut sampler = InputSampler::new(/* 所有 hal 组件 + adc */);
//! loop {
//!   let state = sampler.poll();
//!   transport.send(&Frame::new(seq, state))?;
//!   Timer::after(...).await;
//! }
//! ```

mod sampler;

pub use sampler::{InputSampler, SampleOutput, update_button_led_state};

use crate::protocol::GamepadState;
use crate::transport::control::{SENSITIVITY_MAX, joy_sensitivity, knob_sensitivity};

/// 就地应用灵敏度缩放
///
/// 从 [`crate::transport::control`] 的全局 Atomic 读取当前灵敏度（0..=1000，
/// 定点数分母为 [`SENSITIVITY_MAX`] = 1000），对摇杆的两轴 + 两旋钮做等比缩放。
///
/// # 语义
/// - `scale = 1000` → 不变（100%）
/// - `scale = 500`  → 一半灵敏度（50%）
/// - `scale = 0`    → 完全屏蔽（0%）
///
/// # 边界
/// - i16 摇杆：先扩展到 i32 计算，再 saturating 转回 i16
/// - u16 旋钮：直接 u32 乘除
///
/// # 为什么放在 input 模块而不是 protocol？
/// - `GamepadState` 是纯数据；缩放属于"输入处理"，是 input 层的责任
/// - 未来可扩展死区、反向、曲线等，都可以放这里
pub fn apply_sensitivity(state: &mut GamepadState) {
  let joy_scale = joy_sensitivity();
  let knob_scale = knob_sensitivity();

  if joy_scale != SENSITIVITY_MAX {
    state.joy_x = scale_i16(state.joy_x, joy_scale);
    state.joy_y = scale_i16(state.joy_y, joy_scale);
  }
  if knob_scale != SENSITIVITY_MAX {
    state.knob_1 = scale_u16(state.knob_1, knob_scale);
    state.knob_2 = scale_u16(state.knob_2, knob_scale);
  }
}

/// i16 * scale / 1000（saturating）
///
/// # 编译期不变量
/// 用 `const { assert!(...) }` 把"`SENSITIVITY_MAX` 必须能安全参与 `i32` 运算
/// 且不为 0"这条前置条件固化为**编译期检查**（比 unit test 更强 —— 一旦有人
/// 把 `SENSITIVITY_MAX` 改成 `0` 或 `u16::MAX`，构建立刻失败而不是运行时才炸）。
///
/// # 输出范围
/// `scale ≤ SENSITIVITY_MAX` 时，返回值 ∈ [i16::MIN, i16::MAX]（`clamp` 兜底
/// 处理 `scale = SENSITIVITY_MAX + k` 的越界情况）。
fn scale_i16(v: i16, scale: u16) -> i16 {
  const {
    assert!(
      SENSITIVITY_MAX > 0,
      "SENSITIVITY_MAX must be non-zero divisor"
    );
    assert!(
      SENSITIVITY_MAX as u32 <= i32::MAX as u32,
      "SENSITIVITY_MAX must fit in i32 for scaling arithmetic"
    );
  }
  let result = (i32::from(v) * i32::from(scale)) / i32::from(SENSITIVITY_MAX);
  result.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// u16 * scale / 1000（saturating）
///
/// # 编译期不变量
/// 见 [`scale_i16`] 上方注释；此处复用同一组常量断言。
fn scale_u16(v: u16, scale: u16) -> u16 {
  const {
    assert!(
      SENSITIVITY_MAX > 0,
      "SENSITIVITY_MAX must be non-zero divisor"
    );
  }
  let result = (u32::from(v) * u32::from(scale)) / u32::from(SENSITIVITY_MAX);
  result.min(u32::from(u16::MAX)) as u16
}
