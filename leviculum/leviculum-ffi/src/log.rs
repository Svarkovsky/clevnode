//! Logging control and the process-global `tracing` bridge.
//!
//! The Reticulum stack logs through `tracing`. A C app cannot install a Rust
//! subscriber, so the library installs one (once, via [`crate::ensure_init`])
//! that forwards records to a level filter and an optional C callback, in the
//! spirit of libcurl's `CURLOPT_DEBUGFUNCTION`. Default level is
//! [`LEV_LOG_OFF`], so the library is silent unless asked. See
//! `docs/leviculum-api-design.md` §12.

use std::cell::Cell;
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

use crate::error::set_last_error;
use crate::guard;

/// No logging (default).
pub const LEV_LOG_OFF: c_int = 0;
/// Errors only.
pub const LEV_LOG_ERROR: c_int = 1;
/// Warnings and above.
pub const LEV_LOG_WARN: c_int = 2;
/// Info and above.
pub const LEV_LOG_INFO: c_int = 3;
/// Debug and above.
pub const LEV_LOG_DEBUG: c_int = 4;
/// Everything.
pub const LEV_LOG_TRACE: c_int = 5;

/// Log sink callback: `(level, message, user)`. The message is a
/// NUL-terminated string owned by the library, valid only for the duration of
/// the call. The callback may run on any internal worker thread and must not
/// call back into any `lev_*` function (see design §5, §12). It must not unwind
/// or throw across the boundary; a panic/exception that escapes the callback is
/// caught and swallowed by the library, but the record is then lost. NULL
/// restores the stderr default.
///
/// Nullable on purpose: `Option` is inlined into the typedef so cbindgen
/// renders it as a plain (nullable) C function pointer, not an opaque struct.
/// The `C-unwind` ABI lets the library contain a callback that unwinds (a C++
/// exception, or a Rust callback that panics) with `catch_unwind` instead of
/// the process aborting at a plain `extern "C"` boundary. A normal
/// non-unwinding C callback remains fully compatible.
pub type lev_log_callback =
    Option<extern "C-unwind" fn(level: c_int, message: *const c_char, user: *mut c_void)>;

/// Non-null callback pointer, the inner of [`lev_log_callback`], for storage.
type LogCb = extern "C-unwind" fn(level: c_int, message: *const c_char, user: *mut c_void);

/// Current verbosity. Events at a level numerically `<=` this are emitted.
static LEVEL: AtomicU8 = AtomicU8::new(LEV_LOG_OFF as u8);

/// Installed callback and its opaque user pointer. The pointer is stored as a
/// `usize` so the slot is `Send`; the app owns its lifetime.
#[derive(Clone, Copy)]
struct Sink {
    cb: LogCb,
    user: usize,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

fn current_level() -> c_int {
    LEVEL.load(Ordering::Relaxed) as c_int
}

fn level_to_lev(level: Level) -> c_int {
    match level {
        Level::ERROR => LEV_LOG_ERROR,
        Level::WARN => LEV_LOG_WARN,
        Level::INFO => LEV_LOG_INFO,
        Level::DEBUG => LEV_LOG_DEBUG,
        Level::TRACE => LEV_LOG_TRACE,
    }
}

thread_local! {
    /// Set while a callback is running on this thread, so a callback that
    /// panics (which fires the panic hook, which logs through `dispatch` again)
    /// or that itself logs cannot recurse back into the user callback.
    static IN_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

/// Forward one formatted record to the C callback, or to stderr when none is
/// set. The sink is copied out under the lock and released before the callback
/// runs, so a callback that is slow or re-enters logging cannot deadlock.
fn dispatch(level: c_int, target: &str, msg: &str) {
    let line = format!("{target}: {msg}");
    let sink = SINK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .copied();
    match sink {
        Some(Sink { cb, user }) => {
            // Re-entrancy guard. The callback is unguarded C, invoked here on an
            // internal worker/runtime thread (and from the panic hook), outside
            // any `extern "C"` firewall. A callback that panics triggers the
            // installed panic hook, which logs through `dispatch` again; without
            // this guard that path would call the callback while it is unwinding
            // and recurse without bound. Route the nested record to stderr.
            if IN_CALLBACK.with(|f| f.get()) {
                eprintln!("[leviculum] {line}");
                return;
            }
            // Truncate at the first NUL so construction cannot fail.
            let bytes = line.into_bytes();
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            // Safe: no interior NUL up to `end`.
            let c = unsafe { std::ffi::CString::from_vec_unchecked(bytes[..end].to_vec()) };
            let ptr = c.as_ptr();
            IN_CALLBACK.with(|f| f.set(true));
            // Unwinding into C is undefined behaviour; contain a panicking
            // callback and swallow it (the record is lost, the bridge survives).
            // `catch_unwind` always returns, so the guard flag is cleared.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                cb(level, ptr, user as *mut c_void);
            }));
            IN_CALLBACK.with(|f| f.set(false));
        }
        None => eprintln!("[leviculum] {line}"),
    }
}

/// Collects an event's `message` field, appending any other fields as
/// `key=value`, into a single line for the C sink.
struct MsgVisitor(String);

impl tracing::field::Visit for MsgVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

/// `tracing` layer that gates by [`LEVEL`] and forwards to [`dispatch`].
struct CLayer;

impl<S: Subscriber> Layer<S> for CLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let lev = level_to_lev(*meta.level());
        if lev > current_level() {
            return;
        }
        let mut visitor = MsgVisitor(String::new());
        event.record(&mut visitor);
        dispatch(lev, meta.target(), &visitor.0);
    }
}

/// Install the subscriber and a panic hook. Called exactly once through the
/// `Once` in [`crate::ensure_init`].
pub(crate) fn install() {
    // try_init fails if the host already set a global subscriber; in that case
    // we defer to it rather than override (good citizenship).
    let _ = tracing_subscriber::registry().with(CLayer).try_init();

    // Chain a hook that surfaces panics through our sink, then calls the
    // previous hook so host behaviour (and backtraces) is preserved.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        dispatch(LEV_LOG_ERROR, "panic", &format!("{info}"));
        prev(info);
    }));
}

/// Set the global log level. One of the `LEV_LOG_*` constants.
#[no_mangle]
pub extern "C" fn lev_log_set_level(level: c_int) -> c_int {
    guard(crate::LEV_ERR_PANIC, || {
        crate::ensure_init();
        if !(LEV_LOG_OFF..=LEV_LOG_TRACE).contains(&level) {
            set_last_error("log level out of range");
            return crate::LEV_ERR_INVALID_ARG;
        }
        LEVEL.store(level as u8, Ordering::Relaxed);
        crate::LEV_OK
    })
}

/// Set the log sink callback, or pass `NULL` to restore the stderr default.
/// `user` is passed back to the callback unchanged.
#[no_mangle]
pub extern "C" fn lev_log_set_callback(cb: lev_log_callback, user: *mut c_void) -> c_int {
    guard(crate::LEV_ERR_PANIC, || {
        crate::ensure_init();
        let mut slot = SINK.lock().unwrap_or_else(|e| e.into_inner());
        *slot = cb.map(|cb| Sink {
            cb,
            user: user as usize,
        });
        crate::LEV_OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// Calls per callback, to prove the bridge stays usable after a panic.
    static CALLS: AtomicU32 = AtomicU32::new(0);
    /// Serialize the tests: they share the process-global `SINK` slot.
    static SERIAL: Mutex<()> = Mutex::new(());

    extern "C-unwind" fn panicking_cb(_level: c_int, _msg: *const c_char, _user: *mut c_void) {
        CALLS.fetch_add(1, Ordering::SeqCst);
        panic!("callback blew up");
    }

    extern "C-unwind" fn counting_cb(_level: c_int, _msg: *const c_char, _user: *mut c_void) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    /// A callback that logs again, exercising the re-entrancy guard.
    extern "C-unwind" fn reentrant_cb(_level: c_int, _msg: *const c_char, _user: *mut c_void) {
        CALLS.fetch_add(1, Ordering::SeqCst);
        dispatch(LEV_LOG_ERROR, "reentrant", "from inside the callback");
    }

    fn set_sink(cb: LogCb) {
        *SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Sink { cb, user: 0 });
    }

    fn clear_sink() {
        *SINK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    // A panicking callback must not unwind across the (C) boundary: dispatch
    // catches it, the process survives, and the sink stays usable afterwards.
    #[test]
    fn panicking_callback_is_contained() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        CALLS.store(0, Ordering::SeqCst);
        set_sink(panicking_cb);
        // Would abort the test process if the unwind escaped.
        dispatch(LEV_LOG_ERROR, "test", "first");
        dispatch(LEV_LOG_ERROR, "test", "second");
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            2,
            "both records reached the cb"
        );

        // The bridge is still usable: a fresh, well-behaved callback runs.
        set_sink(counting_cb);
        dispatch(LEV_LOG_ERROR, "test", "third");
        assert_eq!(CALLS.load(Ordering::SeqCst), 3);
        clear_sink();
    }

    // A callback that logs again is routed to stderr by the guard instead of
    // recursing into itself.
    #[test]
    fn reentrant_callback_does_not_recurse() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        CALLS.store(0, Ordering::SeqCst);
        set_sink(reentrant_cb);
        dispatch(LEV_LOG_ERROR, "test", "outer");
        // Exactly one user-callback invocation; the nested record went to stderr.
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        clear_sink();
    }
}
