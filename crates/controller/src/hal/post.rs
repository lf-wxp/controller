//! # 硬件开机自检（POST · Power-On Self-Test）
//!
//! 与 [`crate::self_test`]（协议 / 构建不变式，失败即 panic）互补：本模块检查
//! **物理外设**是否就位、读数是否合理。定位为**非致命诊断**——外设异常只打
//! `warn` 日志、**不阻断启动**（缺屏、引脚浮空不应让手柄直接变砖）。
//!
//! ## 覆盖项
//! - **OLED**：I²C 地址探测（写一个控制字节看设备是否 ACK）
//! - **ADC**：摇杆 X/Y + 两个旋钮各采一次原始值，落在轨值（`0` / `ADC_MAX`）
//!   时告警——可能是引脚浮空 / 短路；但旋钮、摇杆推到底也会到轨，故仅 `warn`
//!   而不判失败。
//!
//! ## 为什么不 panic
//! 无线收发（BLE / ESP-NOW）才是手柄的核心功能，屏幕 / 某路模拟量缺失时仍应
//! 能正常发帧。因此硬件层异常只作为诊断信息呈现，交由使用者判断，而不像协议
//! 自检那样直接复位。

use defmt::{info, warn};
use esp_hal::Blocking;
use esp_hal::i2c::master::I2c;

use crate::config::tuning::ADC_MAX;

/// SSD1306 默认 7-bit I²C 地址。
pub const OLED_I2C_ADDR: u8 = 0x3C;

/// 开机自检结果汇总（供 OLED 开机 POST 摘要屏展示）。
///
/// `protocol_ok` / `radio_ok` 语义为"已顺利越过对应初始化阶段"——协议自检失败
/// 会直接 panic 复位、无线控制器初始化失败会 `expect`/`unwrap` panic，因此能构造
/// 出本结构时这两项必为 `true`；显示它们只是给使用者一个完整的越障确认。
#[derive(Debug, Clone, Copy)]
pub struct PostReport {
  /// 协议 / 构建不变式自检（[`crate::self_test`]）已通过
  pub protocol_ok: bool,
  /// 无线控制器（Wi-Fi / BLE）已初始化成功
  pub radio_ok: bool,
  /// OLED 在 I²C 总线上有应答（[`probe_oled`]）
  pub oled_present: bool,
  /// 各 ADC 通道读数均未处于轨值（[`check_adc`]）
  pub adc_ok: bool,
}

/// 探测 OLED 是否在 I²C 总线上应答（ACK）。
///
/// 向 `addr` 写一个 SSD1306 命令流控制字节（`0x00`）：设备存在会 ACK，
/// 不存在 / 接触不良则总线返回错误。写一个孤立控制字节对 SSD1306 无副作用
/// （后续 `Ssd1306::init` 会重发完整初始化序列）。
///
/// # 返回
/// `true` = 设备已应答；`false` = 无应答（仅打 `warn`，调用方照常继续）。
#[must_use]
pub fn probe_oled(i2c: &mut I2c<'_, Blocking>, addr: u8) -> bool {
  match i2c.write(addr, &[0x00]) {
    Ok(()) => {
      info!("[POST] OLED @0x{:02x}: ACK", addr);
      true
    }
    Err(_e) => {
      warn!(
        "[POST] OLED @0x{:02x}: no ACK (display missing or wiring fault?)",
        addr
      );
      false
    }
  }
}

/// 校验一组 ADC 通道的原始读数是否合理。
///
/// `channels`：`(名字, 原始值)` 列表，原始值范围 `0..=ADC_MAX`。任一通道读到
/// 轨值（`0` 或 `>= ADC_MAX`）都打 `warn`——可能是引脚浮空 / 短路；但旋钮、
/// 摇杆推到底同样会到轨，因此仅作告警。
///
/// # 返回
/// `true` = 全部通道都落在轨值以内（无可疑读数）。
pub fn check_adc(channels: &[(&str, u16)]) -> bool {
  let mut all_ok = true;
  for &(name, raw) in channels {
    if raw == 0 || raw >= ADC_MAX {
      warn!(
        "[POST] ADC {}: raw={} at rail (floating/short? or knob/stick at limit)",
        name, raw
      );
      all_ok = false;
    } else {
      info!("[POST] ADC {}: raw={}", name, raw);
    }
  }
  all_ok
}
