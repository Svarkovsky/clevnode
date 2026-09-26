//! User-button handling on the WisMesh Pocket V2.
//!
//! The carrier's only externally accessible button is on **P0.09** (NFC1,
//! freed by the RAK bootloader's UICR patch — no firmware-side UICR work
//! needed). It is wired active-low with no external pull-up, so we enable
//! the internal pull-up.
//!
//! Behaviour:
//!
//! | Display state | Press duration | Action |
//! |---|---|---|
//! | on  | any           | display off |
//! | off | < 2 s         | ignore |
//! | off | ≥ 2 s         | display on |
//!
//! Rationale for the asymmetric display logic: a short press in the
//! user's pocket can't accidentally wake the screen and burn battery;
//! turning it off is one quick tap.
//!
//! The actual display power-cycling happens in `display::display_task`,
//! which subscribes to `DISPLAY_ON_REQ` and translates a transition into
//! an SSD1306 `set_display_on(_)` command.

use embassy_executor::Spawner;
use embassy_nrf::gpio::{AnyPin, Input, Pull};
use embassy_nrf::Peri;
use embassy_time::{Duration, Instant};

use crate::baseboard::DISPLAY_ON_REQ;

/// Anything below this counts as a short press (display-on gesture).
const LONG_PRESS_MIN: Duration = Duration::from_millis(2000);

#[embassy_executor::task]
pub async fn button_task(button_pin: Peri<'static, AnyPin>) {
    let mut button = Input::new(button_pin, Pull::Up);
    let sender = DISPLAY_ON_REQ.sender();
    // Receiver here is just for the "what's the current state?" lookup
    // when classifying a press. We never block on changes from this side.
    let mut state_rx = DISPLAY_ON_REQ
        .receiver()
        .expect("DISPLAY_ON_REQ watch capacity (button reader)");

    loop {
        // Press start: GPIO falls low (active-low button).
        button.wait_for_falling_edge().await;
        let press_start = Instant::now();

        // Press end. embassy-nrf's GPIOTE-backed wait reliably catches
        // the rising edge no matter how fast.
        button.wait_for_rising_edge().await;
        let duration = press_start.elapsed();

        // Treat first-ever press (Watch never sent yet) as if the display
        // is on — that's the boot state we expect.
        let currently_on = state_rx.try_get().unwrap_or(true);

        let target = if currently_on {
            // Display on + any press → off.
            false
        } else if duration >= LONG_PRESS_MIN {
            // Display off + ≥ 2 s → on.
            true
        } else {
            // Display off + < 2 s → ignore (pocket-press protection).
            crate::log::log_fmt(
                "[BTN] ",
                format_args!(
                    "short press ignored (display off, {} ms)",
                    duration.as_millis()
                ),
            );
            continue;
        };

        crate::log::log_fmt(
            "[BTN] ",
            format_args!(
                "press {} ms → display {}",
                duration.as_millis(),
                if target { "on" } else { "off" }
            ),
        );
        sender.send(target);
    }
}

/// Convenience wrapper invoked from the bin file.
pub fn init(spawner: &Spawner, button_pin: Peri<'static, AnyPin>) {
    spawner.must_spawn(button_task(button_pin));
}
