//! Receiver-side resource sliding-window state and adaptation policy.
//!
//! Codeberg #85: the window adaptation logic is extracted out of
//! `IncomingResource` into a swappable
//! [`WindowPolicy`] so candidate algorithms can be benchmarked against each
//! other in the same harness. [`WindowPolicy::Current`] reproduces the
//! historical behavior exactly; [`WindowPolicy::PythonLike`] mirrors the
//! Python-RNS reference algorithm as the baseline to beat.

use crate::constants::{
    RESOURCE_WINDOW_INITIAL, RESOURCE_WINDOW_MAX_FAST, RESOURCE_WINDOW_MAX_SLOW,
    RESOURCE_WINDOW_MIN,
};
use crate::resource::{
    FAST_RATE_THRESHOLD, RESOURCE_WINDOW_FLEXIBILITY, RESOURCE_WINDOW_MAX_VERY_SLOW,
    SLOW_RATE_THRESHOLD, VERY_SLOW_RATE_THRESHOLD,
};

/// Python Resource.RATE_FAST: (50 * 1000) / 8 B/s. Round goodput above this
/// counts toward the fast tier.
const RATE_FAST: u64 = 50 * 1000 / 8;

/// Python Resource.RATE_VERY_SLOW: (2 * 1000) / 8 B/s. Round goodput below
/// this counts toward the very-slow tier.
const RATE_VERY_SLOW: u64 = 2 * 1000 / 8;

/// Python Resource.FAST_RATE_THRESHOLD = WINDOW_MAX_SLOW - WINDOW - 2: rounds
/// of sustained fast rate before window_max opens to WINDOW_MAX_FAST.
const FAST_RATE_ROUNDS: usize = RESOURCE_WINDOW_MAX_SLOW - RESOURCE_WINDOW_INITIAL - 2;

/// Python Resource.VERY_SLOW_RATE_THRESHOLD: rounds of sustained very slow
/// rate before window_max is capped to WINDOW_MAX_VERY_SLOW.
const VERY_SLOW_RATE_ROUNDS: usize = 2;

/// Receiver-side rate measurements for one completed window round.
///
/// `first_part_rate` is the classic eifr quantity: bytes_per_part * 1000 /
/// elapsed_ms between sending the REQ and the first part of the window
/// arriving. `round_rate` is the whole-round goodput: total bytes received in
/// this window divided by the round's elapsed time. Policies choose which
/// measurement to react to; [`WindowPolicy::Current`] uses `first_part_rate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSample {
    /// bytes_per_part * 1000 / ms from REQ to the window's first part (B/s).
    pub first_part_rate: u64,
    /// Bytes received this window * 1000 / round elapsed ms (B/s).
    pub round_rate: u64,
}

/// Selects the receive-window adaptation algorithm (Codeberg #85).
///
/// Candidate algorithms are added as further variants and selected via the
/// `LEVICULUM_RESOURCE_WINDOW_POLICY` environment variable in `lnsd` and
/// `lncp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowPolicy {
    /// The historical algorithm: grow the window by 1 once
    /// `window + FLEXIBILITY` consecutive rounds completed, pick the
    /// window_max tier from the first-part rate, clamp down on tier drop.
    /// Timeouts never touch the window.
    #[default]
    Current,
    /// Mirror of the Python-RNS reference algorithm (Resource.py): grow the
    /// window by 1 every completed round (with window_min creep), pick the
    /// window_max tier from the whole-round goodput with round-count
    /// hysteresis, shrink window and window_max on receiver timeout.
    PythonLike,
    /// PythonLike growth and rate tiering, but the timeout response shrinks
    /// only the in-flight window toward window_min and leaves window_max
    /// intact, so a transient loss does not permanently lower the ceiling
    /// the window can re-grow to.
    Adaptive,
}

impl WindowPolicy {
    /// Parse a policy name as used by `LEVICULUM_RESOURCE_WINDOW_POLICY`.
    /// Returns `None` for unknown names so callers can warn and fall back.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "current" => Some(Self::Current),
            "pythonlike" => Some(Self::PythonLike),
            "adaptive" => Some(Self::Adaptive),
            _ => None,
        }
    }
}

/// The receive-window state moved out of `IncomingResource`.
///
/// All mutation of the window happens here, driven by the owning
/// `IncomingResource` at three points: parts arriving (`record_part`), the
/// round completing (`on_round_complete`), and the request timing out
/// (`on_timeout`).
pub(crate) struct WindowState {
    /// Current window size: number of parts requested per REQ.
    window: usize,
    /// Ceiling the window may grow to, selected by rate tier.
    window_max: usize,
    /// Parts received since the current REQ was built.
    parts_received_this_window: usize,
    /// Bytes received since the current REQ was built (for round_rate).
    bytes_received_this_window: u64,
    /// Rounds completed since the window last grew.
    consecutive_completed_windows: usize,
    /// Total completed rounds over the transfer (observability only).
    rounds_completed: usize,
    /// Floor the window may shrink to; creeps up as the window grows
    /// (Python's window_min). Only PythonLike uses it, Current ignores it.
    window_min: usize,
    /// Rounds with round goodput above RATE_FAST (tier hysteresis).
    /// Only PythonLike uses it, Current ignores it.
    fast_rate_rounds: usize,
    /// Rounds with round goodput below RATE_VERY_SLOW (tier hysteresis).
    /// Only PythonLike uses it, Current ignores it.
    very_slow_rate_rounds: usize,
}

impl WindowState {
    pub(crate) fn new() -> Self {
        Self {
            window: RESOURCE_WINDOW_INITIAL,
            window_max: RESOURCE_WINDOW_MAX_SLOW,
            parts_received_this_window: 0,
            bytes_received_this_window: 0,
            consecutive_completed_windows: 0,
            rounds_completed: 0,
            window_min: RESOURCE_WINDOW_MIN,
            fast_rate_rounds: 0,
            very_slow_rate_rounds: 0,
        }
    }

    pub(crate) fn window(&self) -> usize {
        self.window
    }

    pub(crate) fn window_max(&self) -> usize {
        self.window_max
    }

    pub(crate) fn parts_received_this_window(&self) -> usize {
        self.parts_received_this_window
    }

    pub(crate) fn bytes_received_this_window(&self) -> u64 {
        self.bytes_received_this_window
    }

    pub(crate) fn rounds_completed(&self) -> usize {
        self.rounds_completed
    }

    /// A part of the current window arrived.
    pub(crate) fn record_part(&mut self, bytes: usize) {
        self.parts_received_this_window += 1;
        self.bytes_received_this_window += bytes as u64;
    }

    /// A new REQ was built: the per-round counters restart.
    pub(crate) fn start_round(&mut self) {
        self.parts_received_this_window = 0;
        self.bytes_received_this_window = 0;
    }

    /// All outstanding parts of the current window arrived.
    pub(crate) fn on_round_complete(&mut self, policy: WindowPolicy, sample: RateSample) {
        self.rounds_completed += 1;
        match policy {
            WindowPolicy::Current => self.round_complete_current(sample.first_part_rate),
            // Adaptive shares PythonLike's round-complete logic verbatim so
            // the two cannot drift; they differ only in on_timeout.
            WindowPolicy::PythonLike | WindowPolicy::Adaptive => {
                self.round_complete_pythonlike(sample.round_rate)
            }
        }
    }

    /// The part request timed out and is being retransmitted.
    pub(crate) fn on_timeout(&mut self, policy: WindowPolicy) {
        match policy {
            // The historical logic never touches the window on timeout.
            WindowPolicy::Current => {}
            WindowPolicy::PythonLike => self.timeout_pythonlike(),
            WindowPolicy::Adaptive => self.timeout_adaptive(),
        }
    }

    /// The historical round-complete logic, verbatim: bump the completed-round
    /// counter, grow the window by 1 once it reaches window + FLEXIBILITY,
    /// then pick the window_max tier from the first-part rate, clamping the
    /// window down if it exceeds a lowered window_max.
    fn round_complete_current(&mut self, rate: u64) {
        self.consecutive_completed_windows += 1;

        if self.consecutive_completed_windows >= self.window + RESOURCE_WINDOW_FLEXIBILITY {
            if self.window < self.window_max {
                self.window += 1;
            }
            self.consecutive_completed_windows = 0;
        }

        if rate > FAST_RATE_THRESHOLD {
            self.window_max = RESOURCE_WINDOW_MAX_FAST;
        } else if rate < VERY_SLOW_RATE_THRESHOLD {
            self.window_max = RESOURCE_WINDOW_MAX_VERY_SLOW;
            if self.window > self.window_max {
                self.window = self.window_max;
            }
        } else if rate < SLOW_RATE_THRESHOLD {
            self.window_max = RESOURCE_WINDOW_MAX_SLOW;
            if self.window > self.window_max {
                self.window = self.window_max;
            }
        }
    }

    /// Python's round-complete logic (Resource.py receive_part, outstanding
    /// == 0 branch): grow the window by 1 every round while below window_max,
    /// creeping window_min up behind it, THEN update the tier counters from
    /// the whole-round goodput. Growth precedes the tier update, exactly as
    /// in the reference, so the round that opens a tier only grows the window
    /// from the next round on. Python never clamps the window down when the
    /// very-slow cap engages; neither does this mirror.
    fn round_complete_pythonlike(&mut self, round_rate: u64) {
        if self.window < self.window_max {
            self.window += 1;
            if (self.window - self.window_min) > (RESOURCE_WINDOW_FLEXIBILITY - 1) {
                self.window_min += 1;
            }
        }

        if round_rate > RATE_FAST && self.fast_rate_rounds < FAST_RATE_ROUNDS {
            self.fast_rate_rounds += 1;
            if self.fast_rate_rounds == FAST_RATE_ROUNDS {
                self.window_max = RESOURCE_WINDOW_MAX_FAST;
            }
        }

        if self.fast_rate_rounds == 0
            && round_rate < RATE_VERY_SLOW
            && self.very_slow_rate_rounds < VERY_SLOW_RATE_ROUNDS
        {
            self.very_slow_rate_rounds += 1;
            if self.very_slow_rate_rounds == VERY_SLOW_RATE_ROUNDS {
                self.window_max = RESOURCE_WINDOW_MAX_VERY_SLOW;
            }
        }
    }

    /// Python's receiver-timeout shrink (Resource.py:616-621): step the
    /// window down toward window_min and pull window_max down with it, twice
    /// if the max-to-window gap exceeds the flexibility.
    fn timeout_pythonlike(&mut self) {
        if self.window > self.window_min {
            self.window -= 1;
            if self.window_max > self.window_min {
                self.window_max -= 1;
                // The window may sit above window_max (the very-slow cap lowers
                // window_max without clamping the window, mirroring Python).
                // Python compares a float that simply goes negative here; use a
                // saturating diff so `window > window_max` yields 0 (no double
                // step) rather than a usize underflow panic.
                if self.window_max.saturating_sub(self.window) > (RESOURCE_WINDOW_FLEXIBILITY - 1) {
                    self.window_max -= 1;
                }
            }
        }
    }

    /// Adaptive's timeout response: step the in-flight window down toward
    /// window_min to relieve burst pressure on a lossy half-duplex channel,
    /// but leave window_max alone so the window re-grows to the full ceiling
    /// as soon as the loss passes. Contrast timeout_pythonlike, which drags
    /// window_max down with the window and permanently throttles recovery.
    fn timeout_adaptive(&mut self) {
        if self.window > self.window_min {
            self.window -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(first_part_rate: u64) -> RateSample {
        RateSample {
            first_part_rate,
            round_rate: 0,
        }
    }

    #[test]
    fn test_initial_state() {
        let ws = WindowState::new();
        assert_eq!(ws.window(), RESOURCE_WINDOW_INITIAL);
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_SLOW);
        assert_eq!(ws.parts_received_this_window(), 0);
        assert_eq!(ws.rounds_completed(), 0);
    }

    #[test]
    fn test_current_growth_cadence() {
        // window grows by 1 after window + FLEXIBILITY completed rounds.
        let mut ws = WindowState::new();
        let rate = SLOW_RATE_THRESHOLD; // mid tier: window_max untouched
        for _ in 0..(RESOURCE_WINDOW_INITIAL + RESOURCE_WINDOW_FLEXIBILITY - 1) {
            ws.on_round_complete(WindowPolicy::Current, sample(rate));
            assert_eq!(ws.window(), RESOURCE_WINDOW_INITIAL);
        }
        ws.on_round_complete(WindowPolicy::Current, sample(rate));
        assert_eq!(ws.window(), RESOURCE_WINDOW_INITIAL + 1);
    }

    #[test]
    fn test_current_tier_selection_and_clamp() {
        let mut ws = WindowState::new();
        ws.on_round_complete(WindowPolicy::Current, sample(FAST_RATE_THRESHOLD + 1));
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_FAST);
        // Grow the window past the very-slow ceiling, then drop the tier.
        while ws.window() <= RESOURCE_WINDOW_MAX_VERY_SLOW {
            ws.on_round_complete(WindowPolicy::Current, sample(FAST_RATE_THRESHOLD + 1));
        }
        ws.on_round_complete(WindowPolicy::Current, sample(VERY_SLOW_RATE_THRESHOLD - 1));
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_VERY_SLOW);
        assert_eq!(ws.window(), RESOURCE_WINDOW_MAX_VERY_SLOW);
    }

    #[test]
    fn test_current_mid_tier_keeps_window_max() {
        let mut ws = WindowState::new();
        ws.on_round_complete(WindowPolicy::Current, sample(FAST_RATE_THRESHOLD + 1));
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_FAST);
        // Between SLOW and FAST thresholds the tier is left as-is.
        ws.on_round_complete(WindowPolicy::Current, sample(SLOW_RATE_THRESHOLD));
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_FAST);
    }

    #[test]
    fn test_current_timeout_is_noop() {
        let mut ws = WindowState::new();
        ws.on_round_complete(WindowPolicy::Current, sample(FAST_RATE_THRESHOLD + 1));
        let (w, m) = (ws.window(), ws.window_max());
        ws.on_timeout(WindowPolicy::Current);
        assert_eq!((ws.window(), ws.window_max()), (w, m));
    }

    #[test]
    fn test_round_counters() {
        let mut ws = WindowState::new();
        ws.record_part(464);
        ws.record_part(464);
        assert_eq!(ws.parts_received_this_window(), 2);
        assert_eq!(ws.bytes_received_this_window(), 928);
        ws.start_round();
        assert_eq!(ws.parts_received_this_window(), 0);
        assert_eq!(ws.bytes_received_this_window(), 0);
    }

    #[test]
    fn test_policy_parse() {
        assert_eq!(WindowPolicy::parse("current"), Some(WindowPolicy::Current));
        assert_eq!(
            WindowPolicy::parse(" Current "),
            Some(WindowPolicy::Current)
        );
        assert_eq!(
            WindowPolicy::parse("pythonlike"),
            Some(WindowPolicy::PythonLike)
        );
        assert_eq!(
            WindowPolicy::parse(" PythonLike "),
            Some(WindowPolicy::PythonLike)
        );
        assert_eq!(
            WindowPolicy::parse("adaptive"),
            Some(WindowPolicy::Adaptive)
        );
        assert_eq!(
            WindowPolicy::parse(" Adaptive "),
            Some(WindowPolicy::Adaptive)
        );
        assert_eq!(WindowPolicy::parse("bogus"), None);
        assert_eq!(WindowPolicy::parse(""), None);
    }

    #[test]
    fn test_default_policy_is_current() {
        assert_eq!(WindowPolicy::default(), WindowPolicy::Current);
    }

    fn round_sample(round_rate: u64) -> RateSample {
        RateSample {
            first_part_rate: 0,
            round_rate,
        }
    }

    /// A mid rate: neither fast nor very slow, window_max stays SLOW.
    const MID_RATE: u64 = RATE_VERY_SLOW + 1;

    #[test]
    fn test_pythonlike_grows_every_round_with_window_min_creep() {
        let mut ws = WindowState::new();
        for round in 1..=(RESOURCE_WINDOW_MAX_SLOW - RESOURCE_WINDOW_INITIAL) {
            ws.on_round_complete(WindowPolicy::PythonLike, round_sample(MID_RATE));
            assert_eq!(ws.window(), RESOURCE_WINDOW_INITIAL + round);
            // window_min trails the window at flexibility - 1 behind, once
            // the window has moved that far from WINDOW_MIN.
            let expected_min = RESOURCE_WINDOW_MIN
                .max(ws.window().saturating_sub(RESOURCE_WINDOW_FLEXIBILITY - 1));
            assert_eq!(ws.window_min, expected_min);
        }
        assert_eq!(ws.window(), RESOURCE_WINDOW_MAX_SLOW);
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_SLOW);
        // At the SLOW ceiling the window stops growing.
        ws.on_round_complete(WindowPolicy::PythonLike, round_sample(MID_RATE));
        assert_eq!(ws.window(), RESOURCE_WINDOW_MAX_SLOW);
    }

    #[test]
    fn test_pythonlike_fast_tier_needs_sustained_rounds() {
        let mut ws = WindowState::new();
        for _ in 0..(FAST_RATE_ROUNDS - 1) {
            ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_FAST + 1));
            assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_SLOW);
        }
        ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_FAST + 1));
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_FAST);
        // A rate exactly at the threshold must not count (strict >).
        let mut ws = WindowState::new();
        for _ in 0..(FAST_RATE_ROUNDS + 1) {
            ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_FAST));
        }
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_SLOW);
    }

    #[test]
    fn test_pythonlike_very_slow_tier_caps_after_two_rounds() {
        let mut ws = WindowState::new();
        ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_VERY_SLOW - 1));
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_SLOW);
        ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_VERY_SLOW - 1));
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_VERY_SLOW);
        // Python leaves the already grown window untouched (no clamp): two
        // growth rounds happened before the cap engaged.
        assert_eq!(ws.window(), RESOURCE_WINDOW_INITIAL + 2);
        // Capped below the window, it can no longer grow.
        ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_VERY_SLOW - 1));
        assert_eq!(ws.window(), RESOURCE_WINDOW_INITIAL + 2);
    }

    #[test]
    fn test_pythonlike_fast_history_blocks_very_slow_cap() {
        let mut ws = WindowState::new();
        ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_FAST + 1));
        // fast_rate_rounds > 0: very slow rounds no longer count.
        for _ in 0..(VERY_SLOW_RATE_ROUNDS + 2) {
            ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_VERY_SLOW - 1));
        }
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_SLOW);
    }

    #[test]
    fn test_pythonlike_timeout_shrinks_window_and_max() {
        let mut ws = WindowState::new();
        // Grow to the SLOW ceiling first.
        for _ in 0..(RESOURCE_WINDOW_MAX_SLOW - RESOURCE_WINDOW_INITIAL) {
            ws.on_round_complete(WindowPolicy::PythonLike, round_sample(MID_RATE));
        }
        assert_eq!((ws.window(), ws.window_max()), (10, 10));
        ws.on_timeout(WindowPolicy::PythonLike);
        // window 10 -> 9, window_max 10 -> 9 (gap 0, no second step).
        assert_eq!((ws.window(), ws.window_max()), (9, 9));
        // Shrink to the floor: window never drops below window_min.
        for _ in 0..20 {
            ws.on_timeout(WindowPolicy::PythonLike);
        }
        assert_eq!(ws.window(), ws.window_min);
        assert!(ws.window_max() >= ws.window_min);
        ws.on_timeout(WindowPolicy::PythonLike);
        assert_eq!(ws.window(), ws.window_min);
    }

    #[test]
    fn test_adaptive_round_complete_matches_pythonlike() {
        // Adaptive and PythonLike share the round-complete logic verbatim:
        // identical state after any round sequence spanning all tiers.
        let mut py = WindowState::new();
        let mut ad = WindowState::new();
        let rates = [
            MID_RATE,
            RATE_FAST + 1,
            RATE_VERY_SLOW - 1,
            MID_RATE,
            RATE_VERY_SLOW - 1,
            RATE_FAST + 1,
        ];
        for rate in rates {
            py.on_round_complete(WindowPolicy::PythonLike, round_sample(rate));
            ad.on_round_complete(WindowPolicy::Adaptive, round_sample(rate));
            assert_eq!(
                (py.window(), py.window_max(), py.window_min),
                (ad.window(), ad.window_max(), ad.window_min)
            );
        }
    }

    #[test]
    fn test_adaptive_timeout_shrinks_window_but_keeps_window_max() {
        let mut ws = WindowState::new();
        for _ in 0..(RESOURCE_WINDOW_MAX_SLOW - RESOURCE_WINDOW_INITIAL) {
            ws.on_round_complete(WindowPolicy::Adaptive, round_sample(MID_RATE));
        }
        assert_eq!((ws.window(), ws.window_max()), (10, 10));
        ws.on_timeout(WindowPolicy::Adaptive);
        // window 10 -> 9, window_max untouched (contrast timeout_pythonlike).
        assert_eq!((ws.window(), ws.window_max()), (9, 10));
        // Shrink to the floor: window stops at window_min, ceiling intact.
        for _ in 0..20 {
            ws.on_timeout(WindowPolicy::Adaptive);
        }
        assert_eq!(ws.window(), ws.window_min);
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_SLOW);
        // After the loss passes the window re-grows to the full ceiling.
        while ws.window() < ws.window_max() {
            ws.on_round_complete(WindowPolicy::Adaptive, round_sample(MID_RATE));
        }
        assert_eq!(ws.window(), RESOURCE_WINDOW_MAX_SLOW);
    }

    #[test]
    fn test_pythonlike_timeout_double_steps_window_max_on_wide_gap() {
        let mut ws = WindowState::new();
        // Open the fast tier: window_max 75, window still small.
        for _ in 0..FAST_RATE_ROUNDS {
            ws.on_round_complete(WindowPolicy::PythonLike, round_sample(RATE_FAST + 1));
        }
        assert_eq!(ws.window_max(), RESOURCE_WINDOW_MAX_FAST);
        let (w, m) = (ws.window(), ws.window_max());
        ws.on_timeout(WindowPolicy::PythonLike);
        // Gap far above flexibility: window_max steps down twice.
        assert_eq!(ws.window(), w - 1);
        assert_eq!(ws.window_max(), m - 2);
    }
}
