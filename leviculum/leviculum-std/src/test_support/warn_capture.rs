//! Per-test capture of plain (non-`event=`) WARN-level tracing records.
//!
//! The structured event-log layer ([`crate::event_log`]) captures only
//! records that carry an `event = "..."` field.  A few interop tests
//! need to observe an ordinary `tracing::warn!` MESSAGE as a reliable
//! assertion — e.g. the relay's `LRPROOF hop asymmetry: rewriting
//! forwarded hops to the frozen count` line (Codeberg #38), which is a plain warn emitted from
//! `leviculum_core::transport`, not a catalogued event.
//!
//! Capturing that with a private `set_global_default` subscriber inside
//! one test is a RACE: the harness's global subscriber (installed by any
//! test that calls `common::init_tracing()`) wins the one-shot install
//! first under the parallel full suite, so the private subscriber never
//! takes effect and its buffer stays empty.  That is exactly the
//! failure the two `lrproof_hop_undercount` tests hit under the parallel
//! `rnsd_interop` run while passing in isolation.
//!
//! This layer is instead part of the ONE global subscriber
//! ([`crate::test_support::tracing_setup`]), so capture works regardless
//! of which test installs the subscriber first.  Isolation mirrors
//! [`crate::test_support::event_log`]: a shared active-handles list; each
//! [`WarnCaptureHandle`] owns a buffer that receives every captured
//! record while registered and unregisters on drop.  Cross-test
//! pollution is possible (every active buffer sees every captured
//! record) but harmless for a `contains`-style assertion on a message
//! unique to the asserting test.

use std::sync::{Arc, Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

type Buffer = Arc<Mutex<String>>;

/// Shared active-handles list.  Lazily allocated so the order of
/// subscriber install and handle registration does not matter.
fn active_list() -> &'static Arc<Mutex<Vec<Buffer>>> {
    static ACTIVE: OnceLock<Arc<Mutex<Vec<Buffer>>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
}

/// A registered capture buffer.  Snapshot the captured text with
/// [`WarnCaptureHandle::snapshot`]; the buffer unregisters on drop.
pub struct WarnCaptureHandle {
    buffer: Buffer,
}

impl WarnCaptureHandle {
    /// Current captured text: one `target message` line per record,
    /// newline-separated, in emission order.
    #[must_use]
    pub fn snapshot(&self) -> String {
        self.buffer.lock().unwrap().clone()
    }
}

impl Drop for WarnCaptureHandle {
    fn drop(&mut self) {
        if let Ok(mut list) = active_list().lock() {
            list.retain(|b| !Arc::ptr_eq(b, &self.buffer));
        }
    }
}

/// Ensure the global subscriber is installed, then register a fresh
/// capture buffer.  Every WARN-level record from `leviculum_core` is
/// appended to it until the returned handle drops.
#[must_use]
pub fn register_warn_capture() -> WarnCaptureHandle {
    crate::test_support::tracing_setup::init_tracing_with_event_log();
    let buffer: Buffer = Arc::new(Mutex::new(String::new()));
    active_list().lock().unwrap().push(Arc::clone(&buffer));
    WarnCaptureHandle { buffer }
}

/// Build the layer used by the global subscriber installer.  One global
/// layer per process; the active-handles list it reads is shared with
/// every [`WarnCaptureHandle`].  The caller applies the WARN/target
/// filter that bounds volume.
pub(crate) fn layer() -> WarnCaptureLayer {
    WarnCaptureLayer
}

/// The capture layer.  With no active handles it does nothing; while a
/// handle is registered it appends each record's message to every active
/// buffer.
pub(crate) struct WarnCaptureLayer;

impl<S: Subscriber> Layer<S> for WarnCaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let list = active_list().lock().unwrap();
        if list.is_empty() {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let Some(message) = visitor.message else {
            return;
        };
        let line = format!("{} {}\n", event.metadata().target(), message);
        for buffer in list.iter() {
            buffer.lock().unwrap().push_str(&line);
        }
    }
}

/// Extracts the record's `message` field (the `tracing::warn!` format
/// string, rendered).  `format_args!` Debug renders without quotes, so
/// this is the message text verbatim.
#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}
