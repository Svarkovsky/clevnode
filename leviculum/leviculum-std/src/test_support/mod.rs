//! Test-harness scaffolding shared between integration tests and mvr tests.
//!
//! Stage 6 (Codeberg #39 piece 1) introduces:
//!
//! - [`tracing_setup`] — process-global subscriber installer that
//!   composes the standard fmt layer with an optional event-log layer.
//!   Used by `tests/rnsd_interop/common.rs::init_tracing()`.

pub mod event_log;
pub mod tracing_setup;
pub mod warn_capture;
