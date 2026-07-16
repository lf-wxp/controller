//! Demo test suite using embedded-test
//!
//! You can run this using `cargo test` as usual.

#![no_std]
#![no_main]

// Panic handler：由 `embedded_test` 提供
//   embedded-test 的 `#[embedded_test::tests]` 宏会自动注入 `#[panic_handler]`
//   用于报告测试失败。因此本文件不再显式 `use esp_backtrace as _;`
//   （若同时引入，会与 esp-backtrace 的 panic_handler 冲突：E0152）。
//
// 生产 firmware（crates/controller/src/bin/main.rs）走的是另一条路径：
//   通过 `cargo build`（默认 features 里包含 `firmware-panic`）启用
//   `esp-backtrace/panic-handler`，由 esp-backtrace 提供全局 panic_handler。
//
// defmt global logger 仍由 esp_println 通过 `#[defmt::global_logger]` 自动注册。
use esp_println as _;

esp_bootloader_esp_idf::esp_app_desc!();

#[cfg(test)]
#[embedded_test::tests(executor = esp_rtos::embassy::Executor::new())]
mod tests {
  use defmt::assert_eq;

  #[init]
  fn init() {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    let timg0 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);
    let sw_interrupt =
      esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    // defmt global logger 由链接期 `#[defmt::global_logger]` 自动注册，无需显式 init。
  }

  #[test]
  async fn hello_test() {
    defmt::info!("Running test!");

    embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
    assert_eq!(1 + 1, 2);
  }
}
