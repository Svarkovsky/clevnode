//! Codeberg #87 hold-and-release interop tests (A/B: our lnsd vs Python rnsd).
//!
//! These exercise the ingress-control HOLD-AND-RELEASE behavior end to end and
//! assert MATCHING behavior on both stacks. When a receiving interface crosses
//! the ingress announce-frequency threshold, excess announces for unknown
//! destinations are HELD (queued, up to MAX_HELD_ANNOUNCES=256) and released
//! slowly, rather than DROPPED. A burst therefore DELAYS propagation instead of
//! losing it (Python Interface.hold_announce / process_held_announces,
//! Interface.py:228-253; Transport.py:1705-1707,943).
//!
//! ## Topology (identical for both stacks)
//!
//! ```text
//!   injector (raw TCP) --burst of N distinct announces--> DUT --forward--> receiver (Python rnsd)
//! ```
//!
//! The injector opens a single raw RNS/TCP connection to the DUT and blasts N
//! distinct, unknown-destination announces well above the 3 Hz new-interface
//! threshold. The DUT (our lnsd in-process, or a Python rnsd) applies ingress
//! control on that one connection interface, holds the excess, and forwards
//! everything (immediately-passed + later-released) to a downstream Python rnsd
//! receiver.
//!
//! ## A/B introspection
//!
//! held_announces / burst_active are read the same way conceptually on both
//! stacks:
//!   * Rust lnsd DUT: `node.interface_stats()` (InterfaceStatusSnapshot).
//!   * Python rnsd DUT: `daemon.get_interfaces()` (InterfaceInfo), whose
//!     `held_announces` / `burst_active` are surfaced by the test daemon from
//!     `Interface.held_announces` / `ic_burst_active`.
//!
//! ## Anti-flake design (A/B discipline: count volumes, avoid wall-clock)
//!
//!   * HOLD-NOT-DROP asserts on the COUNT of destinations that eventually reach
//!     the receiver (`wait_for_all_paths`, eventual-consistency polling), never
//!     on when a specific one arrives. All N must arrive; that is the property
//!     that distinguishes hold from drop.
//!   * VISIBILITY reads held_announces / burst_active INSIDE the release-penalty
//!     window. Python holds for IC_BURST_PENALTY=15 s before the first release,
//!     so a read a fraction of a second after the blast has a ~15 s margin: it
//!     cannot race the drain. The injector connection is kept open for the whole
//!     test so the interface (and its held queue) is never torn down early.
//!   * CAP reads the held count in the same 15 s pre-release window after
//!     injecting well over the cap, so no release has happened yet when the cap
//!     is measured.
//!   * Timeouts are sized to the known release schedule (15 s penalty + one
//!     release per 5 s), never to a guessed absolute latency.
//!
//! These are marked `#[ignore]`: they spawn Python daemon(s) and extra local
//! processes, so the reviewer runs them AFTER the tier3 HW run (which owns the
//! rig + Docker + daemon spawning). Run with:
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop \
//!     held_announce_interop_tests -- --ignored --nocapture
//! ```

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;

use leviculum_core::DestinationHash;
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{
    build_announce_raw, init_tracing, send_framed, temp_storage, DAEMON_SETTLE_TIME,
};
use crate::harness::{find_available_ports, TestDaemon};

/// Python MAX_HELD_ANNOUNCES / our MAX_HELD_ANNOUNCES (Interface.py:70).
const MAX_HELD_ANNOUNCES: usize = 256;

/// Distinct announces in a hold-not-drop / visibility burst: comfortably over
/// the 3 Hz new-interface threshold and under the 256 cap, small enough that the
/// drain finishes inside the propagation deadline.
const BURST_N: usize = 8;

/// Blast `n` distinct unknown-destination announces down one open connection at
/// ~25 Hz (well over the 3 Hz new-interface threshold). Returns the destination
/// hashes so the caller can assert they all propagate.
async fn blast_announces(stream: &mut TcpStream, n: usize) -> Vec<DestinationHash> {
    let mut dests = Vec::with_capacity(n);
    for i in 0..n {
        let aspect = format!("d{i}");
        let (raw, dest_hash, _dest) =
            build_announce_raw("held_interop", &[aspect.as_str()], b"burst");
        send_framed(stream, &raw).await;
        dests.push(dest_hash);
        // ~25 Hz: fast enough to trip the burst after the min-sample gate,
        // paced so the frames stay distinct packets on the wire.
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    dests
}

/// Poll the receiver's path table until every `dest` is present or the deadline
/// passes. Returns the number that propagated (== dests.len() on full success).
/// Eventual-consistency: never couples to a single announce's arrival time.
async fn wait_for_all_paths(
    receiver: &TestDaemon,
    dests: &[DestinationHash],
    deadline: Duration,
) -> usize {
    let start = Instant::now();
    loop {
        let paths = receiver.get_path_table().await.unwrap_or_default();
        let have = dests
            .iter()
            .filter(|d| paths.contains_key(&hex::encode(d.as_bytes())))
            .count();
        if have == dests.len() || start.elapsed() >= deadline {
            return have;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// =========================================================================
// Test 1 + 2: HOLD-NOT-DROP and VISIBILITY — our lnsd DUT
// =========================================================================

/// Drive a burst into our Rust lnsd acting as a transit node; assert the burst
/// is held (visible) and that every announce still propagates to a downstream
/// Python receiver (none lost), then that the queue drains back to zero.
#[tokio::test]
#[ignore = "spawns a Python daemon; reviewer runs after the tier3"]
async fn hold_not_drop_and_visibility_rust_lnsd_dut() {
    init_tracing();

    // The harness allocator hands out a minimum of two ports; only the first
    // is needed here (the DUT's TCP server bind).
    let (ports, _port_alloc) = find_available_ports::<2>()
        .await
        .expect("allocate DUT tcp port");
    let dut_tcp_addr: SocketAddr = format!("127.0.0.1:{}", ports[0]).parse().unwrap();

    // Downstream receiver (Python rnsd, transport enabled).
    let receiver = TestDaemon::start().await.expect("start receiver daemon");

    // DUT: our lnsd, transit node. Accepts the injector on its TCP server and
    // forwards to the receiver via a TCP client interface.
    let _storage = temp_storage("hold_not_drop_rust_lnsd_dut", "dut");
    let mut dut = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_server(dut_tcp_addr)
        .add_tcp_client(receiver.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build DUT node");
    dut.start().await.expect("start DUT node");

    // Let the DUT connect to the receiver.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Injector: one raw RNS/TCP connection into the DUT, kept open for the whole
    // test so the interface (and its held queue) is never torn down.
    let mut injector = TcpStream::connect(dut_tcp_addr)
        .await
        .expect("connect injector to DUT");
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    let dests = blast_announces(&mut injector, BURST_N).await;

    // Give the DUT a moment to process the burst (still far inside the 15 s
    // release-penalty window).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // VISIBILITY: the burst is held and flagged active on some interface.
    let stats = dut.interface_stats();
    let held_now: usize = stats.iter().map(|s| s.held_announces).sum();
    assert!(
        held_now > 0,
        "DUT must HOLD excess announces during the burst (held={held_now})"
    );
    assert!(
        stats.iter().any(|s| s.burst_active),
        "DUT must report burst_active during the burst"
    );

    // HOLD-NOT-DROP: every announce eventually reaches the receiver. Deadline
    // covers 15 s penalty + one release per 5 s for the held remainder, plus
    // slack.
    let propagated = wait_for_all_paths(&receiver, &dests, Duration::from_secs(90)).await;
    assert_eq!(
        propagated,
        dests.len(),
        "all {} announces must propagate (held + released, none dropped); got {}",
        dests.len(),
        propagated
    );

    // Queue drains back to zero once released.
    let drain_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let held: usize = dut.interface_stats().iter().map(|s| s.held_announces).sum();
        if held == 0 || Instant::now() >= drain_deadline {
            assert_eq!(held, 0, "held queue must drain to zero after release");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    drop(injector);
    dut.stop().await.expect("stop DUT node");
}

// =========================================================================
// Test 1 + 2: HOLD-NOT-DROP and VISIBILITY — Python rnsd DUT (reference)
// =========================================================================

/// Same scenario against a Python rnsd DUT: assert the SAME hold-not-drop and
/// visibility behavior the reference implementation produces.
#[tokio::test]
#[ignore = "spawns Python daemons; reviewer runs after the tier3"]
async fn hold_not_drop_and_visibility_python_rnsd_dut() {
    init_tracing();

    // Downstream receiver and the Python DUT (both transport enabled).
    let receiver = TestDaemon::start().await.expect("start receiver daemon");
    let dut = TestDaemon::start().await.expect("start Python DUT daemon");

    // DUT forwards to the receiver.
    dut.add_client_interface("127.0.0.1", receiver.rns_port(), Some("ToReceiver"))
        .await
        .expect("connect DUT to receiver");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Injector: one raw RNS/TCP connection into the DUT, kept open throughout.
    let mut injector = TcpStream::connect(dut.rns_addr())
        .await
        .expect("connect injector to Python DUT");
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    let dests = blast_announces(&mut injector, BURST_N).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // VISIBILITY on the reference stack.
    let ifaces = dut.get_interfaces().await.expect("get DUT interfaces");
    let held_now: usize = ifaces.iter().map(|i| i.held_announces).sum();
    assert!(
        held_now > 0,
        "Python DUT must HOLD excess announces during the burst (held={held_now})"
    );
    assert!(
        ifaces.iter().any(|i| i.burst_active),
        "Python DUT must report burst_active during the burst"
    );

    // HOLD-NOT-DROP on the reference stack.
    let propagated = wait_for_all_paths(&receiver, &dests, Duration::from_secs(90)).await;
    assert_eq!(
        propagated,
        dests.len(),
        "all {} announces must propagate on Python rnsd (held + released); got {}",
        dests.len(),
        propagated
    );

    // Queue drains back to zero.
    let drain_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ifaces = dut.get_interfaces().await.expect("get DUT interfaces");
        let held: usize = ifaces.iter().map(|i| i.held_announces).sum();
        if held == 0 || Instant::now() >= drain_deadline {
            assert_eq!(held, 0, "Python DUT held queue must drain to zero");
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    drop(injector);
}

// =========================================================================
// Test 3: CAP at MAX_HELD_ANNOUNCES — both stacks
// =========================================================================

/// A burst well beyond MAX_HELD_ANNOUNCES caps the held queue at 256 on our
/// lnsd (excess handled as Python does: new destinations rejected at the cap).
#[tokio::test]
#[ignore = "spawns a Python daemon; reviewer runs after the tier3"]
async fn held_queue_caps_at_max_rust_lnsd_dut() {
    init_tracing();

    // Minimum allocation is two ports; only the first is used (DUT TCP server).
    let (ports, _port_alloc) = find_available_ports::<2>()
        .await
        .expect("allocate DUT tcp port");
    let dut_tcp_addr: SocketAddr = format!("127.0.0.1:{}", ports[0]).parse().unwrap();

    let receiver = TestDaemon::start().await.expect("start receiver daemon");

    let _storage = temp_storage("held_queue_caps_rust_lnsd_dut", "dut");
    let mut dut = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_server(dut_tcp_addr)
        .add_tcp_client(receiver.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build DUT node");
    dut.start().await.expect("start DUT node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut injector = TcpStream::connect(dut_tcp_addr)
        .await
        .expect("connect injector to DUT");
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    // Inject well over the cap. Read the held count inside the 15 s pre-release
    // window, so no release has thinned the queue when we measure the cap.
    blast_over_cap(&mut injector).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let max_held: usize = dut
        .interface_stats()
        .iter()
        .map(|s| s.held_announces)
        .max()
        .unwrap_or(0);
    assert_eq!(
        max_held, MAX_HELD_ANNOUNCES,
        "held queue must cap at MAX_HELD_ANNOUNCES on our lnsd, got {max_held}"
    );

    drop(injector);
    dut.stop().await.expect("stop DUT node");
}

/// Same cap behavior on the Python rnsd reference stack.
#[tokio::test]
#[ignore = "spawns a Python daemon; reviewer runs after the tier3"]
async fn held_queue_caps_at_max_python_rnsd_dut() {
    init_tracing();

    let dut = TestDaemon::start().await.expect("start Python DUT daemon");

    let mut injector = TcpStream::connect(dut.rns_addr())
        .await
        .expect("connect injector to Python DUT");
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    blast_over_cap(&mut injector).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let max_held: usize = dut
        .get_interfaces()
        .await
        .expect("get DUT interfaces")
        .iter()
        .map(|i| i.held_announces)
        .max()
        .unwrap_or(0);
    assert_eq!(
        max_held, MAX_HELD_ANNOUNCES,
        "held queue must cap at MAX_HELD_ANNOUNCES on Python rnsd, got {max_held}"
    );

    drop(injector);
}

/// Blast (MAX_HELD_ANNOUNCES + 48) distinct announces as fast as the connection
/// accepts them, sustaining the burst so the queue fills to the cap and the
/// surplus is rejected. Faster than `blast_announces` (no inter-frame delay):
/// all frames land inside the 15 s pre-release window.
async fn blast_over_cap(stream: &mut TcpStream) {
    let total = MAX_HELD_ANNOUNCES + 48;
    for i in 0..total {
        let aspect = format!("cap{i}");
        let (raw, _dest_hash, _dest) =
            build_announce_raw("held_cap_interop", &[aspect.as_str()], b"cap");
        send_framed(stream, &raw).await;
        // Tiny yield keeps frames distinct without letting the 5 s release
        // interval elapse across the whole blast.
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
}
