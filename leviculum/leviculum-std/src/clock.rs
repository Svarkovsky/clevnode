//! System clock implementation for std platforms
//!
//! Provides a monotonic clock using `std::time::Instant`.

use leviculum_core::traits::Clock;
use std::time::Instant;

/// System clock using `std::time::Instant`
///
/// Monotonic, suitable for timeouts and RTT measurement. Wall-clock unix
/// time for wire fields (announce emission timestamps, Codeberg #155) is
/// exposed separately via `wall_unix_secs`, backed by `SystemTime`.
pub struct SystemClock {
    start: Instant,
}

impl SystemClock {
    /// Create a new system clock (epoch = now)
    ///
    /// `pub` for Codeberg #202: this is one of the three arguments a
    /// downstream crate has to pass to build the `StdNodeCore` the processor
    /// seam hands its hooks. Nameable without constructible is not a seam.
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// Expose the start anchor so the interface layer's backpressure
    /// clock can align with ours. The airtime bucket's `last_update_ms`
    /// and `Transport::now_ms` must share a frame, otherwise the
    /// retry-scheduler's deferral math breaks.
    pub(crate) fn start_instant(&self) -> Instant {
        self.start
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn wall_unix_secs(&self) -> Option<u64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
    }

    /// The same reading without the truncation to whole seconds (Codeberg
    /// #217). LXMF hashes the message timestamp into the message ID, so the
    /// precision discarded here is the only thing that distinguishes two
    /// identical messages sent inside one second.
    fn wall_unix_micros(&self) -> Option<u64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_micros()).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wall_unix_secs_is_plausible_unix_time() {
        // Codeberg #155: the announce emission timestamp is built from this
        // value; it must be wall-clock unix seconds, not process uptime.
        let secs = SystemClock::new()
            .wall_unix_secs()
            .expect("std has a wall clock");
        assert!((1_750_000_000..2_200_000_000).contains(&secs));
    }

    /// Codeberg #217: the microsecond reading must not be the second reading
    /// scaled up. `wall_unix_secs * 1_000_000` is what the trait default
    /// already gives; an override that returns it is indistinguishable from no
    /// override, and LXMF message IDs would keep colliding inside one second.
    #[test]
    fn wall_unix_micros_is_not_truncated_to_whole_seconds() {
        let clock = SystemClock::new();

        // Agreement first: the two readings describe the same instant.
        let micros = clock.wall_unix_micros().expect("std has a wall clock");
        let secs = clock.wall_unix_secs().expect("std has a wall clock");
        assert!(
            micros / 1_000_000 >= secs && micros / 1_000_000 <= secs + 1,
            "wall_unix_micros {micros} and wall_unix_secs {secs} must be the same reading"
        );

        // Then precision, at the resolution the defect actually needs: two
        // consecutive `create_message` calls are ~115 µs apart, so a reading
        // that cannot separate two calls in the same millisecond does not fix
        // #217. No sleep — back-to-back reads must already differ.
        let before = clock.wall_unix_micros().expect("std has a wall clock");
        let after = clock.wall_unix_micros().expect("std has a wall clock");
        assert!(
            after >= before,
            "the wall clock must not run backwards: {before} to {after}"
        );
        assert!(
            after - before < 1_000_000,
            "two back-to-back reads must not be a whole second apart: \
             {before} to {after}"
        );

        // A 5 ms sleep must move the reading by roughly 5000 µs, not by a
        // whole second and not by nothing.
        let before = clock.wall_unix_micros().expect("std has a wall clock");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let after = clock.wall_unix_micros().expect("std has a wall clock");
        assert!(
            (4_000..1_000_000).contains(&(after - before)),
            "a 5 ms sleep must advance the microsecond reading by about 5000 µs: \
             {before} to {after}"
        );
    }

    #[test]
    fn test_system_clock() {
        let clock = SystemClock::new();
        let t1 = clock.now_ms();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let t2 = clock.now_ms();
        assert!(t2 > t1);
    }

    #[test]
    fn test_clock_trait_methods() {
        let clock = SystemClock::new();
        let deadline = clock.deadline(1000);
        assert!(!clock.has_elapsed(deadline));
        assert!(clock.now_secs() < 1); // Just started
    }
}
