//! Process-global tracing subscriber installer for the test harness.
//!
//! Replaces the previous standalone `tracing_subscriber::fmt().init()`
//! in `tests/rnsd_interop/common.rs::init_tracing()` with a `Registry`
//! chain so subsequent layers (Stage 6 / Codeberg #39 piece 1's
//! event-log layer) can be attached without rewriting every test
//! that depends on the global subscriber.
//!
//! Idempotent via `std::sync::Once` — multiple test files can call
//! `init_tracing_with_event_log()` and the install runs at most once
//! per process.
//!
//! # Why a global subscriber, not per-thread
//!
//! `tracing::dispatcher::set_default()` returns a per-thread guard.
//! In `#[tokio::test(multi_thread)]` tests the spawned worker threads
//! have their own thread-local dispatcher and never see events
//! emitted on the test's main thread (and vice versa).
//!
//! Using `set_global_default` once per process means every thread —
//! main or worker — routes events through the same subscriber.  The
//! event-log layer (added in Stage 6 commit 2) builds per-test
//! buffer isolation on top of this global root via an active-handles
//! list.

use std::sync::Once;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use tracing_subscriber::{fmt, EnvFilter, Registry};

static INIT: Once = Once::new();

/// Install the process-global subscriber.  Composes:
///
/// - an `EnvFilter` driven by `RUST_LOG` (default `info`),
/// - the standard `fmt` layer with `with_test_writer` so libtest
///   captures the output, and
/// - the event-log layer from
///   [`crate::event_log::layer`], driven by an active-handles list
///   shared with [`crate::test_support::event_log::init_event_log`].
///
/// Once-guarded — every test file that needs the global subscriber
/// can call this safely; only the first call wins.
pub fn init_tracing_with_event_log() {
    INIT.call_once(|| {
        let env_filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        // Filter is attached to the fmt layer only — keeps fmt-output
        // tame at info-level by default while letting the event-log
        // layer see every record.  RUST_LOG=debug still works for
        // anyone who wants verbose fmt output.
        let fmt_layer = fmt::layer().with_test_writer().with_filter(env_filter);
        let event_layer = crate::event_log::layer();
        // Plain-WARN capture for tests that assert on an ordinary
        // `tracing::warn!` message (Codeberg #38's LRPROOF asymmetry
        // line, Codeberg #157's corrupt-discovery-record line).  Filtered
        // to `leviculum_core` / `leviculum_std` WARN so the per-handle
        // buffers stay small; inert until a test registers a handle.
        let warn_capture_layer = crate::test_support::warn_capture::layer()
            .with_filter(EnvFilter::new("leviculum_core=warn,leviculum_std=warn"));
        Registry::default()
            .with(fmt_layer)
            .with(event_layer)
            .with(warn_capture_layer)
            .init();
    });
}
