#![no_std]
//! The on-air spacing knob for the LoRa transmit path (Codeberg #345).
//!
//! A measurement instrument, not a mechanism. The board emits a telemetry
//! announce and its report back to back; the relays hear the announce and
//! not the report, and the threshold at which that stops is somewhere in a
//! window no bench observation covers. This crate is the one number that
//! moves the second packet away from the first so the window can be swept.
//!
//! # What "spacing" means here
//!
//! The gap is measured **from the end of the previous packet's airtime to
//! the moment the radio is keyed for the next one** — the same two edges a
//! receiver sees on the air, not the moment a packet is handed to the
//! interface. [`TxSpacer::note_tx_end`] is called when the last frame of a
//! packet has left the air; [`TxSpacer::wait_ms`] is asked, immediately
//! before key-up, how much of the requested gap is still owed.
//!
//! Because the wait is what is *left* of the spacing, everything the
//! transmit path already spends between two packets (the CAD, the SPI
//! traffic, the log lines) is **absorbed** into the gap rather than added
//! on top of it. So a requested 60 ms is a 60 ms gap on the air, and a
//! requested value below what the path costs anyway asks for no wait at
//! all — which is why [`DEFAULT_TX_SPACING_MS`] leaves today's behaviour
//! exactly as it is.
//!
//! # What this is not
//!
//! No retry, no jitter, no protective floor. `wait_ms` returns one
//! deterministic number from the configured spacing and the clock, and a
//! spacing of `0` returns `0` always.

/// The compiled default: no spacing is imposed, so the gap between two
/// packets is whatever the transmit path costs on its own.
///
/// The knob's whole point is that the default behaviour is unchanged
/// until a sweep sets a value, so this is `0` and nothing else. A board
/// that is never told otherwise transmits exactly as it did before the
/// knob existed.
pub const DEFAULT_TX_SPACING_MS: u16 = 0;

/// Tracks the end of the last packet's airtime and answers how long the
/// transmit path still owes before it may key the radio again.
///
/// Times are plain milliseconds from an arbitrary monotonic origin (the
/// firmware passes `embassy_time::Instant::now().as_millis()`), so the
/// whole decision is a function of two integers and is testable on the
/// host against a fake clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxSpacer {
    spacing_ms: u16,
    last_tx_end_ms: Option<u64>,
}

impl TxSpacer {
    /// A spacer that has not transmitted yet.
    pub const fn new(spacing_ms: u16) -> Self {
        Self {
            spacing_ms,
            last_tx_end_ms: None,
        }
    }

    /// The configured spacing, for the log line and the report.
    pub const fn spacing_ms(&self) -> u16 {
        self.spacing_ms
    }

    /// Take a new spacing. Applies to the next key-up; a packet already on
    /// the air is not affected, because its gap was decided when it was
    /// keyed.
    pub fn set_spacing_ms(&mut self, spacing_ms: u16) {
        self.spacing_ms = spacing_ms;
    }

    /// How long the caller must wait before keying the radio, in ms.
    ///
    /// `0` when the spacing is `0`, when nothing has been transmitted yet
    /// (there is no previous packet to be spaced from), or when the
    /// requested gap has already elapsed on its own.
    pub fn wait_ms(&self, now_ms: u64) -> u64 {
        let Some(last) = self.last_tx_end_ms else {
            return 0;
        };
        // saturating: a clock that appears to move backwards must not turn
        // into an enormous wait, it must ask for the full spacing.
        let elapsed = now_ms.saturating_sub(last);
        (self.spacing_ms as u64).saturating_sub(elapsed)
    }

    /// Record that a packet's last frame has just left the air.
    pub fn note_tx_end(&mut self, now_ms: u64) {
        self.last_tx_end_ms = Some(now_ms);
    }

    /// The gap actually achieved: previous packet's airtime end to this
    /// key-up. `None` before the first transmission, where there is no
    /// previous edge to measure from.
    ///
    /// Reported rather than assumed equal to [`spacing_ms`](Self::spacing_ms)
    /// so a sweep reads the number off the board instead of inferring it
    /// from the value it set.
    pub fn achieved_gap_ms(&self, key_ms: u64) -> Option<u64> {
        self.last_tx_end_ms.map(|last| key_ms.saturating_sub(last))
    }
}

impl Default for TxSpacer {
    fn default() -> Self {
        Self::new(DEFAULT_TX_SPACING_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The transmit path's clock, faked: the tests move it by hand, so
    /// every assertion below is on a number and not on a sleep.
    struct FakeClock {
        now_ms: u64,
    }

    impl FakeClock {
        fn new() -> Self {
            Self { now_ms: 1_000 }
        }
        fn now_ms(&self) -> u64 {
            self.now_ms
        }
        fn advance(&mut self, ms: u64) {
            self.now_ms += ms;
        }
    }

    /// Everything the transmit path does around one packet, so the tests
    /// read as the sequence the firmware runs: wait what is owed, key,
    /// spend the airtime, record the end.
    fn send_packet(spacer: &mut TxSpacer, clock: &mut FakeClock, airtime_ms: u64) -> u64 {
        let waited = spacer.wait_ms(clock.now_ms());
        clock.advance(waited);
        let gap = spacer.achieved_gap_ms(clock.now_ms()).unwrap_or(0);
        clock.advance(airtime_ms);
        spacer.note_tx_end(clock.now_ms());
        gap
    }

    #[test]
    fn the_default_imposes_no_wait_at_all() {
        // The control: at the compiled default the spacer is inert, so a
        // board that is never configured transmits exactly as before.
        assert_eq!(DEFAULT_TX_SPACING_MS, 0);
        let mut clock = FakeClock::new();
        let mut spacer = TxSpacer::default();
        assert_eq!(spacer.spacing_ms(), 0);
        assert_eq!(send_packet(&mut spacer, &mut clock, 200), 0);
        // Second packet, immediately after the first: still no wait.
        assert_eq!(spacer.wait_ms(clock.now_ms()), 0);
        assert_eq!(send_packet(&mut spacer, &mut clock, 200), 0);
    }

    #[test]
    fn the_first_packet_is_never_delayed() {
        // There is no previous packet to be spaced from, so even a large
        // spacing must not hold the very first transmission.
        let clock = FakeClock::new();
        let spacer = TxSpacer::new(5_000);
        assert_eq!(spacer.wait_ms(clock.now_ms()), 0);
        assert_eq!(spacer.achieved_gap_ms(clock.now_ms()), None);
    }

    #[test]
    fn a_set_spacing_is_the_gap_the_second_packet_gets() {
        let mut clock = FakeClock::new();
        let mut spacer = TxSpacer::new(60);
        send_packet(&mut spacer, &mut clock, 200);
        // The pair: the second packet is keyed exactly 60 ms after the
        // first left the air.
        assert_eq!(spacer.wait_ms(clock.now_ms()), 60);
        assert_eq!(send_packet(&mut spacer, &mut clock, 200), 60);
    }

    #[test]
    fn time_the_transmit_path_already_spent_is_absorbed_not_added() {
        // The CAD, the SPI traffic and the log lines between two packets
        // cost ~15 ms today. A 60 ms request has to be a 60 ms gap on the
        // air, not 75.
        let mut clock = FakeClock::new();
        let mut spacer = TxSpacer::new(60);
        send_packet(&mut spacer, &mut clock, 200);
        clock.advance(15);
        assert_eq!(spacer.wait_ms(clock.now_ms()), 45);
        clock.advance(45);
        assert_eq!(spacer.achieved_gap_ms(clock.now_ms()), Some(60));
    }

    #[test]
    fn a_spacing_the_path_already_exceeds_asks_for_nothing() {
        // Below the floor the path costs anyway, the knob is inert: this is
        // why every value at or under today's ~15 ms reproduces today's
        // behaviour rather than shortening it.
        let mut clock = FakeClock::new();
        let mut spacer = TxSpacer::new(10);
        send_packet(&mut spacer, &mut clock, 200);
        clock.advance(15);
        assert_eq!(spacer.wait_ms(clock.now_ms()), 0);
        assert_eq!(spacer.achieved_gap_ms(clock.now_ms()), Some(15));
    }

    #[test]
    fn zero_and_a_large_value_both_do_what_they_say() {
        let mut clock = FakeClock::new();
        let mut spacer = TxSpacer::new(0);
        send_packet(&mut spacer, &mut clock, 200);
        assert_eq!(spacer.wait_ms(clock.now_ms()), 0);

        spacer.set_spacing_ms(u16::MAX);
        assert_eq!(spacer.spacing_ms(), 65_535);
        assert_eq!(spacer.wait_ms(clock.now_ms()), 65_535);
        assert_eq!(send_packet(&mut spacer, &mut clock, 200), 65_535);

        // And back to zero, which is how a sweep returns the board to the
        // default without a reflash.
        spacer.set_spacing_ms(0);
        assert_eq!(spacer.wait_ms(clock.now_ms()), 0);
    }

    #[test]
    fn a_new_spacing_applies_from_the_next_key_up() {
        let mut clock = FakeClock::new();
        let mut spacer = TxSpacer::new(0);
        send_packet(&mut spacer, &mut clock, 200);
        spacer.set_spacing_ms(80);
        assert_eq!(spacer.wait_ms(clock.now_ms()), 80);
    }

    #[test]
    fn every_packet_is_spaced_from_the_previous_one_not_from_the_first() {
        let mut clock = FakeClock::new();
        let mut spacer = TxSpacer::new(40);
        send_packet(&mut spacer, &mut clock, 200);
        for _ in 0..3 {
            assert_eq!(send_packet(&mut spacer, &mut clock, 200), 40);
        }
    }

    #[test]
    fn a_clock_that_appears_to_go_backwards_asks_for_the_full_spacing() {
        // Cannot happen behind a monotonic Instant, and must not become a
        // 49-day wait if it ever does.
        let mut spacer = TxSpacer::new(50);
        spacer.note_tx_end(10_000);
        assert_eq!(spacer.wait_ms(9_000), 50);
        assert_eq!(spacer.achieved_gap_ms(9_000), Some(0));
    }
}
