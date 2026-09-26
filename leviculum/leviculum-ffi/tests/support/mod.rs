//! Shared helpers for the Rust-driven C API test suites.
//!
//! Each test file includes this with `mod support;`. The helpers wrap the
//! `unsafe extern "C"` `lev_*` functions into ergonomic, asserting Rust so the
//! test bodies stay readable. A subdir `mod.rs` is not compiled as its own test
//! binary.
#![allow(dead_code)]

use std::ffi::{CStr, CString};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::raw::{c_char, c_int};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use leviculum::*;

pub mod python_daemon;

/// Host-wide unique listener ports for consumers that cannot bind `:0` and
/// read the result back (a symmetric UDP pair, the Python daemon's config
/// file). Shared source with the leviculum-std suites — see that file's
/// module docs for the allocator's contract. Codeberg #221: the band sits
/// above the kernel's ephemeral range, so no `bind(":0")` co-tenant can be
/// handed one of its numbers, and the flock'd counter keeps two test
/// processes from ever drawing the same number.
#[path = "../../../leviculum-std/tests/support/port_alloc.rs"]
#[allow(dead_code)]
pub mod port_alloc;

/// Runtime control for a [`FaultProxy`]: a silent-drop gate and a hard-cut flag.
struct ProxyCtrl {
    /// When set, bytes are read from both sides but discarded (sockets stay
    /// open) so the peers see silence, not a disconnect. Mimics `iptables DROP`.
    blocked: AtomicBool,
    /// When set, both connections are shut down and the accept loop stops.
    closed: AtomicBool,
}

/// An in-process TCP proxy that bridges a loopback `port` to an `upstream`
/// port and can inject faults: [`FaultProxy::block`] silently drops traffic in
/// both directions (peers see silence), [`FaultProxy::cut`] tears the
/// connection down (peers see EOF). Point a node's TCP client at `port` instead
/// of the real server to make link/path failures deterministic. Dropping the
/// proxy cuts it. Modelled on the MVR proxies in `leviculum-std/tests/mvr`,
/// reimplemented here because that test code lives in another crate.
pub struct FaultProxy {
    pub port: u16,
    ctrl: Arc<ProxyCtrl>,
    accept: Option<thread::JoinHandle<()>>,
}

impl FaultProxy {
    /// Spawn a proxy forwarding loopback `port` to `upstream` (also loopback).
    pub fn spawn(upstream: u16) -> FaultProxy {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy port");
        let port = listener.local_addr().expect("local_addr").port();
        listener
            .set_nonblocking(true)
            .expect("proxy listener nonblocking");
        let ctrl = Arc::new(ProxyCtrl {
            blocked: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        let accept_ctrl = ctrl.clone();
        let accept = thread::spawn(move || {
            while !accept_ctrl.closed.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let up = match TcpStream::connect(("127.0.0.1", upstream)) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        let (c2, u2) = (
                            client.try_clone().expect("clone client"),
                            up.try_clone().expect("clone upstream"),
                        );
                        let ctrl_a = accept_ctrl.clone();
                        let ctrl_b = accept_ctrl.clone();
                        thread::spawn(move || pump(client, up, ctrl_a));
                        thread::spawn(move || pump(u2, c2, ctrl_b));
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        FaultProxy {
            port,
            ctrl,
            accept: Some(accept),
        }
    }

    /// Silently drop traffic in both directions (peers see silence).
    pub fn block(&self) {
        self.ctrl.blocked.store(true, Ordering::Relaxed);
    }

    /// Resume forwarding after a [`block`](Self::block).
    pub fn unblock(&self) {
        self.ctrl.blocked.store(false, Ordering::Relaxed);
    }

    /// Tear the connection down (peers see EOF) and stop accepting.
    pub fn cut(&self) {
        self.ctrl.closed.store(true, Ordering::Relaxed);
    }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.cut();
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

/// Copy `from` -> `to` until EOF, error, or the proxy is cut. While blocked,
/// bytes are read and discarded so the sender does not stall but the receiver
/// sees nothing. A short read timeout lets the loop observe the control flags.
fn pump(mut from: TcpStream, mut to: TcpStream, ctrl: Arc<ProxyCtrl>) {
    from.set_read_timeout(Some(Duration::from_millis(50))).ok();
    let mut buf = [0u8; 16384];
    loop {
        if ctrl.closed.load(Ordering::Relaxed) {
            break;
        }
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if ctrl.blocked.load(Ordering::Relaxed) {
                    continue;
                }
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    let _ = from.shutdown(Shutdown::Both);
    let _ = to.shutdown(Shutdown::Both);
}

/// Build a NUL-terminated C string (test input is never NUL-containing).
pub fn cstr(s: &str) -> CString {
    CString::new(s).expect("test string has no interior NUL")
}

/// The thread-local last-error detail, as a Rust string.
pub fn last_error() -> String {
    unsafe {
        let p = lev_last_error();
        if p.is_null() {
            return String::new();
        }
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// The bound `ip:port` of `node`'s TCP server listener number `index`.
///
/// Codeberg #221: the node binds `:0` itself and the test reads the chosen
/// port back here, so there is no window in which another process can take a
/// pre-probed port between probe and bind.
pub fn tcp_listen_addr(node: &Node, index: usize) -> String {
    let v = read2(|b, c, l| unsafe { lev_tcp_listen_addr(node.0, index, b, c, l) })
        .expect("tcp listen addr");
    String::from_utf8(v).expect("utf-8 listen addr")
}

/// The bound port of `node`'s TCP server listener number `index`.
pub fn tcp_listen_port(node: &Node, index: usize) -> u16 {
    let addr = tcp_listen_addr(node, index);
    addr.rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("no port in listen addr {addr}"))
}

/// Build and start a node whose first interface is a TCP server on
/// `127.0.0.1:0`, returning it together with the listener's actual `ip:port`
/// (see [`tcp_listen_addr`]). The closure configures the rest of the builder
/// (identity, more interfaces, …).
pub fn start_tcp_server_node(
    storage: &Path,
    configure: impl FnOnce(*mut lev_builder_t),
) -> (Node, String) {
    let node = start_node(storage, |b| unsafe {
        let addr = cstr("127.0.0.1:0");
        assert_eq!(lev_builder_add_tcp_server(b, addr.as_ptr()), LEV_OK);
        configure(b);
    });
    let addr = tcp_listen_addr(&node, 0);
    (node, addr)
}

/// A workspace-unique token for names that must not collide between
/// concurrently running test processes (shared-instance socket names). Not a
/// port: pid plus a process-local counter, no kernel resource involved.
pub fn unique_token() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Owning wrapper for a node handle; frees on drop.
pub struct Node(pub *mut leviculum_t);

impl Drop for Node {
    fn drop(&mut self) {
        unsafe { lev_free(self.0) }
    }
}

impl Node {
    pub fn ptr(&self) -> *mut leviculum_t {
        self.0
    }
}

/// Owning wrapper for an event handle; frees on drop.
pub struct Event(pub *mut lev_event_t);

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { lev_event_free(self.0) }
    }
}

impl Event {
    pub fn ty(&self) -> c_int {
        unsafe { lev_event_type(self.0) }
    }
}

/// Owning wrapper for a link handle; closes and frees on drop.
pub struct Link(pub *mut lev_link_t);

impl Drop for Link {
    fn drop(&mut self) {
        unsafe { lev_link_free(self.0) }
    }
}

impl Link {
    pub fn id(&self) -> [u8; 16] {
        let mut h = [0u8; 16];
        let mut l = 16usize;
        let rc = unsafe { lev_link_id(self.0, h.as_mut_ptr(), 16, &mut l) };
        assert_eq!(rc, LEV_OK);
        h
    }
}

/// Owning wrapper for an identity handle; frees on drop.
pub struct Identity(pub *mut lev_identity_t);

impl Drop for Identity {
    fn drop(&mut self) {
        unsafe { lev_identity_free(self.0) }
    }
}

impl Identity {
    pub fn generate() -> Identity {
        let p = lev_identity_generate();
        assert!(!p.is_null(), "identity_generate failed");
        Identity(p)
    }

    pub fn hash(&self) -> [u8; 16] {
        let mut h = [0u8; 16];
        let mut l = 16usize;
        let rc = unsafe { lev_identity_hash(self.0, h.as_mut_ptr(), 16, &mut l) };
        assert_eq!(rc, LEV_OK);
        h
    }
}

/// Build and start a node, configuring the builder via the closure (which adds
/// interfaces, an identity, etc.). Panics with the detail on failure.
pub fn start_node(storage: &Path, configure: impl FnOnce(*mut lev_builder_t)) -> Node {
    unsafe {
        let b = lev_builder_new();
        assert!(!b.is_null());
        let sp = cstr(storage.to_str().expect("utf-8 path"));
        assert_eq!(lev_builder_storage_path(b, sp.as_ptr()), LEV_OK);
        configure(b);
        let node = lev_builder_build(b);
        lev_builder_free(b);
        assert!(!node.is_null(), "build failed: {}", last_error());
        assert_eq!(lev_start(node), LEV_OK, "start failed: {}", last_error());
        Node(node)
    }
}

/// Register a new incoming SINGLE destination on `node` and return its 16-byte
/// hash. `identity` may be null for a node-default destination test, but is
/// normally the node's own identity.
pub fn register_single_dest(
    node: *mut leviculum_t,
    identity: *mut lev_identity_t,
    app_name: &str,
    aspects: &[&str],
) -> [u8; 16] {
    unsafe {
        let app_c = cstr(app_name);
        let aspect_cs: Vec<CString> = aspects.iter().map(|a| cstr(a)).collect();
        let aspect_ptrs: Vec<*const c_char> = aspect_cs.iter().map(|c| c.as_ptr()).collect();
        let dest = lev_destination_new(
            identity,
            LEV_DIRECTION_IN,
            LEV_DEST_SINGLE,
            app_c.as_ptr(),
            aspect_ptrs.as_ptr(),
            aspect_ptrs.len(),
        );
        assert!(!dest.is_null(), "destination_new: {}", last_error());
        let mut h = [0u8; 16];
        let mut l = 16usize;
        assert_eq!(
            lev_destination_hash(dest, h.as_mut_ptr(), 16, &mut l),
            LEV_OK
        );
        assert_eq!(lev_register_destination(node, dest), LEV_OK);
        lev_destination_free(dest);
        h
    }
}

/// Drain events on `node` up to `timeout`, returning the first event whose type
/// equals `want`. Non-matching events are freed. Returns `None` on timeout.
pub fn wait_event(node: *mut leviculum_t, want: c_int, timeout: Duration) -> Option<Event> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let ms = remaining.as_millis().min(200) as c_int;
        let mut ev: *mut lev_event_t = ptr::null_mut();
        let rc = unsafe { lev_wait_event(node, &mut ev, ms) };
        if rc != LEV_OK {
            return None;
        }
        if ev.is_null() {
            continue;
        }
        if unsafe { lev_event_type(ev) } == want {
            return Some(Event(ev));
        }
        unsafe { lev_event_free(ev) }
    }
}

/// Run a read(2)-style accessor closure, returning the bytes. The closure is
/// `|buf, cap, out_len| -> rc`. Performs a size query then a fill.
pub fn read2<F>(f: F) -> Result<Vec<u8>, c_int>
where
    F: Fn(*mut u8, usize, *mut usize) -> c_int,
{
    let mut len: usize = 0;
    let rc = f(ptr::null_mut(), 0, &mut len);
    if rc != LEV_ERR_BUFFER_TOO_SMALL && rc != LEV_OK {
        return Err(rc);
    }
    let mut buf = vec![0u8; len];
    let rc = f(buf.as_mut_ptr(), buf.len(), &mut len);
    if rc != LEV_OK {
        return Err(rc);
    }
    buf.truncate(len);
    Ok(buf)
}

/// Read an event's link id (16 bytes), or panic.
pub fn event_link_id(ev: &Event) -> [u8; 16] {
    let v = read2(|b, c, l| unsafe { lev_event_link_id(ev.0, b, c, l) }).expect("link id");
    v.try_into().expect("16 bytes")
}

/// Read an event's destination hash (16 bytes), or panic.
pub fn event_dest_hash(ev: &Event) -> [u8; 16] {
    let v = read2(|b, c, l| unsafe { lev_event_dest_hash(ev.0, b, c, l) }).expect("dest hash");
    v.try_into().expect("16 bytes")
}

/// Read an event's primary payload.
pub fn event_data(ev: &Event) -> Vec<u8> {
    read2(|b, c, l| unsafe { lev_event_data(ev.0, b, c, l) }).expect("event data")
}

/// Re-announce A's destination until B has learned a path to it (and thus A's
/// cached identity). Panics if B never learns the path.
pub fn learn(a: &Node, b: &Node, dest: &[u8; 16]) {
    for _ in 0..50 {
        unsafe { lev_announce(a.0, dest.as_ptr(), ptr::null(), 0, 2000) };
        let mut ev: *mut lev_event_t = ptr::null_mut();
        unsafe { lev_wait_event(b.0, &mut ev, 300) };
        while !ev.is_null() {
            unsafe {
                lev_event_free(ev);
                ev = ptr::null_mut();
            }
            if unsafe { lev_next_event(b.0, &mut ev) } != LEV_OK {
                break;
            }
        }
        if unsafe { lev_has_path(b.0, dest.as_ptr()) } == 1 {
            return;
        }
    }
    panic!("B never learned a path to A");
}

/// Connect B to A's destination. The inbound link is auto-accepted on A
/// (Python-RNS parity), so A sees an inbound LinkEstablished; mint its handle.
/// Wait for the outbound established event on B. Returns `(b_link, a_link)`.
pub fn establish_link(a: &Node, b: &Node, dest: &[u8; 16]) -> (Link, Link) {
    let ev = Duration::from_secs(5);
    let mut lb: *mut lev_link_t = ptr::null_mut();
    assert_eq!(
        unsafe { lev_connect(b.0, dest.as_ptr(), 5000, &mut lb) },
        LEV_OK,
        "connect: {}",
        last_error()
    );
    assert!(!lb.is_null());

    let req = wait_event(a.0, LEV_EVENT_LINK_ESTABLISHED, ev).expect("inbound link on A");
    let lid = event_link_id(&req);
    drop(req);

    let mut la: *mut lev_link_t = ptr::null_mut();
    assert_eq!(
        unsafe { lev_accept_link(a.0, lid.as_ptr(), 5000, &mut la) },
        LEV_OK
    );
    assert!(!la.is_null());

    wait_event(b.0, LEV_EVENT_LINK_ESTABLISHED, ev).expect("established on B");
    (Link(lb), Link(la))
}
