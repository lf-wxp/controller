//! Demo test suite using embedded-test
//!
//! You can run this using `cargo test` as usual.

#![no_std]
#![no_main]

// Panic handler + defmt global logger via UART（无需 JTAG）：
// - esp_backtrace 提供 #[panic_handler]，panic 时用 defmt::error! 打印
// - esp_println 通过 #[defmt::global_logger] 自动注册 defmt logger
use esp_backtrace as _;
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
