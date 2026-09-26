//! Embassy-backed monotonic clock for leviculum-core

use leviculum_core::traits::Clock;

/// Monotonic clock using Embassy's RTC1 timer driver.
pub struct EmbassyClock;

impl Clock for EmbassyClock {
    fn now_ms(&self) -> u64 {
        embassy_time::Instant::now().as_millis()
    }
}
