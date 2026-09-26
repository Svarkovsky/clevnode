//! Shared tracing-log capture for tests.
//!
//! Zero-flake requirement (root cause). `tracing` caches a per-callsite
//! `Interest` process-globally, computed from whichever dispatcher first
//! registers the callsite. Hot callsites (PROOF_SEND in `link.rs`, PKT_LOCAL /
//! RESOURCE_RW in `transport.rs` / `resource.rs`) are exercised by hundreds of
//! OTHER unit tests. A per-test scoped subscriber installed via
//! `subscriber::set_default` does NOT trigger an interest-cache rebuild, so if a
//! callsite was already registered before the scoped subscriber armed, the
//! scoped subscriber never receives it: the assertion flakes under parallel
//! scheduling (~1 in 4 full runs on `diamond_lrproof_return_path_*`).
//!
//! Fix: install EXACTLY ONE process-global subscriber (via
//! `set_global_default`) that is always DEBUG-enabled, so every callsite
//! registers enabled process-wide and no capturing region depends on
//! registration order. Events route to a THREAD-LOCAL buffer: only the thread
//! that armed a buffer collects the events emitted on it (the sans-I/O
//! scenarios run entirely on their own test thread), every other thread
//! discards into `None`, so there is no cross-test pollution and no unbounded
//! process-wide buffer growth. `rebuild_interest_cache` un-poisons any callsite
//! an earlier no-subscriber test cached before our global default existed.

extern crate std;

use std::cell::RefCell;
use std::string::String;
use std::sync::{Arc, Mutex, OnceLock};
use std::vec::Vec;

std::thread_local! {
    static CAPTURE: RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { RefCell::new(None) };
}

/// A `MakeWriter` that appends to whatever buffer the current thread has armed
/// via [`with_captured_logs`], and discards otherwise.
#[derive(Clone, Copy)]
struct ThreadLocalWriter;

impl std::io::Write for ThreadLocalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        CAPTURE.with(|c| {
            if let Some(sink) = c.borrow().as_ref() {
                sink.lock().unwrap().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLocalWriter {
    type Writer = ThreadLocalWriter;
    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

fn ensure_global_subscriber() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(ThreadLocalWriter)
            // TRACE, not DEBUG: some decisions are only ever narrated at
            // trace level (the transport-id mismatch line, Codeberg #344), and
            // a capture ceiling below the callsite's level makes such a line
            // untestable. The ceiling has to be raised here rather than per
            // test — one process-global subscriber decides enablement for
            // every callsite, so a scoped subscriber could not widen it.
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_target(true)
            .finish();
        // Ignore the error: only this module installs a global default in the
        // leviculum-core test binary, so this succeeds. If it ever loses the
        // race the capture buffer stays empty and the assertions fail loudly.
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Run `body` with DEBUG tracing captured into a string. The formatted event
/// lines (DEBUG level, no ansi, `target` included) match the historical
/// per-test capture format, so existing substring assertions keep passing.
///
/// Nesting on one thread is not supported (the inner region would replace the
/// outer buffer); callers never nest.
pub(crate) fn with_captured_logs<R>(body: impl FnOnce() -> R) -> (R, String) {
    ensure_global_subscriber();
    let buf = Arc::new(Mutex::new(Vec::new()));
    CAPTURE.with(|c| {
        let mut slot = c.borrow_mut();
        debug_assert!(
            slot.is_none(),
            "with_captured_logs does not support nesting"
        );
        *slot = Some(Arc::clone(&buf));
    });
    // Un-poison any callsite an earlier no-subscriber test cached before our
    // global DEBUG default existed. Rebuilds every already-registered callsite
    // against the global default (now ours).
    tracing::callsite::rebuild_interest_cache();
    let out = body();
    CAPTURE.with(|c| *c.borrow_mut() = None);
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    (out, logs)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::with_captured_logs;

    // Order-independence invariant. A prior NON-capturing emission at a callsite
    // must not hide that callsite from a later capturing region. With the global
    // subscriber (+rebuild_interest_cache) this holds regardless of registration
    // order; a regression to per-test `set_default` breaks it under parallel
    // scheduling (see the module docs).
    #[test]
    fn capture_independent_of_prior_noncapturing_emission() {
        // Register this unique callsite FIRST with no capture armed on this
        // thread.
        tracing::debug!(target: "tlc_repro", marker = "tlc-unique-9f3a", "TLC_REPRO_EVENT");
        let ((), logs) = with_captured_logs(|| {
            tracing::debug!(target: "tlc_repro", marker = "tlc-unique-9f3a", "TLC_REPRO_EVENT");
        });
        assert!(
            logs.contains("TLC_REPRO_EVENT") && logs.contains("tlc-unique-9f3a"),
            "capturing region missed an event registered by a prior non-capturing \
             emission; logs={logs:?}"
        );
        assert!(
            logs.contains("tlc_repro"),
            "target must be included; logs={logs:?}"
        );
    }

    // A non-capturing thread must not leak events into a capturing thread's
    // buffer, and the capturing thread must see its own.
    #[test]
    fn capture_is_thread_local() {
        let ((), logs) = with_captured_logs(|| {
            tracing::debug!(target: "tlc_local", "OWN_EVENT");
            let h = std::thread::spawn(|| {
                tracing::debug!(target: "tlc_local", "OTHER_THREAD_EVENT");
            });
            h.join().unwrap();
        });
        assert!(
            logs.contains("OWN_EVENT"),
            "own thread event missing; logs={logs:?}"
        );
        assert!(
            !logs.contains("OTHER_THREAD_EVENT"),
            "non-capturing thread leaked into buffer; logs={logs:?}"
        );
    }
}
