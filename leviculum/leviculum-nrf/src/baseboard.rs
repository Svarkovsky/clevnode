//! Shared state for RAK19026 baseboard peripherals (display + GNSS + battery).
//!
//! This module is the rendezvous point between the producer tasks (GNSS,
//! battery) and the consumer task (display). It is compiled whenever any
//! one of the three feature flags is enabled; each individual `Watch` /
//! struct definition is then gated on the producing peripheral's feature.

#![allow(dead_code)]

#[cfg(any(feature = "display", feature = "gnss", feature = "battery"))]
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
#[cfg(any(feature = "display", feature = "gnss", feature = "battery"))]
use embassy_sync::watch::Watch;

/// Compact GNSS fix snapshot — what the display task needs and nothing
/// more. Updated by `gnss::gnss_task` when an RMC or GGA sentence parses
/// cleanly. `valid` is the receiver's own validity flag (RMC `Mode::is_valid`
/// or GGA quality != NoFix), not a project-wide policy.
///
/// `latitude` / `longitude` are decimal degrees with sign (negative = S/W,
/// positive = N/E). They keep their last good values across non-valid
/// sentences so the display has something to render even briefly during
/// a fix dip.
#[cfg(feature = "gnss")]
#[derive(Clone, Copy, Debug)]
pub struct GnssFix {
    /// True when the receiver reports a usable fix.
    pub valid: bool,
    /// Number of satellites in use, from the latest GGA.
    pub sat_in_use: u8,
    /// Latitude in decimal degrees (signed). `None` until the first valid
    /// sentence carries position data.
    pub latitude: Option<f64>,
    /// Longitude in decimal degrees (signed). Same convention as latitude.
    pub longitude: Option<f64>,
    /// RMC UTC date+time as unix seconds (Codeberg #166 item 1), set on
    /// every valid RMC and — unlike position — CLEARED on an invalid one:
    /// a stale time claim would seed the calendar wrong, while stale
    /// position only mis-renders. Consumers must treat it as "time as of
    /// this snapshot's publication", good to ~1 s (one RMC cadence).
    pub unix_secs: Option<u64>,
    /// Metres above mean sea level, from the latest GGA with a fix.
    /// Follows position: kept across non-valid sentences.
    pub altitude_m: Option<f32>,
    /// Horizontal dilution of precision, from the latest GGA with a fix
    /// (Codeberg #236). The only accuracy number standard NMEA carries,
    /// and therefore the one the telemetry accuracy gate is applied to —
    /// a position with no HDOP is a position of unknown quality and is
    /// not reported.
    pub hdop: Option<f32>,
    /// Ground speed in metres per second, from the latest valid RMC.
    pub speed_mps: Option<f32>,
    /// Course over ground in degrees, from the latest valid RMC. `None`
    /// while stationary — receivers stop reporting it, and inventing 0°
    /// would claim due north.
    pub bearing_deg: Option<f32>,
}

#[cfg(feature = "gnss")]
impl GnssFix {
    pub const fn empty() -> Self {
        Self {
            valid: false,
            sat_in_use: 0,
            latitude: None,
            longitude: None,
            unix_secs: None,
            altitude_m: None,
            hdop: None,
            speed_mps: None,
            bearing_deg: None,
        }
    }
}

/// Latest GNSS fix snapshot. Capacity-3 watch: one slot for the producer,
/// one for the display consumer, one for the main loop's calendar
/// seeding (#166).
#[cfg(feature = "gnss")]
pub static GNSS_FIX: Watch<CriticalSectionRawMutex, GnssFix, 3> = Watch::new();

/// The runtime GNSS presence answer (Codeberg #240), re-exported from
/// the pure crate that owns the sweep/hysteresis policy.
#[cfg(feature = "gnss")]
pub use leviculum_gnss_presence::Presence as GnssPresence;

/// Settled GNSS presence snapshot, published by `gnss::gnss_task` on
/// every state transition. The watch starts empty — "no value yet" IS
/// the detection phase, exactly like `GNSS_FIX` before the first
/// sentence.
///
/// Only [`GnssPresence::Fix`] may feed a position or a timebase;
/// [`GnssPresence::NoFix`] and [`GnssPresence::NoHardware`] are
/// operator-facing diagnoses (wait, versus check the wiring).
#[cfg(feature = "gnss")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GnssPresenceState {
    pub state: GnssPresence,
    /// The locked baud rate; 0 while `NoHardware` (no baud found).
    pub baud: u32,
}

/// Latest presence snapshot. Capacity-3 watch, same shape as
/// `GNSS_FIX`: display consumer, the #236 position tracker, one spare.
#[cfg(feature = "gnss")]
pub static GNSS_PRESENCE: Watch<CriticalSectionRawMutex, GnssPresenceState, 3> = Watch::new();

/// Battery snapshot published by the SAADC task and consumed by the display.
///
/// `voltage_mv` is the pack voltage in millivolts at the battery terminal
/// (already multiplied by the 1.73 hardware divider compensation).
/// `percent` is mapped from `voltage_mv` via the per-cell LiPo OCV curve
/// in `battery.rs`, scaled by the detected cell count.
/// `cell_count` is 1 or 2; on the WisMesh Pocket V2 the value is detected
/// at first boot from the read voltage and persisted, so subsequent
/// boots get a stable result.
#[cfg(feature = "battery")]
#[derive(Clone, Copy, Debug)]
pub struct BatteryState {
    pub voltage_mv: u16,
    pub percent: u8,
    pub cell_count: u8,
}

#[cfg(feature = "battery")]
impl BatteryState {
    pub const fn empty() -> Self {
        Self {
            voltage_mv: 0,
            percent: 0,
            cell_count: 1,
        }
    }
}

/// Latest battery snapshot. Same capacity-2 watch shape as `GNSS_FIX`.
#[cfg(feature = "battery")]
pub static BATTERY_STATE: Watch<CriticalSectionRawMutex, BatteryState, 2> = Watch::new();

/// Requested display power state, written by the user-button task and read
/// by both the display task (to drive `set_display_on`) and the button task
/// itself (to know what the next press should do).
///
/// Capacity-3 watch: button-writer + button-reader (current-state lookup) +
/// display-reader.
#[cfg(feature = "display")]
pub static DISPLAY_ON_REQ: Watch<CriticalSectionRawMutex, bool, 3> = Watch::new();

/// One-shot signals raised by `lora.rs` whenever a packet boundary
/// completes. The LED pulse tasks in `led.rs` block on these and emit a
/// short flash each time. Coalescing is intentional — a Signal stores at
/// most one pending event, so a burst of TX-completes inside an 80 ms
/// window collapses to a single flash. That matches what the eye can
/// resolve anyway.
#[cfg(feature = "display")]
pub static LORA_TX_FLASH: embassy_sync::signal::Signal<CriticalSectionRawMutex, ()> =
    embassy_sync::signal::Signal::new();

#[cfg(feature = "display")]
pub static LORA_RX_FLASH: embassy_sync::signal::Signal<CriticalSectionRawMutex, ()> =
    embassy_sync::signal::Signal::new();
