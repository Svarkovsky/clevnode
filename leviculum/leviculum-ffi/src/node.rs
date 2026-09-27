//! Node builder and instance lifecycle.
//!
//! `lev_builder_t` configures a node; `lev_builder_build` projects it into a
//! `leviculum_t` that owns the hidden tokio runtime and the engine node. The
//! async facade methods (`start`, `stop`) are driven with `block_on`, so each
//! call blocks the calling C thread. See `docs/leviculum-api-design.md` §1, §5.

use std::ffi::CStr;
use std::net::SocketAddr;
use std::os::raw::{c_char, c_int};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use leviculum_std::api::{Node, NodeBuilder};

use crate::error::*;
use crate::events::{lev_event_t, EventBridge};
use crate::guard;
use crate::identity::lev_identity_t;

/// Default lossless control-plane queue capacity for the event bridge.
const DEFAULT_CONTROL_CAP: usize = 64;
/// Default droppable data-plane queue capacity for the event bridge.
const DEFAULT_DATA_CAP: usize = 32;

/// Opaque node configuration handle.
///
/// `inner` is taken (set to `None`) by `lev_builder_build`; the caller still
/// owns and frees the now-empty handle.
pub struct lev_builder_t {
    inner: Option<NodeBuilder>,
    control_cap: usize,
    data_cap: usize,
}

/// Opaque node handle: owns the hidden runtime, the engine node, and the event
/// bridge that drains engine events onto a pollable fd.
pub struct leviculum_t {
    node: Node,
    events: Arc<EventBridge>,
    event_rx: std::sync::Mutex<Option<leviculum_std::EventReceiver>>,
}

impl leviculum_t {
    /// Borrow the facade node (for sibling modules like destinations).
    pub(crate) fn node(&self) -> &Node {
        &self.node
    }

    /// Borrow the hidden runtime to drive async engine calls.
    pub(crate) fn runtime(&self) -> &tokio::runtime::Runtime {
        self.node.runtime().expect("node runtime not running")
    }
}

/// Drive `fut` to completion under a deadline, blocking the calling thread.
///
/// Returns `Ok(output)` on completion, or `Err(())` if `timeout_ms >= 0` and
/// the deadline elapsed first. A negative `timeout_ms` waits forever.
pub(crate) fn block_on_timeout<F: std::future::Future>(
    rt: &tokio::runtime::Runtime,
    fut: F,
    timeout_ms: c_int,
) -> Result<F::Output, ()> {
    if timeout_ms < 0 {
        Ok(rt.block_on(fut))
    } else {
        let dur = Duration::from_millis(timeout_ms as u64);
        rt.block_on(async { tokio::time::timeout(dur, fut).await })
            .map_err(|_| ())
    }
}

/// Borrow a C string as `&str`, or `None` if NULL or not valid UTF-8.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    CStr::from_ptr(p).to_str().ok()
}

/// Apply `f` to the builder's inner value in place.
///
/// Returns `LEV_ERR_NULL_PTR` on a NULL handle and `LEV_ERR_INVALID_ARG` if the
/// builder was already consumed by `lev_builder_build`.
unsafe fn with_builder(b: *mut lev_builder_t, f: impl FnOnce(NodeBuilder) -> NodeBuilder) -> c_int {
    let b = match b.as_mut() {
        Some(b) => b,
        None => return LEV_ERR_NULL_PTR,
    };
    match b.inner.take() {
        Some(nb) => {
            b.inner = Some(f(nb));
            LEV_OK
        }
        None => {
            set_last_error("builder already consumed");
            LEV_ERR_INVALID_ARG
        }
    }
}

/// Create a new node builder. Returns NULL on failure.
#[no_mangle]
pub extern "C" fn lev_builder_new() -> *mut lev_builder_t {
    guard(std::ptr::null_mut(), || {
        // Install the logging subscriber before any node can emit tracing.
        crate::ensure_init();
        Box::into_raw(Box::new(lev_builder_t {
            inner: Some(NodeBuilder::new()),
            control_cap: DEFAULT_CONTROL_CAP,
            data_cap: DEFAULT_DATA_CAP,
        }))
    })
}

/// Free a builder handle. `lev_builder_free(NULL)` is a no-op.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_free(b: *mut lev_builder_t) {
    guard((), || {
        if !b.is_null() {
            drop(Box::from_raw(b));
        }
    })
}

/// Use an explicit identity instead of generating one. The identity is cloned.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_identity(
    b: *mut lev_builder_t,
    id: *const lev_identity_t,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let id = match id.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        let identity = id.inner.clone();
        with_builder(b, move |nb| nb.identity(identity))
    })
}

/// Set the storage directory (UTF-8 path).
#[no_mangle]
pub unsafe extern "C" fn lev_builder_storage_path(
    b: *mut lev_builder_t,
    path: *const c_char,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let path = match cstr(path) {
            Some(p) => PathBuf::from(p),
            None => return LEV_ERR_INVALID_ARG,
        };
        with_builder(b, move |nb| nb.storage_path(path))
    })
}

/// Parse `addr` as `host:port` and apply `f` with the resulting socket address.
unsafe fn with_addr(
    b: *mut lev_builder_t,
    addr: *const c_char,
    f: impl FnOnce(NodeBuilder, SocketAddr) -> NodeBuilder,
) -> c_int {
    let parsed: SocketAddr = match cstr(addr).and_then(|s| s.parse().ok()) {
        Some(a) => a,
        None => {
            set_last_error("address must be host:port");
            return LEV_ERR_INVALID_ARG;
        }
    };
    with_builder(b, move |nb| f(nb, parsed))
}

/// Add a TCP client interface to a remote node (`addr` is `host:port`).
#[no_mangle]
pub unsafe extern "C" fn lev_builder_add_tcp_client(
    b: *mut lev_builder_t,
    addr: *const c_char,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        with_addr(b, addr, |nb, a| nb.add_tcp_client(a))
    })
}

/// Add a TCP server interface listening on `addr` (`host:port`).
#[no_mangle]
pub unsafe extern "C" fn lev_builder_add_tcp_server(
    b: *mut lev_builder_t,
    addr: *const c_char,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        with_addr(b, addr, |nb, a| nb.add_tcp_server(a))
    })
}

/// Add a UDP interface bound to `listen_addr`, forwarding to `forward_addr`.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_add_udp(
    b: *mut lev_builder_t,
    listen_addr: *const c_char,
    forward_addr: *const c_char,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let listen: SocketAddr = match cstr(listen_addr).and_then(|s| s.parse().ok()) {
            Some(a) => a,
            None => {
                set_last_error("listen address must be host:port");
                return LEV_ERR_INVALID_ARG;
            }
        };
        let forward: SocketAddr = match cstr(forward_addr).and_then(|s| s.parse().ok()) {
            Some(a) => a,
            None => {
                set_last_error("forward address must be host:port");
                return LEV_ERR_INVALID_ARG;
            }
        };
        with_builder(b, move |nb| nb.add_udp(listen, forward))
    })
}

/// Add an AutoInterface (IPv6 multicast LAN discovery) with defaults.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_add_auto_interface(b: *mut lev_builder_t) -> c_int {
    guard(LEV_ERR_PANIC, || {
        with_builder(b, |nb| nb.add_auto_interface())
    })
}

/// Add an RNode (LoRa) interface. `port` is the serial device path; the rest
/// are the required radio settings (frequency in Hz, bandwidth in Hz,
/// spreading factor, coding rate denominator, tx power in dBm). Returns
/// `LEV_ERR_INVALID_ARG` on a NULL port.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_add_rnode(
    b: *mut lev_builder_t,
    port: *const c_char,
    frequency: u64,
    bandwidth: u32,
    spreading_factor: u8,
    coding_rate: u8,
    tx_power: i8,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let port = match cstr(port) {
            Some(p) => p.to_string(),
            None => return LEV_ERR_INVALID_ARG,
        };
        with_builder(b, move |nb| {
            nb.add_rnode(
                &port,
                frequency,
                bandwidth,
                spreading_factor,
                coding_rate,
                tx_power,
            )
        })
    })
}

/// Add a serial interface (KISS framing over a raw serial port). `port` is the
/// device path, `parity` is one of "N", "E", "O". Returns
/// `LEV_ERR_INVALID_ARG` on a NULL port or parity.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_add_serial(
    b: *mut lev_builder_t,
    port: *const c_char,
    speed: u32,
    databits: u8,
    parity: *const c_char,
    stopbits: u8,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let port = match cstr(port) {
            Some(p) => p.to_string(),
            None => return LEV_ERR_INVALID_ARG,
        };
        let parity = match cstr(parity) {
            Some(p) => p.to_string(),
            None => return LEV_ERR_INVALID_ARG,
        };
        with_builder(b, move |nb| {
            nb.add_serial(&port, speed, databits, &parity, stopbits)
        })
    })
}

/// Load interface and node configuration from an INI config file (the format
/// `rnsd`/`lnsd` use), bringing every interface type, including RNode and
/// serial, into the node.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_config_file(
    b: *mut lev_builder_t,
    path: *const c_char,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let path = match cstr(path) {
            Some(p) => PathBuf::from(p),
            None => return LEV_ERR_INVALID_ARG,
        };
        with_builder(b, move |nb| nb.config_file(path))
    })
}

/// Run as a shared instance under `name`: expose the local IPC socket and the
/// RPC server so `rnstatus`/`rnpath`/`rnprobe` and other tools can use this
/// node's transport.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_share_instance(
    b: *mut lev_builder_t,
    name: *const c_char,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let name = match cstr(name) {
            Some(s) => s.to_string(),
            None => return LEV_ERR_INVALID_ARG,
        };
        with_builder(b, move |nb| nb.share_instance(&name))
    })
}

/// Connect to a running shared instance `name` instead of bringing up own
/// interfaces, routing everything through that daemon.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_connect_shared_instance(
    b: *mut lev_builder_t,
    name: *const c_char,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let name = match cstr(name) {
            Some(s) => s.to_string(),
            None => return LEV_ERR_INVALID_ARG,
        };
        with_builder(b, move |nb| nb.connect_to_shared_instance(&name))
    })
}

/// Enable (`1`) or disable (`0`) transport (relay and routing) mode.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_enable_transport(
    b: *mut lev_builder_t,
    enabled: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        with_builder(b, move |nb| nb.enable_transport(enabled != 0))
    })
}

/// Override the link keepalive interval, in seconds, for every link this node
/// creates. The stale-link timeout scales with it (stale at keepalive*2). The
/// value is clamped to the protocol minimum. Useful for slow links; with a
/// short value, `LEV_EVENT_LINK_STALE`/`LINK_RECOVERED` become observable in
/// seconds. A value of 0 is treated as the protocol minimum.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_link_keepalive(b: *mut lev_builder_t, secs: u64) -> c_int {
    guard(LEV_ERR_PANIC, || {
        with_builder(b, move |nb| nb.link_keepalive(secs))
    })
}

/// Set the event-queue capacities for the node's pollable event fd: the
/// lossless control plane and the droppable data plane. A value of 0 keeps the
/// current default for that plane.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_event_capacity(
    b: *mut lev_builder_t,
    control_cap: usize,
    data_cap: usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let b = match b.as_mut() {
            Some(b) => b,
            None => return LEV_ERR_NULL_PTR,
        };
        if control_cap > 0 {
            b.control_cap = control_cap;
        }
        if data_cap > 0 {
            b.data_cap = data_cap;
        }
        LEV_OK
    })
}

/// Build a node from the builder. The builder is emptied but not freed; the
/// caller still calls `lev_builder_free`. Returns NULL on failure.
#[no_mangle]
pub unsafe extern "C" fn lev_builder_build(b: *mut lev_builder_t) -> *mut leviculum_t {
    guard(std::ptr::null_mut(), || {
        let b = match b.as_mut() {
            Some(b) => b,
            None => return std::ptr::null_mut(),
        };
        let (control_cap, data_cap) = (b.control_cap, b.data_cap);
        let nb = match b.inner.take() {
            Some(nb) => nb,
            None => {
                set_last_error("builder already consumed");
                return std::ptr::null_mut();
            }
        };
        let mut node = match nb.build() {
            Ok(n) => n,
            Err(e) => {
                map_error(&e);
                return std::ptr::null_mut();
            }
        };
        let events = match EventBridge::new(control_cap, data_cap) {
            Ok(b) => Arc::new(b),
            Err(e) => {
                set_last_error(format!("failed to create event fd: {e}"));
                return std::ptr::null_mut();
            }
        };
        let event_rx = std::sync::Mutex::new(node.take_event_receiver());
        Box::into_raw(Box::new(leviculum_t { node, events, event_rx }))
    })
}

/// Start the node: spawn the event loop and bring up interfaces.
#[no_mangle]
pub unsafe extern "C" fn lev_start(node: *mut leviculum_t) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_mut() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        match h.node.start_sync() {
            Ok(()) => {
                if let Some(rx) = h.event_rx.lock().unwrap().take() {
                    h.node.runtime().unwrap().spawn(crate::events::run_bridge(rx, Arc::clone(&h.events)));
                }
                LEV_OK
            }
            Err(e) => map_error(&e),
        }
    })
}

/// Stop the node, persist state, and tear down the event loop.
#[no_mangle]
pub unsafe extern "C" fn lev_stop(node: *mut leviculum_t) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_mut() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        match h.node.stop_sync() {
            Ok(()) => LEV_OK,
            Err(e) => map_error(&e),
        }
    })
}

/// Return 1 if the node event loop is running, 0 otherwise (also 0 on NULL).
#[no_mangle]
pub unsafe extern "C" fn lev_is_running(node: *const leviculum_t) -> c_int {
    guard(0, || match node.as_ref() {
        Some(h) if h.node.is_running() => 1,
        _ => 0,
    })
}

/// Write the node's own identity hash (16 bytes) into `buf`, read(2) style.
#[no_mangle]
pub unsafe extern "C" fn lev_identity_hash_self(
    node: *const leviculum_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        crate::write_out(&h.node.identity_hash(), buf, cap, out_len)
    })
}

/// Return the readable event fd to add to the app's `poll`/`epoll`/`select`
/// loop. The fd is owned by the library and closed by `lev_free`; the app must
/// never close it. Returns a negative error code on a NULL node.
#[no_mangle]
pub unsafe extern "C" fn lev_event_fd(node: *const leviculum_t) -> c_int {
    guard(LEV_ERR_PANIC, || match node.as_ref() {
        Some(h) => h.events.fd(),
        None => LEV_ERR_NULL_PTR,
    })
}

/// Dequeue the next event without blocking. On success `*out` is the event
/// handle (free it with `lev_event_free`) or NULL when the queue is empty.
#[no_mangle]
pub unsafe extern "C" fn lev_next_event(
    node: *mut leviculum_t,
    out: *mut *mut lev_event_t,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        *out = match h.events.next() {
            Some(ev) => Box::into_raw(ev),
            None => std::ptr::null_mut(),
        };
        LEV_OK
    })
}

/// Block up to `timeout_ms` for the next event (negative means forever). On
/// success `*out` is the event handle, or NULL if the timeout elapsed first.
///
/// Single-consumer: do not call concurrently with `lev_next_event` on the same
/// node.
#[no_mangle]
pub unsafe extern "C" fn lev_wait_event(
    node: *mut leviculum_t,
    out: *mut *mut lev_event_t,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let fd = h.events.fd();
        let deadline = if timeout_ms < 0 {
            None
        } else {
            Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
        };
        loop {
            if let Some(ev) = h.events.next() {
                *out = Box::into_raw(ev);
                return LEV_OK;
            }
            // Poll in bounded slices so an infinite wait still rechecks state.
            let slice_ms: c_int = match deadline {
                None => 250,
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        *out = std::ptr::null_mut();
                        return LEV_OK;
                    }
                    (d - now).as_millis().min(250) as c_int
                }
            };
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: single valid pollfd for the lifetime of the call.
            unsafe {
                libc::poll(&mut pfd as *mut libc::pollfd, 1, slice_ms);
            }
            // Loop back: re-check the queue (the poll may be spurious or the
            // slice may have expired).
        }
    })
}

/// Free a node handle, stopping it gracefully first if still running.
/// `lev_free(NULL)` is a no-op.
#[no_mangle]
pub unsafe extern "C" fn lev_free(node: *mut leviculum_t) {
    guard((), || {
        if node.is_null() {
            return;
        }
        let mut boxed = Box::from_raw(node);
        if boxed.node.is_running() {
            // Best effort graceful shutdown so state is persisted. If an earlier
            // caught panic poisoned the core mutex, stop() re-locks it (via
            // save_persistent_state) and panics again; contain that here so
            // teardown stays deterministic. Drop then reclaims the runtime and
            // event loop via shutdown_background, at the cost of the final
            // flush, which is recovered later from fresh announces.
            let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                boxed.node.stop_sync()
            }));
            if stopped.is_err() {
                set_last_error_static(
                    c"lev_free: graceful stop panicked on a poisoned node, reclaimed via teardown",
                );
            }
        }
        drop(boxed);
    })
}
