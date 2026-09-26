//! Helper binary for the multi-process event-log integration test.
//!
//! Usage (driven by env vars + argv):
//!
//! ```sh
//! LEVICULUM_EVENT_LOG=/tmp/foo.log \
//! LEVICULUM_EVENT_NODE=node-a \
//!     event-log-helper <pre-sleep-ms> <cadence-ms>
//! ```
//!
//! Sleeps `<pre-sleep-ms>`, then emits 3 `HELPER_TICK` events spaced
//! by `<cadence-ms>`, exits 0.  Designed to interleave deterministically
//! with a second instance using staggered (pre-sleep, cadence) values
//! so the merged log alternates `node=` keys.
//!
//! The structured event-log layer is installed via
//! `event_log::install_global_subscriber`; events go to the
//! per-process file `LEVICULUM_EVENT_LOG` points at.

use std::thread;
use std::time::Duration;

use leviculum_std::event_log::install_global_subscriber;

fn main() {
    let mut args = std::env::args().skip(1);
    let pre_sleep_ms: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .expect("usage: event-log-helper <pre-sleep-ms> <cadence-ms>");
    let cadence_ms: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .expect("usage: event-log-helper <pre-sleep-ms> <cadence-ms>");

    install_global_subscriber("debug");

    thread::sleep(Duration::from_millis(pre_sleep_ms));
    for i in 0..3 {
        tracing::debug!(event = "HELPER_TICK", i = i as u64);
        thread::sleep(Duration::from_millis(cadence_ms));
    }
}
