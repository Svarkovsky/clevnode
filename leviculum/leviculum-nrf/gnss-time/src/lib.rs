//! Pure time logic for GNSS calendar seeding (Codeberg #166 items 1+3).
//!
//! Two pieces, both free of peripherals so the host can unit-test them,
//! next to the `leviculum-screen` painter and `leviculum-sd-policy` that
//! follow the same pattern:
//!
//! - [`unix_secs_from_rmc_utc`] turns the UTC date+time of an NMEA RMC
//!   sentence into unix seconds. NMEA UTC only — the receiver has already
//!   applied the broadcast leap-second offset before building RMC, so no
//!   GPS-time correction may be applied here
//!   (`docs/src/concepts/time-and-clocks.md`, "GNSS specifics").
//! - [`SeedGate`] is the seed-once policy: one accepted fix seeds the
//!   calendar and the monotonic clock carries it forward; a refused fix
//!   leaves the gate open so the next fix retries.

#![cfg_attr(not(test), no_std)]

use nmea0183::datetime::DateTime;

/// Convert an RMC UTC date+time into unix seconds.
///
/// Fractional seconds are truncated: the wire granularity is one second,
/// and truncation errs backwards — the direction the calendar model
/// requires ("never ahead of the estimate").
///
/// Returns `None` for a date before the unix epoch, which no live
/// receiver can produce (the nmea0183 two-digit-year window is
/// 1970–2069) but which must not wrap into a huge unsigned value.
pub fn unix_secs_from_rmc_utc(dt: &DateTime) -> Option<u64> {
    let days = days_from_civil(
        i64::from(dt.date.year),
        i64::from(dt.date.month),
        i64::from(dt.date.day),
    );
    if days < 0 {
        return None;
    }
    let day_secs =
        u64::from(dt.time.hours) * 3600 + u64::from(dt.time.minutes) * 60 + dt.time.seconds as u64;
    Some(days as u64 * 86_400 + day_secs)
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date
/// (Howard Hinnant's `days_from_civil`, the standard branch-light form).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seed-once policy for calendar seeding from GNSS
/// (`docs/src/concepts/time-and-clocks.md`: "Acquire, seed, let the
/// receiver sleep").
///
/// The gate stays open until a candidate is *accepted* by the seam —
/// a refused candidate (out of the sanity window) must not close it,
/// or one garbage fix would wedge seeding for the whole boot.
pub struct SeedGate {
    seeded: bool,
}

impl SeedGate {
    pub const fn new() -> Self {
        Self { seeded: false }
    }

    /// Whether an accepted seed has closed the gate.
    pub fn is_seeded(&self) -> bool {
        self.seeded
    }

    /// Pass a candidate through the gate: returns it while the gate is
    /// open, swallows it once seeded.
    pub fn offer(&self, candidate: Option<u64>) -> Option<u64> {
        if self.seeded {
            None
        } else {
            candidate
        }
    }

    /// Close the gate — call only after the seam accepted the value.
    pub fn mark_seeded(&mut self) {
        self.seeded = true;
    }
}

impl Default for SeedGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmea0183::{ParseResult, Parser};

    /// Feed one complete sentence through the real nmea0183 parser and
    /// return its RMC — the exact pipeline the firmware's GNSS task runs.
    fn parse_rmc(sentence: &str) -> nmea0183::RMC {
        let mut parser = Parser::new();
        let mut line = sentence.to_string();
        line.push_str("\r\n");
        for b in line.bytes() {
            if let Some(result) = parser.parse_from_byte(b) {
                match result {
                    Ok(ParseResult::RMC(Some(rmc))) => return rmc,
                    other => panic!("expected RMC, got {other:?}"),
                }
            }
        }
        panic!("parser never completed a sentence for {sentence:?}");
    }

    #[track_caller]
    fn assert_sentence_unix(sentence: &str, expected: u64) {
        let rmc = parse_rmc(sentence);
        assert_eq!(
            unix_secs_from_rmc_utc(&rmc.datetime),
            Some(expected),
            "sentence {sentence:?}"
        );
    }

    // The classic NMEA documentation sentence: 1994-11-19T22:54:46Z.
    // Its published checksum (*68) doubles as a positive control that
    // the constructed fixtures below use the same checksum algorithm.
    #[test]
    fn classic_reference_sentence_converts() {
        assert_sentence_unix(
            "$GPRMC,225446,A,4916.45,N,12311.12,W,000.5,054.7,191194,020.3,E*68",
            785_285_686,
        );
    }

    // Year rollover: the last second of 2025 and the first of 2026 are
    // adjacent unix seconds. This is also the midnight-rollover shape at
    // its hardest (day, month and year all change).
    #[test]
    fn year_rollover_is_contiguous() {
        assert_sentence_unix(
            "$GPRMC,235959,A,4916.45,N,12311.12,W,000.5,054.7,311225,020.3,E*69",
            1_767_225_599,
        );
        assert_sentence_unix(
            "$GPRMC,000000,A,4916.45,N,12311.12,W,000.5,054.7,010126,020.3,E*6A",
            1_767_225_600,
        );
    }

    // Plain midnight rollover inside a month.
    #[test]
    fn midnight_rollover_is_contiguous() {
        assert_sentence_unix(
            "$GPRMC,235959,A,4916.45,N,12311.12,W,000.5,054.7,150626,020.3,E*69",
            1_781_567_999,
        );
        assert_sentence_unix(
            "$GPRMC,000000,A,4916.45,N,12311.12,W,000.5,054.7,160626,020.3,E*6B",
            1_781_568_000,
        );
    }

    // Leap day: 2024-02-29T12:00:00Z exists and lands on the right second.
    #[test]
    fn leap_day_converts() {
        assert_sentence_unix(
            "$GPRMC,120000,A,4916.45,N,12311.12,W,000.5,054.7,290224,020.3,E*62",
            1_709_208_000,
        );
    }

    // Fractional seconds truncate toward the past, never round forward.
    #[test]
    fn fractional_seconds_truncate_backwards() {
        let mut rmc =
            parse_rmc("$GPRMC,225446,A,4916.45,N,12311.12,W,000.5,054.7,191194,020.3,E*68");
        rmc.datetime.time.seconds = 46.99;
        assert_eq!(unix_secs_from_rmc_utc(&rmc.datetime), Some(785_285_686));
    }

    // A valid fix seeds exactly once: after the seam accepts one value,
    // later candidates are swallowed by the gate.
    #[test]
    fn seed_gate_seeds_exactly_once() {
        let mut gate = SeedGate::new();
        assert!(!gate.is_seeded());
        assert_eq!(gate.offer(Some(1_790_000_000)), Some(1_790_000_000));
        gate.mark_seeded();
        assert!(gate.is_seeded());
        assert_eq!(gate.offer(Some(1_790_000_060)), None);
    }

    // A refused candidate must leave the gate open: the next fix gets
    // offered again instead of the boot wedging on one garbage value.
    #[test]
    fn refused_candidate_keeps_gate_open() {
        let gate = SeedGate::new();
        assert_eq!(gate.offer(Some(1_000)), Some(1_000));
        // The seam refused it — mark_seeded is NOT called.
        assert_eq!(gate.offer(Some(1_790_000_000)), Some(1_790_000_000));
    }

    // A fix without a time claim offers nothing.
    #[test]
    fn absent_time_offers_nothing() {
        let gate = SeedGate::new();
        assert_eq!(gate.offer(None), None);
    }
}
