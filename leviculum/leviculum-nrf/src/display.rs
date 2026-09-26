//! 1.3" OLED status display on the WisMesh Pocket V2 (RAK19026 VC baseboard).
//!
//! Builds an I²C bus on TWISPI0 with SDA on P0.13 / SCL on P0.14, runtime-
//! probes for an OLED at 0x3C (and 0x3D as fallback), and drives an SSD1306
//! status screen at ~1 Hz.
//!
//! Chip-detection follows Meshtastic's `src/detect/ScanI2CTwoWire.cpp:52-87`
//! exactly: send register address `0x00`, read one byte, mask the lower
//! nibble, and classify per the lookup table. SH1106 detection is logged
//! but not currently rendered; the only sh1106 crate on crates.io still
//! pins embedded-hal 0.2.3 which is not directly usable with embassy-nrf
//! 0.9's TWIM. SH1106 hardware is uncommon on RAK19026 VC and adding it
//! is a follow-up.

extern crate alloc;

use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_nrf::peripherals;
use embassy_nrf::twim::{self, Twim};
use embassy_nrf::{bind_interrupts, Peri};
use embassy_time::{Duration, Timer};

use leviculum_screen::{ident_short, BatteryStatus, FrameKey, GnssStatus, StatusModel};
use ssd1306::{prelude::*, I2CDisplayInterface, Ssd1306};
use static_cell::StaticCell;

bind_interrupts!(pub struct DisplayIrqs {
    TWISPI0 => twim::InterruptHandler<peripherals::TWISPI0>;
});

/// I²C addresses Meshtastic probes for an OLED. Order matters — 0x3C first.
const PROBE_ADDRS: [u8; 2] = [0x3C, 0x3D];

/// What the chip-probe found. Only the first variant is renderable today.
#[derive(Clone, Copy, Debug)]
enum DetectedKind {
    Ssd1306,
    Sh1106,
    None,
}

#[derive(Clone, Copy, Debug)]
struct Detected {
    kind: DetectedKind,
    addr: u8,
}

/// Address-only ACK probe — matches Meshtastic's `scanPort` quick-test
/// (`beginTransmission(addr); endTransmission()` with no payload). A
/// SSD1306/SH1106 in display-off mode will still ACK its own address even
/// when it would NACK a register read; using `write_read` for the probe
/// (as we did initially) loses those slaves.
async fn ack_probe(twim: &mut Twim<'_>, addr: u8) -> bool {
    twim.write(addr, &[]).await.is_ok()
}

/// Read the SSD1306/SH1106 status byte at register 0x00 with the
/// stabilization loop from `meshtastic/src/detect/ScanI2CTwoWire.cpp:52-87`.
/// Only called after `ack_probe` confirmed the slave is alive.
async fn classify(twim: &mut Twim<'_>, addr: u8) -> DetectedKind {
    let mut r: u8 = 0;
    let mut prev: u8 = 0xFF;
    let mut tries: u8 = 0;
    while r != prev && tries < 4 {
        prev = r;
        let mut buf = [0u8; 1];
        if twim.write_read(addr, &[0x00], &mut buf).await.is_err() {
            return DetectedKind::None;
        }
        r = buf[0] & 0x0F;
        tries += 1;
    }
    match r {
        0x00 | 0x08 => DetectedKind::Sh1106,
        0x03..=0x07 => DetectedKind::Ssd1306,
        _ => DetectedKind::None,
    }
}

/// Probe both standard addresses; the first hit wins.
///
/// If the slave ACKs its address but `classify()` returns `None` (typical
/// when an SSD1306 is in display-off and won't honour a status read), we
/// default to SSD1306. The RAK19026 VC's RAK-vendor sample driver hard-
/// codes SSD1306, so this is the correct default for this carrier; SH1106
/// detection is best-effort and currently not rendered anyway.
async fn detect(twim: &mut Twim<'_>) -> Detected {
    for &addr in &PROBE_ADDRS {
        if ack_probe(twim, addr).await {
            let kind = classify(twim, addr).await;
            let kind = match kind {
                DetectedKind::None => DetectedKind::Ssd1306,
                k => k,
            };
            return Detected { kind, addr };
        }
    }
    Detected {
        kind: DetectedKind::None,
        addr: 0,
    }
}

#[embassy_executor::task]
pub async fn display_task(
    twispi0: Peri<'static, peripherals::TWISPI0>,
    sda: Peri<'static, peripherals::P0_13>,
    scl: Peri<'static, peripherals::P0_14>,
    identity_hash: [u8; 16],
) {
    static TX_BUF: StaticCell<[u8; 64]> = StaticCell::new();

    let mut config = twim::Config::default();
    config.frequency = twim::Frequency::K400;
    // The RAK19026 VC baseboard exposes I²C1 (SDA P0.13 / SCL P0.14) without
    // external pull-up resistors — internal nRF52840 pull-ups are required.
    config.sda_pullup = true;
    config.scl_pullup = true;

    // TX_BUF is owned exclusively by this single, never-respawned task.
    // StaticCell::init hands out a unique `&'static mut` with no aliasing
    // hazard (it panics if init runs twice, which cannot happen here).
    let tx_buf: &'static mut [u8] = TX_BUF.init([0u8; 64]);
    let mut twim = Twim::new(twispi0, DisplayIrqs, sda, scl, config, tx_buf);

    // OLEDs (and several baseboard sensors) need a few hundred ms after the
    // 3V3-S rail comes up before they ACK on I²C. Give the bus 500 ms quiet
    // time so the very first probe doesn't see a still-resetting chip.
    Timer::after(Duration::from_millis(500)).await;

    let detected = detect(&mut twim).await;
    crate::log::log_fmt(
        "[DISP] ",
        format_args!("OLED probe: {:?} at 0x{:02X}", detected.kind, detected.addr),
    );

    let mut display = match detected.kind {
        DetectedKind::Ssd1306 => {
            let interface = I2CDisplayInterface::new_custom_address(twim, detected.addr);
            let mut d = Ssd1306::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
                .into_buffered_graphics_mode();
            if let Err(e) = d.init() {
                crate::log::log_fmt("[DISP] ", format_args!("init failed: {:?}", e));
                return;
            }
            crate::log::log_fmt("[DISP] ", format_args!("SSD1306 init ok"));
            d
        }
        DetectedKind::Sh1106 => {
            crate::log::log_fmt(
                "[DISP] ",
                format_args!("SH1106 detected — not rendered (driver pending)"),
            );
            return;
        }
        DetectedKind::None => {
            crate::log::log_fmt(
                "[DISP] ",
                format_args!("no OLED found on TWISPI0 — display task exiting"),
            );
            return;
        }
    };

    let id_short = ident_short(&identity_hash);

    // Receivers for shared baseboard state — held outside the loop so the
    // last value survives between producer updates.
    #[cfg(feature = "gnss")]
    let mut gnss_rx = crate::baseboard::GNSS_FIX
        .receiver()
        .expect("gnss watch capacity");
    // Presence consumer (#240). A `None` receiver means the capacity
    // accounting in baseboard.rs is wrong — surface it, render as
    // "init" forever rather than panicking the display task.
    #[cfg(feature = "gnss")]
    let mut presence_rx = {
        let rx = crate::baseboard::GNSS_PRESENCE.receiver();
        if rx.is_none() {
            crate::log::log_fmt(
                "[DISP] ",
                format_args!("GNSS_PRESENCE watch capacity exhausted"),
            );
        }
        rx
    };
    #[cfg(feature = "battery")]
    let mut bat_rx = crate::baseboard::BATTERY_STATE
        .receiver()
        .expect("battery watch capacity");

    // Display power state, controlled by `crate::button` via DISPLAY_ON_REQ.
    // We seed the watch to `true` so the button task can read the current
    // state before the user has ever pressed anything.
    let display_req_sender = crate::baseboard::DISPLAY_ON_REQ.sender();
    display_req_sender.send(true);
    let mut display_req_rx = crate::baseboard::DISPLAY_ON_REQ
        .receiver()
        .expect("DISPLAY_ON_REQ watch capacity (display reader)");
    let mut display_on = true;

    // Frame key (see `leviculum_screen::FrameKey`) — when nothing
    // relevant has changed since last render, we skip the I²C flush
    // entirely.
    let mut last_key: Option<FrameKey> = None;
    let mut tick: u32 = 0;

    loop {
        tick = tick.wrapping_add(1);
        let heartbeat = (tick / 5) & 1 != 0; // toggles every 5 seconds

        // Honour the latest power-state request from the button task. We
        // only act on actual changes — `try_changed()` returns `Some(_)`
        // exactly once per new value.
        if let Some(req) = display_req_rx.try_changed() {
            if req != display_on {
                if let Err(e) = display.set_display_on(req) {
                    crate::log::log_fmt(
                        "[DISP] ",
                        format_args!("set_display_on({}) failed: {:?}", req, e),
                    );
                } else {
                    crate::log::log_fmt(
                        "[DISP] ",
                        format_args!("display power → {}", if req { "on" } else { "off" }),
                    );
                    display_on = req;
                }
            }
        }

        // While the screen is off there is no point computing or flushing
        // a frame. Tick at the same 1 Hz cadence so the request poll above
        // still runs.
        if !display_on {
            Timer::after(Duration::from_secs(1)).await;
            continue;
        }

        let rx = crate::lora::LORA_RX_COUNT.load(Ordering::Relaxed);
        let tx = crate::lora::LORA_TX_COUNT.load(Ordering::Relaxed);

        #[cfg(feature = "battery")]
        let battery = match bat_rx.try_get() {
            Some(b) if b.voltage_mv > 0 => BatteryStatus::Data {
                percent: b.percent,
                voltage_mv: b.voltage_mv,
            },
            _ => BatteryStatus::NoData,
        };
        #[cfg(not(feature = "battery"))]
        let battery = BatteryStatus::FeatureOff;

        // The presence tri-state (#240) drives the GPS line: NoHardware
        // renders "check wiring", and the fix/search label follows the
        // hysteresis-held policy state rather than the raw per-sentence
        // receiver flag, so the display agrees with what consumers of
        // GNSS_PRESENCE act on. Sats/coords still come from the
        // GNSS_FIX snapshot. Before the first settle (watch empty) the
        // line stays in the historical "init" state.
        #[cfg(feature = "gnss")]
        let gnss = match presence_rx.as_mut().and_then(|rx| rx.try_get()) {
            Some(p) if p.state == crate::baseboard::GnssPresence::NoHardware => {
                GnssStatus::NoHardware
            }
            Some(p) => {
                let f = gnss_rx
                    .try_get()
                    .unwrap_or(crate::baseboard::GnssFix::empty());
                GnssStatus::Data {
                    sats: f.sat_in_use,
                    valid: p.state == crate::baseboard::GnssPresence::Fix,
                    coords: f.latitude.zip(f.longitude),
                }
            }
            None => GnssStatus::NoData,
        };
        #[cfg(not(feature = "gnss"))]
        let gnss = GnssStatus::FeatureOff;

        let model = StatusModel {
            title: "leviculum RAK4631",
            id_short: id_short.as_str(),
            rx,
            tx,
            battery,
            gnss,
            // Same source as the `links=` field of the `BLE_COUNTERS`
            // log line, so the screen and the capture can never
            // disagree. Not behind a cfg: `ble` is an ungated module
            // and both BSP features pull `softdevice`, so no
            // configuration of this crate builds without BLE — the
            // painter's `None` arm exists for a board that one day has
            // none, not for a feature combination reachable today.
            // `MAX_LINKS` is 4, so the saturation never bites.
            ble_peers: Some(crate::ble::HVN_DRAIN.claimed().min(u8::MAX as usize) as u8),
            heartbeat,
        };

        let key = model.key();
        if last_key.as_ref() == Some(&key) {
            // Nothing meaningful changed since last render and the
            // heartbeat phase is the same. Skip the I²C flush entirely.
            Timer::after(Duration::from_secs(1)).await;
            continue;
        }

        // The SSD1306's buffered-graphics mode is itself the paint
        // target; the shared painter clears, draws the six lines, and
        // sets the heartbeat dot at (128-3, 0) = (125, 0), exactly as
        // the in-loop drawing here always did.
        let _ = model.paint(&mut display, 128);

        if let Err(e) = display.flush() {
            crate::log::log_fmt("[DISP] ", format_args!("flush failed: {:?}", e));
        }
        last_key = Some(key);

        Timer::after(Duration::from_secs(1)).await;
    }
}

/// Convenience wrapper invoked from the bin file.
pub fn init(
    spawner: &Spawner,
    twispi0: Peri<'static, peripherals::TWISPI0>,
    sda: Peri<'static, peripherals::P0_13>,
    scl: Peri<'static, peripherals::P0_14>,
    identity_hash: [u8; 16],
) {
    spawner.must_spawn(display_task(twispi0, sda, scl, identity_hash));
}
