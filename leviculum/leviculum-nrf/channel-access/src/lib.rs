#![no_std]
//! The LoRa transmit path's channel-access policy: when a packet that is
//! ready to go may actually key the radio.
//!
//! Two decisions live here, extracted from `lora_task` so a host test can
//! drive them against scripted radios and seeded randomness:
//!
//! 1. **Acquisition jitter** — a randomised pre-TX wait, spent listening,
//!    before the first CAD of a channel acquisition. Two senders whose
//!    transmissions are triggered by the same event (a co-started probe
//!    announce, a rebroadcast of the same received frame) reach their
//!    radios phase-locked; CAD alone cannot separate them, because both
//!    probe a channel on which neither has keyed yet. The jitter de-tiles
//!    them so at most one is inside the other's CAD window.
//!
//! 2. **The CAD retry gate** — the bounded listen-before-talk state
//!    machine around the SX1262's channel-activity detection: clear
//!    transmits, busy backs off a random contention window that doubles
//!    per retry, and after [`CAD_MAX_RETRIES`] attempts the frame is
//!    transmitted anyway. The give-up is deliberate: the layers above
//!    assume the interface eventually transmits, so a frame must not
//!    starve behind a busy channel forever. (The reference firmware
//!    instead waits indefinitely and lets its airtime lock be the only
//!    bound; our bounded forced TX is a deviation, justified because a
//!    silent starvation is indistinguishable from packet loss to every
//!    layer above.)
//!
//! # Reference
//!
//! The jitter's parameters are the RNode firmware's CSMA band-1 (idle
//! channel) draw, taken from the vendored source
//! (`reference/RNode_Firmware`):
//!
//! * every transmission waits DIFS = `CSMA_SIFS_MS + 2 * csma_slot_ms`
//!   with SIFS = 0, i.e. two slots (Config.h:102, Config.h:119), then a
//!   contention window of `random(cw_min, cw_max)` slots
//!   (RNode_Firmware.ino:1625-1627, Arduino `random` upper-exclusive);
//!   at ≤ 7 % channel airtime the band-1 window is `cw_min = 0`,
//!   `cw_max = 14` (Config.h:108-111, RNode_Firmware.ino:1614-1617), so
//!   the idle-channel draw is uniform over 0..=13 slots;
//! * the slot is 12 symbol times, clamped to at most 100 ms and at least
//!   24 ms — or 6 ms when the modulation runs faster than 30 kbps
//!   (Config.h:104-107, Utilities.h:1244-1252, bitrate Utilities.h:1237);
//! * there is **no host-visible way to disable this**: `tx_queue_handler`
//!   (RNode_Firmware.ino:1623) runs it for every queued packet.
//!
//! Only the band-1 draw is mirrored, not the airtime-scaled band
//! escalation: the escalation widens the window under sustained load,
//! which is what the retry gate's doubling contention window already does
//! here, reacting to observed CAD-busy instead of to an airtime average.

/// Maximum CAD attempts before a frame is transmitted even though the
/// channel appears busy — the documented give-up that keeps a frame from
/// starving forever (see the crate docs for why this deviates from the
/// reference's unbounded wait).
pub const CAD_MAX_RETRIES: u8 = 8;
/// Initial contention window (slots) for the CAD backoff. Starting at 2
/// guarantees a non-zero-slot random choice on the first retry so two
/// nodes that simultaneously detect traffic desynchronize meaningfully.
pub const CAD_CW_INITIAL: u8 = 2;
/// Maximum contention window (slots) after exponential back-off.
pub const CAD_CW_MAX: u8 = 64;

/// DIFS in slots: `CSMA_SIFS_MS + 2 * csma_slot_ms` with SIFS = 0
/// (reference Config.h:102, Config.h:119).
pub const JITTER_DIFS_SLOTS: u64 = 2;
/// Number of equally likely jitter draws: band-1 `random(0, 14)` is
/// uniform over 0..=13 slots (reference Config.h:108-111,
/// RNode_Firmware.ino:1626, Arduino `random` upper-exclusive).
pub const JITTER_CW_SLOTS: u32 = 14;
/// One jitter slot is 12 symbol times (reference Config.h:107).
pub const JITTER_SLOT_SYMBOLS: u64 = 12;
/// Slot ceiling in ms (reference Config.h:104).
pub const JITTER_SLOT_MAX_MS: u64 = 100;
/// Slot floor in ms (reference Config.h:105).
pub const JITTER_SLOT_MIN_MS: u64 = 24;
/// Slot floor at fast rates: `CSMA_SLOT_MIN_MS - CSMA_SLOT_MIN_FAST_DELTA`
/// = 24 - 18 (reference Config.h:105-106, Utilities.h:1246).
pub const JITTER_SLOT_MIN_FAST_MS: u64 = 6;
/// Rates above this count as fast (reference Config.h:87,
/// `LORA_FAST_THRESHOLD_BPS`).
pub const JITTER_FAST_THRESHOLD_BPS: u64 = 30_000;

/// The reference's jitter slot for a modulation: 12 symbol times clamped
/// to `[24, 100]` ms, floor 6 ms at rates above 30 kbps (Utilities.h:
/// 1244-1252). Integer ms, computed in µs so SF7's 12.288 ms does not
/// truncate before the clamp compares it.
pub fn jitter_slot_ms(bw_hz: u32, sf: u8, cr_denom: u8) -> u64 {
    // Guard the degenerate configs a corrupt flash page could feed us:
    // a zero divisor must not trap in the radio task.
    if bw_hz == 0 || cr_denom == 0 || sf == 0 || sf > 31 {
        return JITTER_SLOT_MAX_MS;
    }
    let symbol_us = (1u64 << sf) * 1_000_000 / bw_hz as u64;
    let slot_ms = JITTER_SLOT_SYMBOLS * symbol_us / 1_000;
    // Reference bitrate (Utilities.h:1237):
    // sf * (4 / cr) / symbol_time * 1000, in bits per second.
    let bitrate_bps = sf as u64 * 4 * bw_hz as u64 / (cr_denom as u64 * (1u64 << sf));
    let slot_min = if bitrate_bps > JITTER_FAST_THRESHOLD_BPS {
        JITTER_SLOT_MIN_FAST_MS
    } else {
        JITTER_SLOT_MIN_MS
    };
    slot_ms.clamp(slot_min, JITTER_SLOT_MAX_MS)
}

/// What the transmit path must do next, after asking the gate about a CAD
/// outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Key the radio. `forced` is true when the retry budget ran out with
    /// the channel still busy; `retries` is the attempt count for the
    /// `[LORA_CSMA_TX]` line.
    Transmit { forced: bool, retries: u8 },
    /// Listen for this many backoff slots (caller multiplies by its
    /// backoff slot time), then CAD again.
    Backoff { slots: u64 },
    /// CAD itself failed; retry it immediately. Bounded by the same
    /// retry budget as busy, so a radio that cannot CAD still transmits.
    Retry,
}

/// The channel-access state one LoRa transmit path owns: the seeded
/// randomness, the owed acquisition jitter, and the CAD retry gate.
///
/// Everything is a function of the seed and the fed-in CAD outcomes, so a
/// host test scripts a radio ("busy, busy, clear") and asserts the exact
/// decisions.
#[derive(Debug, Clone)]
pub struct ChannelAccess {
    rng: u32,
    jitter_slot_ms: u64,
    jitter_owed: bool,
    attempt: u8,
    cw: u8,
}

/// xorshift32 step. The all-zero state is a fixed point, which is why the
/// constructor refuses a zero seed.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

impl ChannelAccess {
    /// A fresh access state. `seed` must come from per-board entropy (the
    /// hardware RNG): two boards seeded identically draw identical jitter
    /// and identical backoffs at identical draw counts, which re-creates
    /// exactly the phase lock the jitter exists to break. A zero seed is
    /// replaced (xorshift32's zero state is a fixed point).
    ///
    /// Boot counts as a released channel: the very first transmission owes
    /// jitter, because co-started boards are the canonical phase-locked
    /// senders.
    pub const fn new(seed: u32) -> Self {
        Self {
            rng: if seed == 0 { 0x9E37_79B9 } else { seed },
            jitter_slot_ms: JITTER_SLOT_MIN_MS,
            jitter_owed: true,
            attempt: 0,
            cw: CAD_CW_INITIAL,
        }
    }

    /// Adopt a modulation. Called at radio bring-up and on every runtime
    /// reconfiguration, beside the backoff slot recomputation.
    pub fn set_phy(&mut self, bw_hz: u32, sf: u8, cr_denom: u8) {
        self.jitter_slot_ms = jitter_slot_ms(bw_hz, sf, cr_denom);
    }

    /// The jitter slot in force, for the `[LORA_JITTER]` line.
    pub const fn jitter_slot(&self) -> u64 {
        self.jitter_slot_ms
    }

    /// The channel was yielded back (a post-TX listening window ran, or is
    /// about to): the next acquisition owes jitter again.
    pub fn channel_released(&mut self) {
        self.jitter_owed = true;
    }

    /// A new packet starts contending: reset the retry gate. Does not
    /// touch the owed jitter — a burst continuation is a new packet but
    /// not a new acquisition.
    pub fn begin_packet(&mut self) {
        self.attempt = 0;
        self.cw = CAD_CW_INITIAL;
    }

    /// The randomised pre-TX wait for this acquisition, in ms: DIFS plus a
    /// uniform draw of 0..=13 slots ([`JITTER_DIFS_SLOTS`],
    /// [`JITTER_CW_SLOTS`]) the first time it is asked after a channel
    /// release, `0` on every later ask until the next release. The caller
    /// spends the wait listening, so a peer that keys during it is heard,
    /// not talked over.
    pub fn acquisition_jitter_ms(&mut self) -> u64 {
        if !self.jitter_owed {
            return 0;
        }
        self.jitter_owed = false;
        let draw = (xorshift32(&mut self.rng) % JITTER_CW_SLOTS) as u64;
        (JITTER_DIFS_SLOTS + draw) * self.jitter_slot_ms
    }

    /// The retry count so far, for the `[LORA_CAD]` log lines.
    pub const fn retries(&self) -> u8 {
        self.attempt
    }

    /// The channel was probed and found clear: transmit.
    pub fn cad_clear(&mut self) -> Verdict {
        Verdict::Transmit {
            forced: false,
            retries: self.attempt,
        }
    }

    /// The channel was probed and found busy: back off a random number of
    /// slots inside the doubling contention window — or transmit anyway
    /// once the retry budget is spent (the documented give-up).
    pub fn cad_busy(&mut self) -> Verdict {
        self.attempt += 1;
        if self.attempt >= CAD_MAX_RETRIES {
            return Verdict::Transmit {
                forced: true,
                retries: self.attempt,
            };
        }
        let slots = (xorshift32(&mut self.rng) as u64) % (self.cw as u64);
        self.cw = core::cmp::min(self.cw.saturating_mul(2), CAD_CW_MAX);
        Verdict::Backoff { slots }
    }

    /// The CAD operation itself errored: count it against the same budget
    /// and retry immediately (no backoff listen — the radio, not the
    /// channel, refused), forcing the TX once the budget is spent so a
    /// radio that cannot CAD still transmits.
    pub fn cad_error(&mut self) -> Verdict {
        self.attempt += 1;
        if self.attempt >= CAD_MAX_RETRIES {
            return Verdict::Transmit {
                forced: true,
                retries: self.attempt,
            };
        }
        Verdict::Retry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- The reference slot, checked against hand-derived values -------

    #[test]
    fn the_slot_matches_the_reference_at_the_bench_phy() {
        // SF7/BW125: symbol 1.024 ms, 12 symbols = 12.288 ms, bitrate
        // 5468 bps (not fast) -> clamped up to the 24 ms floor. This is
        // the corpus-wide bench PHY, so it is the value the acceptance
        // ladder runs under.
        assert_eq!(jitter_slot_ms(125_000, 7, 5), 24);
    }

    #[test]
    fn the_slot_scales_with_the_symbol_time_between_the_clamps() {
        // SF9/BW125: symbol 4.096 ms -> 49.152 ms -> 49.
        assert_eq!(jitter_slot_ms(125_000, 9, 5), 49);
        // SF10/BW125: symbol 8.192 ms -> 98.304 ms -> 98, just under the
        // ceiling.
        assert_eq!(jitter_slot_ms(125_000, 10, 5), 98);
    }

    #[test]
    fn the_slot_ceiling_binds_at_slow_modulations() {
        // SF12/BW125: 12 symbols are 393 ms; the reference caps the slot
        // at 100 ms (Config.h:104).
        assert_eq!(jitter_slot_ms(125_000, 12, 5), 100);
        assert_eq!(jitter_slot_ms(125_000, 11, 8), 100);
    }

    #[test]
    fn the_fast_rate_floor_is_6_ms() {
        // SF5/BW500: bitrate 5*4*500000/(5*32) = 62500 bps > 30 kbps,
        // symbol 0.064 ms, 12 symbols = 0.768 ms -> fast floor 6 ms
        // (Config.h:105-106).
        assert_eq!(jitter_slot_ms(500_000, 5, 5), 6);
    }

    #[test]
    fn a_degenerate_modulation_cannot_divide_by_zero() {
        // A corrupt flash page decodes to whatever it decodes to; the
        // slot function must stay total.
        assert_eq!(jitter_slot_ms(0, 7, 5), JITTER_SLOT_MAX_MS);
        assert_eq!(jitter_slot_ms(125_000, 0, 5), JITTER_SLOT_MAX_MS);
        assert_eq!(jitter_slot_ms(125_000, 7, 0), JITTER_SLOT_MAX_MS);
        assert_eq!(jitter_slot_ms(125_000, 40, 5), JITTER_SLOT_MAX_MS);
    }

    // --- The acquisition jitter ----------------------------------------

    #[test]
    fn boot_owes_jitter_and_the_draw_is_difs_plus_a_bounded_window() {
        // Every seed must land inside DIFS + 0..=13 slots; run a spread
        // of seeds so the bound is a property, not a lucky draw.
        for seed in 1..=1_000u32 {
            let mut access = ChannelAccess::new(seed);
            access.set_phy(125_000, 7, 5);
            let jitter = access.acquisition_jitter_ms();
            assert!(jitter.is_multiple_of(24), "whole slots only, got {jitter}");
            assert!(
                (48..=48 + 13 * 24).contains(&jitter),
                "seed {seed} drew {jitter}"
            );
        }
    }

    #[test]
    fn jitter_is_owed_once_per_acquisition_not_once_per_packet() {
        let mut access = ChannelAccess::new(7);
        access.set_phy(125_000, 7, 5);
        assert!(access.acquisition_jitter_ms() > 0);
        // Burst continuation: more packets, same acquisition, no new wait.
        access.begin_packet();
        assert_eq!(access.acquisition_jitter_ms(), 0);
        access.begin_packet();
        assert_eq!(access.acquisition_jitter_ms(), 0);
        // The channel is yielded back: the next acquisition owes again.
        access.channel_released();
        assert!(access.acquisition_jitter_ms() > 0);
    }

    #[test]
    fn the_same_seed_draws_the_same_jitter_and_different_seeds_de_tile() {
        // Determinism is what makes the draw testable; per-board seeds are
        // what make it useful. Both properties in one place: identical
        // seeds reproduce, and across a small population of seeds more
        // than one distinct value appears.
        let draw = |seed: u32| {
            let mut access = ChannelAccess::new(seed);
            access.set_phy(125_000, 7, 5);
            access.acquisition_jitter_ms()
        };
        assert_eq!(draw(42), draw(42));
        let mut distinct = [false; 14];
        for seed in 1..=100u32 {
            distinct[((draw(seed) - 48) / 24) as usize] = true;
        }
        assert!(
            distinct.iter().filter(|d| **d).count() > 1,
            "a hundred boards all drew the same slot"
        );
    }

    #[test]
    fn a_zero_seed_is_replaced_not_a_fixed_point() {
        // xorshift32(0) == 0 forever; the constructor must not let a
        // zeroed RNG turn the jitter into a constant.
        let mut access = ChannelAccess::new(0);
        access.set_phy(125_000, 7, 5);
        let first = access.acquisition_jitter_ms();
        access.channel_released();
        let second = access.acquisition_jitter_ms();
        access.channel_released();
        let third = access.acquisition_jitter_ms();
        // Not all three equal — the RNG is stepping.
        assert!(
            !(first == second && second == third),
            "zero-seeded RNG is stuck at {first}"
        );
    }

    #[test]
    fn the_draw_is_roughly_uniform_over_the_fourteen_slots() {
        // 14_000 draws over the 14 values: each bucket should be near
        // 1000. A crude 3-sigma-ish band suffices to catch a modulo or
        // shift mistake without turning the test statistical.
        let mut access = ChannelAccess::new(0xC0FF_EE11);
        access.set_phy(125_000, 7, 5);
        let mut buckets = [0u32; 14];
        for _ in 0..14_000 {
            access.channel_released();
            let jitter = access.acquisition_jitter_ms();
            buckets[((jitter - 48) / 24) as usize] += 1;
        }
        for (slot, count) in buckets.iter().enumerate() {
            assert!(
                (900..=1100).contains(count),
                "slot {slot} drew {count} of 14000"
            );
        }
    }

    // --- The CAD retry gate --------------------------------------------

    #[test]
    fn a_clear_channel_transmits_unforced() {
        let mut access = ChannelAccess::new(3);
        access.begin_packet();
        assert_eq!(
            access.cad_clear(),
            Verdict::Transmit {
                forced: false,
                retries: 0
            }
        );
    }

    #[test]
    fn busy_backs_off_inside_the_doubling_window_and_then_gives_up() {
        // The scripted radio: busy on every probe. The gate must back off
        // with slot counts inside the advertised window (2, 4, 8, ... 64)
        // and force the TX on the 8th attempt — the frame must not starve.
        let mut access = ChannelAccess::new(0xB00_57ED);
        access.begin_packet();
        let mut cw = CAD_CW_INITIAL as u64;
        for attempt in 1..CAD_MAX_RETRIES {
            match access.cad_busy() {
                Verdict::Backoff { slots } => {
                    assert!(slots < cw, "attempt {attempt}: {slots} >= window {cw}");
                    cw = core::cmp::min(cw * 2, CAD_CW_MAX as u64);
                }
                other => panic!("attempt {attempt}: expected backoff, got {other:?}"),
            }
        }
        assert_eq!(
            access.cad_busy(),
            Verdict::Transmit {
                forced: true,
                retries: CAD_MAX_RETRIES
            }
        );
    }

    #[test]
    fn a_new_packet_resets_the_retry_budget() {
        let mut access = ChannelAccess::new(9);
        access.begin_packet();
        for _ in 0..CAD_MAX_RETRIES {
            access.cad_busy();
        }
        access.begin_packet();
        assert_eq!(access.retries(), 0);
        // With a fresh budget a busy can never force.
        assert!(matches!(access.cad_busy(), Verdict::Backoff { .. }));
    }

    #[test]
    fn cad_errors_spend_the_same_budget_and_still_transmit() {
        // A radio that cannot CAD (SPI fault, chip wedged) must not
        // starve the frame either: errors retry immediately and force at
        // the same bound.
        let mut access = ChannelAccess::new(11);
        access.begin_packet();
        for _ in 1..CAD_MAX_RETRIES {
            assert_eq!(access.cad_error(), Verdict::Retry);
        }
        assert_eq!(
            access.cad_error(),
            Verdict::Transmit {
                forced: true,
                retries: CAD_MAX_RETRIES
            }
        );
    }

    #[test]
    fn busy_and_error_mix_shares_one_budget() {
        // 4 busies and 4 errors together exhaust the 8-attempt budget.
        let mut access = ChannelAccess::new(13);
        access.begin_packet();
        for _ in 0..4 {
            assert!(matches!(access.cad_busy(), Verdict::Backoff { .. }));
        }
        for _ in 0..3 {
            assert_eq!(access.cad_error(), Verdict::Retry);
        }
        assert_eq!(
            access.cad_error(),
            Verdict::Transmit {
                forced: true,
                retries: CAD_MAX_RETRIES
            }
        );
    }
}
