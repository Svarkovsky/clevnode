//! Activity-blink tasks for the two on-module LEDs of the RAK4631.
//!
//! Hardware:
//! - LED1 (green, P1.03)  — TX activity indicator
//! - LED2 (blue,  P1.04)  — RX activity indicator
//!
//! Both are driven from `embassy_sync::signal::Signal`s in
//! `crate::baseboard`. The LoRa task raises a signal at every successful
//! TX-done / RX-frame-reassembly, and a tiny task per LED converts each
//! signal into an ~80 ms pulse with a forced 30 ms off period. The off
//! window matters: a burst of completions inside one 80 ms span would
//! otherwise look like a single longer flash and the user couldn't count
//! the events.
//!
//! Module is gated on the same `display` feature as the rest of the
//! baseboard tasks — no LEDs without the rest of the user-facing
//! peripherals.

use embassy_executor::Spawner;
use embassy_nrf::gpio::Output;
use embassy_time::{Duration, Timer};

/// Pulse duration for one event. Long enough to be eye-visible, short
/// enough that two back-to-back events read as two flashes.
const PULSE_ON_MS: u64 = 80;
/// Forced quiet time after a pulse before the next can fire. Without this,
/// a tight burst of signals merges into a single long flash.
const PULSE_OFF_MS: u64 = 30;

#[embassy_executor::task]
pub async fn tx_blink_task(mut led: Output<'static>) {
    led.set_low(); // green LED is active-high; start off
    crate::log::log_fmt("[LED] ", format_args!("tx_blink_task alive"));
    loop {
        crate::baseboard::LORA_TX_FLASH.wait().await;
        led.set_high();
        Timer::after(Duration::from_millis(PULSE_ON_MS)).await;
        led.set_low();
        Timer::after(Duration::from_millis(PULSE_OFF_MS)).await;
    }
}

#[embassy_executor::task]
pub async fn rx_blink_task(mut led: Output<'static>) {
    led.set_low();
    crate::log::log_fmt("[LED] ", format_args!("rx_blink_task alive"));
    loop {
        crate::baseboard::LORA_RX_FLASH.wait().await;
        led.set_high();
        Timer::after(Duration::from_millis(PULSE_ON_MS)).await;
        led.set_low();
        Timer::after(Duration::from_millis(PULSE_OFF_MS)).await;
    }
}

/// Convenience wrapper invoked from the bin file.
pub fn init(spawner: &Spawner, tx_led: Output<'static>, rx_led: Output<'static>) {
    spawner.must_spawn(tx_blink_task(tx_led));
    spawner.must_spawn(rx_blink_task(rx_led));
}
