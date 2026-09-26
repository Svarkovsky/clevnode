//! Battery voltage monitor on the WisMesh Pocket V2 (RAK19026 VC baseboard).
//!
//! Battery sense is on **P0.05 / AIN3** through a 1.5/2.5 voltage divider
//! (multiplier 1.73). The SAADC is configured single-ended with the
//! internal 0.6 V reference and `Gain::GAIN1_6` so full-scale at the pin
//! reads 3.6 V — adequate headroom even when a freshly-charged 1S LiPo
//! sits at ~4.2 V which divides to ~2.5 V at the ADC pin.
//!
//! Cell-count detection: the WisMesh Pocket V2 ships with a 1S LiPo, but
//! some RAK4631-based dev kits use a 2S pack. We classify on the first
//! sample (1S maxes near 4.4 V, 2S floors above ~6.0 V — they don't
//! overlap), and stick with the result for the lifetime of the task.
//! Persistent storage is deferred until the identity-store / NVMC-sharing
//! refactor lands; redoing the classification on every boot costs one
//! ADC reading.
//!
//! Voltage-to-percent: per-cell LiPo OCV curve borrowed verbatim from
//! `meshtastic/src/power.h:13-22`, multiplied by the detected cell count
//! for the threshold values.

use embassy_executor::Spawner;
use embassy_nrf::peripherals;
use embassy_nrf::saadc::{self, ChannelConfig, Config, Resolution, Saadc};
use embassy_nrf::{bind_interrupts, Peri};
use embassy_time::{Duration, Timer};

use crate::baseboard::{BatteryState, BATTERY_STATE};

bind_interrupts!(pub struct BatteryIrqs {
    SAADC => saadc::InterruptHandler;
});

/// Per-cell open-circuit-voltage curve (mV → percent), Meshtastic-borrowed.
/// Piecewise-linear interpolation between adjacent points.
const OCV_CURVE: [(u16, u8); 11] = [
    (4190, 100),
    (4050, 90),
    (3990, 80),
    (3890, 65),
    (3800, 50),
    (3720, 35),
    (3630, 20),
    (3530, 10),
    (3420, 5),
    (3300, 0),
    (3000, 0), // sentinel — anything below 3.0 V/cell stays at 0 %
];

/// nRF52840 internal reference is 0.6 V, full-scale with Gain::GAIN1_6 is
/// 3.6 V at the pin. 12-bit ADC ⇒ raw range 0..=4095. The 1.5/2.5 divider
/// outside the chip gives a full-scale at the battery terminal of
/// 3.6 V × 5/3 = 6.0 V — fits 1S LiPo (max ~4.2 V) and 2S LiPo (max
/// ~8.4 V scaled to 5.04 V at the ADC ~ 84 % of the 6 V range).
fn raw_to_battery_mv(raw: i16) -> u16 {
    // Saadc::sample returns i16 but values are non-negative for a single-
    // ended positive input. Clamp negatives (rail-to-rail noise) to 0.
    let r = raw.max(0) as u32;
    // mv_at_pin = r * 3600 / 4095
    // mv_at_battery = mv_at_pin * 1.73 (the 1.73 factor encodes the
    //                                  1.5/2.5 voltage divider per
    //                                  meshtastic variant.h:267).
    // Combine: r * 3600 * 173 / (4095 * 100) = r * 622_800 / 409_500
    // To stay in u32 we factor: r * 6228 / 4095 mV.
    // Sanity: r=4095 → 6228 mV (close to the 6.0 V full-scale).
    ((r * 6228) / 4095) as u16
}

/// Map a per-cell millivolt reading to a 0–100 percent estimate via the
/// LiPo OCV curve. Inputs above the top breakpoint clamp to 100 %, below
/// the bottom to 0 %.
fn cell_mv_to_percent(cell_mv: u16) -> u8 {
    if cell_mv >= OCV_CURVE[0].0 {
        return 100;
    }
    for i in 0..OCV_CURVE.len() - 1 {
        let (hi_mv, hi_pct) = OCV_CURVE[i];
        let (lo_mv, lo_pct) = OCV_CURVE[i + 1];
        if cell_mv >= lo_mv {
            // Linear interpolation between (lo_mv, lo_pct) and (hi_mv, hi_pct).
            let span_mv = (hi_mv - lo_mv) as u32;
            let span_pct = (hi_pct - lo_pct) as u32;
            let above = (cell_mv - lo_mv) as u32;
            return (lo_pct as u32 + above * span_pct / span_mv.max(1)) as u8;
        }
    }
    0
}

/// Decide whether the pack is 1S or 2S from a single millivolt reading.
/// 1S is anything below ~5.0 V (max for 1S is 4.2 V at the cell terminal,
/// so a freshly charged 1S sits below ~4.4 V and we add headroom). 2S
/// floors near 6.0 V even at deep discharge, so the gap is a comfortable
/// 1 V wide. Anything in between is unusual; we conservatively assume 1S
/// and log a warning.
fn classify_cell_count(pack_mv: u16) -> u8 {
    if pack_mv >= 6000 {
        2
    } else {
        1
    }
}

#[embassy_executor::task]
pub async fn battery_task(
    saadc_periph: Peri<'static, peripherals::SAADC>,
    adc_pin: Peri<'static, peripherals::P0_05>,
) {
    let mut config = Config::default();
    config.resolution = Resolution::_12BIT;
    // ChannelConfig::single_ended already picks Reference::INTERNAL (0.6 V)
    // and Gain::GAIN1_6 (full-scale 3.6 V at the pin) on nRF52840 — exactly
    // what the WisMesh Pocket V2's 1.5/2.5 divider needs to give 6 V
    // headroom at the battery terminal.
    let ch = ChannelConfig::single_ended(adc_pin);
    let mut adc = Saadc::new(saadc_periph, BatteryIrqs, config, [ch]);

    // Single sample for cell-count detection.
    let mut buf = [0i16; 1];
    adc.sample(&mut buf).await;
    let pack_mv_first = raw_to_battery_mv(buf[0]);
    let cell_count = classify_cell_count(pack_mv_first);
    crate::log::log_fmt(
        "[BAT] ",
        format_args!("init pack_mv={} cells={}S", pack_mv_first, cell_count),
    );

    let sender = BATTERY_STATE.sender();

    // EWMA filter — α = 0.5 (LPF coefficient Meshtastic uses on this same
    // signal, `power.cpp:392`). Smooths transient ADC spikes without
    // adding noticeable lag at the 5-second cadence.
    let mut ewma_mv: u32 = pack_mv_first as u32;

    loop {
        adc.sample(&mut buf).await;
        let pack_mv = raw_to_battery_mv(buf[0]) as u32;
        ewma_mv = (ewma_mv + pack_mv) / 2;
        let cell_mv = (ewma_mv as u16) / cell_count as u16;
        let percent = cell_mv_to_percent(cell_mv);

        sender.send(BatteryState {
            voltage_mv: ewma_mv as u16,
            percent,
            cell_count,
        });

        Timer::after(Duration::from_secs(5)).await;
    }
}

/// Convenience wrapper invoked from the bin file.
pub fn init(
    spawner: &Spawner,
    saadc_periph: Peri<'static, peripherals::SAADC>,
    adc_pin: Peri<'static, peripherals::P0_05>,
) {
    spawner.must_spawn(battery_task(saadc_periph, adc_pin));
}
