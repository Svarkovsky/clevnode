//! Test-capture helpers for the structured event-log sink.
//!
//! The PRODUCTION sink (the [`crate::event_log::EventLogLayer`],
//! [`crate::event_log::EVENT_CATALOG`], the
//! visitor, the canonical-line format, the append-only file output,
//! `install_global_subscriber`) lives in [`crate::event_log`].  This
//! module adds the per-test buffer-capture layer on top: helpers that
//! ensure the global subscriber is installed (via
//! [`crate::test_support::tracing_setup`]) and then register an
//! [`EventLogHandle`] in the layer's active list so a single test sees
//! the events it emits.
//!
//! # Wiring a test
//!
//! ```ignore
//! use leviculum_std::test_support::event_log::init_event_log;
//!
//! #[tokio::test]
//! async fn my_mvr() {
//!     let _evlog = init_event_log();
//!     // ... test body emits tracing::debug!(event = "...", ...) calls ...
//! }
//! ```
//!
//! On `Drop`, if the thread is panicking, the buffer dumps to stderr
//! (or to the configured file via [`init_event_log_to_file`]) with a
//! `=== EVENT LOG DUMP …` banner.  Per-handle buffer isolation, the
//! cross-test pollution caveat, and the schema/field-violation lines
//! are all documented in [`crate::event_log`].

use std::path::PathBuf;

use crate::event_log::{new_handle, DumpTarget};

// Re-export the sink's public schema/handle types at this path so
// existing test imports (`test_support::event_log::{EventLogHandle,
// EventSchema}`) keep resolving after the production move.  These are
// also the return / parameter types of the helpers below.
pub use crate::event_log::{EventLogHandle, EventSchema};

/// Initialise the subscriber with the production catalogue.
/// Panic-dump goes to stderr.
#[must_use]
pub fn init_event_log() -> EventLogHandle {
    crate::test_support::tracing_setup::init_tracing_with_event_log();
    new_handle(DumpTarget::Stderr, &[])
}

/// Initialise the subscriber with the production catalogue.
/// Panic-dump is written to `path` instead of stderr.  Useful when
/// a test wants to assert on the dumped content.
#[must_use]
pub fn init_event_log_to_file(path: PathBuf) -> EventLogHandle {
    crate::test_support::tracing_setup::init_tracing_with_event_log();
    new_handle(DumpTarget::File(path), &[])
}

/// Test-only entry point: extends the production catalogue with the
/// supplied extra schemas (per-handle, not global).  Used by
/// `tests/event_log_subscriber.rs` to exercise the schema-violation
/// path with synthetic event names that don't pollute the production
/// catalogue.
#[doc(hidden)]
#[must_use]
pub fn init_event_log_with_extra_schemas(
    extra: &'static [EventSchema],
    file_path: Option<PathBuf>,
) -> EventLogHandle {
    crate::test_support::tracing_setup::init_tracing_with_event_log();
    let target = match file_path {
        Some(p) => DumpTarget::File(p),
        None => DumpTarget::Stderr,
    };
    new_handle(target, extra)
}

/// Asserts that the buffer contains no `EVENT_SCHEMA_VIOLATION` lines
/// referencing an event the test cares about.  Filter caller-side
/// before passing the dump (or accept a generic check that any
/// violation panics).  Use at the end of a test where catalogue
/// completeness is part of the contract.
#[macro_export]
macro_rules! assert_no_schema_violations {
    ($handle:expr) => {{
        let __dump = $handle.dump();
        let __violations: Vec<&String> = __dump
            .iter()
            .filter(|l| l.starts_with("EVENT_SCHEMA_VIOLATION"))
            .collect();
        if !__violations.is_empty() {
            panic!(
                "schema violations: {} (first: {:?})",
                __violations.len(),
                __violations.first(),
            );
        }
    }};
}
