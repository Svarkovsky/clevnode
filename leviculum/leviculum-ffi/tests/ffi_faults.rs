//! Fault-path tests: deliberately trigger every error code and failure event
//! and assert the C API reports it correctly. Network faults are injected with
//! an in-process [`FaultProxy`] (block = silence, cut = disconnect) so link,
//! path, and resource failures are deterministic. See `fault_coverage.rs` for
//! the guard that keeps this coverage complete.

mod support;

use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use leviculum::*;
use support::{
    cstr, establish_link, last_error, learn, register_single_dest, start_node, wait_event,
    FaultProxy, Identity, Node,
};

const EV: Duration = Duration::from_secs(5);

/// Serialize the node-spawning tests. Each test starts two nodes (two tokio
/// runtimes) plus proxy threads; running them in parallel starves the per-node
/// timers and makes the timeout-driven faults (stale, resource failure) miss
/// their deadlines. Holding this lock keeps the timing-sensitive paths
/// deterministic. Poison-tolerant so one failing test does not cascade.
///
/// `lev_free` tears nodes down with tokio's `shutdown_background`, which
/// detaches the runtime instead of waiting for it. Settle briefly after taking
/// the lock so the previous test's runtime has largely drained before this one
/// builds its nodes, keeping the timer-driven faults off a loaded scheduler.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    std::thread::sleep(Duration::from_millis(500));
    guard
}

/// Two nodes over a direct TCP loopback (no fault proxy): A is a server with a
/// registered SINGLE destination that B has learned a path to.
struct Pair {
    a: Node,
    b: Node,
    dest: [u8; 16],
    _ida: Identity,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// Build a pair. When `use_proxy`, B connects through a returned [`FaultProxy`]
/// pointed at A (so the connection can be cut or silenced); otherwise B
/// connects straight to A and the proxy is `None`. `keepalive` overrides the
/// link keepalive on both nodes (shrinks the stale timeout for stale tests).
fn build_pair(use_proxy: bool, keepalive: Option<u64>) -> (Pair, Option<FaultProxy>) {
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let id_ptr = ida.0;

    // A binds `:0` and reports its port (Codeberg #221); the proxy, when
    // used, points at whatever A got. It only dials upstream per accepted
    // connection, so A being up first changes nothing for it.
    let (a, _a_addr) = support::start_tcp_server_node(da.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, id_ptr), LEV_OK);
        if let Some(k) = keepalive {
            assert_eq!(lev_builder_link_keepalive(b, k), LEV_OK);
        }
    });
    let aport = support::tcp_listen_port(&a, 0);
    let proxy = if use_proxy {
        Some(FaultProxy::spawn(aport))
    } else {
        None
    };
    let client_port = proxy.as_ref().map(|p| p.port).unwrap_or(aport);
    let client_addr = cstr(&format!("127.0.0.1:{client_port}"));
    let client_ptr = client_addr.as_ptr();

    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_add_tcp_client(b, client_ptr), LEV_OK);
        if let Some(k) = keepalive {
            assert_eq!(lev_builder_link_keepalive(b, k), LEV_OK);
        }
    });

    let dest = register_single_dest(a.0, id_ptr, "levfault", &["faults"]);
    learn(&a, &bnode, &dest);
    (
        Pair {
            a,
            b: bnode,
            dest,
            _ida: ida,
            _dirs: (da, db),
        },
        proxy,
    )
}

// --- Error codes returned by call-site checks ------------------------------

#[test]
fn operations_after_stop_report_not_running() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let ida = Identity::generate();
    let node = start_node(dir.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, ida.0), LEV_OK);
    });
    let dest = register_single_dest(node.0, ida.0, "levfault", &["stop"]);
    assert_eq!(unsafe { lev_stop(node.0) }, LEV_OK);

    // An announce after the loop has stopped must report NOT_RUNNING, not panic
    // or silently succeed.
    let rc = unsafe { lev_announce(node.0, dest.as_ptr(), ptr::null(), 0, 1000) };
    assert_eq!(rc, LEV_ERR_NOT_RUNNING, "after stop: {}", last_error());
}

#[test]
fn link_try_send_reports_again_under_backpressure() {
    let _serial = serial();
    let (p, _proxy) = build_pair(false, None);
    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);

    // Flood the non-blocking send path without ever draining the channel; the
    // bounded outbound window fills and reports retryable backpressure.
    let payload = [0xABu8; 400];
    let mut saw_again = false;
    for _ in 0..2000 {
        let rc = unsafe { lev_link_try_send(lb.0, payload.as_ptr(), payload.len()) };
        if rc == LEV_ERR_AGAIN {
            saw_again = true;
            break;
        }
        assert!(
            rc == LEV_OK || rc == LEV_ERR_AGAIN,
            "unexpected try_send rc {rc}: {}",
            last_error()
        );
    }
    assert!(
        saw_again,
        "channel never reported LEV_ERR_AGAIN backpressure"
    );
}

#[test]
fn oversized_datagram_reports_send_error() {
    let _serial = serial();
    let (p, _proxy) = build_pair(false, None);
    // A datagram is a single packet; a payload far larger than the MDU cannot
    // be sent and must surface as the generic send error (not no-path: the
    // path is known).
    let big = vec![0x5Au8; 200_000];
    let mut out = [0u8; 32];
    let rc = unsafe {
        lev_send_datagram(
            p.b.0,
            p.dest.as_ptr(),
            big.as_ptr(),
            big.len(),
            out.as_mut_ptr(),
            2000,
        )
    };
    assert_eq!(rc, LEV_ERR_SEND, "oversized datagram: {}", last_error());
}

#[test]
fn resource_on_unknown_link_reports_resource_error() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(dir.path(), |_| {});
    let fake_link = [0x11u8; 16];
    let data = b"payload";
    let mut out = [0u8; 32];
    let rc = unsafe {
        lev_send_resource(
            node.0,
            fake_link.as_ptr(),
            data.as_ptr(),
            data.len(),
            ptr::null(),
            0,
            0,
            out.as_mut_ptr(),
            2000,
        )
    };
    assert_eq!(
        rc,
        LEV_ERR_RESOURCE,
        "unknown-link resource: {}",
        last_error()
    );
}

#[test]
fn request_on_unknown_link_reports_request_error() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let node = start_node(dir.path(), |_| {});
    let fake_link = [0x22u8; 16];
    let path = cstr("levfault.req");
    let mut out = [0u8; 16];
    let rc = unsafe {
        lev_send_request(
            node.0,
            fake_link.as_ptr(),
            path.as_ptr(),
            ptr::null(),
            0,
            1000,
            out.as_mut_ptr(),
        )
    };
    assert_eq!(
        rc,
        LEV_ERR_REQUEST,
        "unknown-link request: {}",
        last_error()
    );
}

#[test]
fn identity_save_to_bad_path_reports_io() {
    let id = Identity::generate();
    let dir = tempfile::tempdir().unwrap();
    // Parent directory does not exist, so the write fails with an I/O error.
    let bad = dir.path().join("missing").join("identity");
    let bad_c = cstr(bad.to_str().unwrap());
    let rc = unsafe { lev_identity_save_file(id.0, bad_c.as_ptr()) };
    assert_eq!(rc, LEV_ERR_IO, "save to bad path: {}", last_error());
}

// --- Failure events injected with the fault proxy --------------------------

#[test]
fn cutting_the_interface_emits_path_lost() {
    let _serial = serial();
    // A reconnecting TCP client keeps its channel open across a drop, so the
    // path must be learned on an interface that actually closes: the server's
    // per-peer connection. B (the client) announces a destination that A (the
    // server) learns over that peer connection; cutting the proxy closes it on
    // A, so A culls the path and emits PATH_LOST.
    let da = tempfile::tempdir().unwrap();
    let db = tempfile::tempdir().unwrap();
    let bid = Identity::generate();

    let (a, _a_addr) = support::start_tcp_server_node(da.path(), |_b| {});
    let aport = support::tcp_listen_port(&a, 0);
    let proxy = FaultProxy::spawn(aport);
    let client_addr = cstr(&format!("127.0.0.1:{}", proxy.port));
    let bnode = start_node(db.path(), |b| unsafe {
        assert_eq!(lev_builder_identity(b, bid.0), LEV_OK);
        assert_eq!(lev_builder_add_tcp_client(b, client_addr.as_ptr()), LEV_OK);
    });

    let bdest = register_single_dest(bnode.0, bid.0, "levfault", &["pathlost"]);
    learn(&bnode, &a, &bdest); // B announces, A learns the path over the peer link

    proxy.cut();
    let ev = wait_event(a.0, LEV_EVENT_PATH_LOST, Duration::from_secs(45));
    assert!(ev.is_some(), "A never reported PATH_LOST after the cut");
}

#[test]
fn silencing_a_transfer_emits_resource_failed() {
    let _serial = serial();
    // Large keepalive so the silenced link does NOT go stale and close during
    // the test. On a loopback link the RTT-derived keepalive otherwise clamps
    // to the 5s minimum, so the link would close (~15s) before the resource
    // adv-retry fails, and a stale-close does not fail the in-flight resource,
    // leaving no RESOURCE_FAILED. Keeping the link up isolates the resource
    // failure path.
    let (p, proxy) = build_pair(true, Some(600));
    let proxy = proxy.expect("proxy requested");
    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);

    // Silence the link, then start a transfer: the advertisement never reaches
    // the receiver, so the sender's outgoing resource exhausts its retries and
    // fails. is_sender is true on this side.
    proxy.block();
    let data = vec![0x7Eu8; 4096];
    let mut hash = [0u8; 32];
    let rc = unsafe {
        lev_send_resource(
            p.b.0,
            lb.id().as_ptr(),
            data.as_ptr(),
            data.len(),
            ptr::null(),
            0,
            0,
            hash.as_mut_ptr(),
            5000,
        )
    };
    assert_eq!(rc, LEV_OK, "send_resource initiate: {}", last_error());

    let ev = wait_event(p.b.0, LEV_EVENT_RESOURCE_FAILED, Duration::from_secs(45))
        .expect("sender never reported RESOURCE_FAILED");
    assert_eq!(
        unsafe { lev_event_is_sender(ev.0) },
        1,
        "the failing resource is the one we sent"
    );
}

#[test]
fn closing_a_link_emits_link_closed() {
    let _serial = serial();
    let (p, _proxy) = build_pair(false, None);
    let (_lb, la) = establish_link(&p.a, &p.b, &p.dest);

    // Closing the link surfaces a LINK_CLOSED event (reason Normal) on the
    // node that closed it, distinct from merely failing the next send.
    assert_eq!(unsafe { lev_close_link(la.0, 2000) }, LEV_OK);
    let ev = wait_event(p.a.0, LEV_EVENT_LINK_CLOSED, EV);
    assert!(ev.is_some(), "the closing node never reported LINK_CLOSED");
}

#[test]
fn silence_makes_the_link_go_stale() {
    let _serial = serial();
    // Keepalive override of 5s (the protocol minimum) makes the stale timeout
    // 10s, so this runs in seconds instead of the 12-minute default.
    let (p, proxy) = build_pair(true, Some(5));
    let proxy = proxy.expect("proxy requested");
    let (lb, _la) = establish_link(&p.a, &p.b, &p.dest);

    // Send one message so the responder's link is fully active with a recorded
    // inbound timestamp to measure staleness from (is_stale ignores a link that
    // has not received inbound).
    assert_eq!(
        unsafe { lev_link_send(lb.0, b"hi".as_ptr(), 2, 5000) },
        LEV_OK
    );
    wait_event(p.a.0, LEV_EVENT_LINK_MESSAGE, EV).expect("A receives the priming message");

    // Silence the link: with no inbound traffic the responder passes its stale
    // deadline and reports the link stale. (Recovery, the LINK_RECOVERED event,
    // is intentionally not asserted here: it can only happen inside the engine's
    // fixed few-second stale-before-close window, which races timer scheduling
    // and cannot be made deterministic in-process. See fault_coverage.rs.)
    proxy.block();
    let stale = wait_event(p.a.0, LEV_EVENT_LINK_STALE, Duration::from_secs(45));
    assert!(stale.is_some(), "A never reported LINK_STALE under silence");
}
