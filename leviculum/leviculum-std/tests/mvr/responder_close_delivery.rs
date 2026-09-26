//! mvr for Codeberg #77 — responder-initiated graceful close usually not
//! delivered to the initiator over a real (TCP) interface.
//!
//! Topology: two in-process `ReticulumNode`s on `127.0.0.1` connected by
//! TCP loopback. A = TCP **server** + responder (registers a destination,
//! announces it). B = TCP **client** + initiator (installs the path from
//! A's announce, then `connect`s a link to A's destination). Once the link
//! is established on both ends, A `close_link`s the responder-side link and
//! B waits (bounded) for its `LinkClosed`.
//!
//! Per the ledger (#77): the closing node always gets its own LinkClosed,
//! but the *peer* notification (server-accepted-peer -> client) is delivered
//! in only ~1/8 runs. Loopback loses no packets, so any miss is a software
//! dispatch / timing nondeterminism, not network loss.
//!
//! `responder_close_delivery_rate` runs N iterations and records the
//! delivery rate. It is the RED reproduction: it asserts 100% delivery, so
//! it fails on master while the bug is live and prints the measured rate.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use leviculum_core::link::{LinkCloseReason, LinkId};
use leviculum_core::{Destination, DestinationType, Direction, Identity};
use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::NodeEvent;

/// Listener port from the host-wide allocator shared by every suite
/// (`crate::harness::port_alloc`). This file used to carry its own
/// `AtomicU16` at a private base, which is why the bases were chosen
/// disjoint; one counter per host does not need disjoint bases and, unlike
/// a per-process one, does not hand the same number to a concurrently
/// running test binary.
fn next_port() -> u16 {
    crate::harness::port_alloc::free_tcp_port()
}

#[derive(Debug)]
struct RunOutcome {
    /// Did B receive a LinkClosed for the initiator link within the wait?
    delivered: bool,
    /// Did A emit its own LinkClosed (should always be true)?
    closer_self_notified: bool,
    /// Milliseconds from A.close_link to B's LinkClosed, if delivered.
    delivery_ms: Option<u128>,
    /// Free-form note for failed setups.
    note: String,
}

/// What the responder does immediately after `close_link`.
#[derive(Clone, Copy, PartialEq)]
enum TeardownMode {
    /// Keep A alive while B waits (control: close should always be flushed).
    KeepAlive,
    /// Call `a.stop()` right after `close_link` returns, before B can read —
    /// the H3 flush-vs-teardown seam (graceful shutdown branch wins the race).
    StopImmediately,
    /// `drop(a)` right after `close_link` returns. Drop calls
    /// `runtime.shutdown_background()`, which *aborts* the event loop and the
    /// TCP task immediately — no graceful break, no draining. This is the
    /// widest race window and matches the ffi-agent's ~1/8-delivery report.
    DropImmediately,
}

/// Run one full establish -> responder-close -> wait-for-peer-close cycle
/// in the given teardown mode.
async fn run_once_mode(iter: usize, teardown: TeardownMode) -> RunOutcome {
    let server_port = next_port();
    let server_addr: SocketAddr = format!("127.0.0.1:{server_port}").parse().unwrap();

    // A: server + responder.
    let a_storage = tempfile::tempdir().expect("tempdir A");
    let mut a = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_server(server_addr)
        .storage_path(a_storage.path().to_path_buf())
        .build()
        .await
        .expect("build A");
    a.start().await.expect("start A");

    // B: client connecting directly to A.
    let b_storage = tempfile::tempdir().expect("tempdir B");
    let mut b = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(server_addr)
        .storage_path(b_storage.path().to_path_buf())
        .build()
        .await
        .expect("build B");
    b.start().await.expect("start B");

    // Drain A's events: capture the responder-side link_id and whether A
    // ever emits its own LinkClosed.
    let mut a_rx = a.take_event_receiver().expect("A event rx");
    let (a_link_tx, mut a_link_rx) = tokio::sync::mpsc::unbounded_channel::<LinkId>();
    let (a_closed_tx, mut a_closed_rx) = tokio::sync::mpsc::unbounded_channel::<LinkId>();
    let a_drain = tokio::spawn(async move {
        while let Some(ev) = a_rx.recv().await {
            match ev {
                NodeEvent::LinkEstablished {
                    link_id,
                    is_initiator: false,
                    ..
                } => {
                    let _ = a_link_tx.send(link_id);
                }
                NodeEvent::LinkClosed { link_id, .. } => {
                    let _ = a_closed_tx.send(link_id);
                }
                _ => {}
            }
        }
    });

    // Drain B's events: capture initiator-side LinkEstablished + LinkClosed.
    let mut b_rx = b.take_event_receiver().expect("B event rx");
    let (b_est_tx, mut b_est_rx) = tokio::sync::mpsc::unbounded_channel::<LinkId>();
    let (b_closed_tx, mut b_closed_rx) =
        tokio::sync::mpsc::unbounded_channel::<(LinkId, LinkCloseReason)>();
    let b_drain = tokio::spawn(async move {
        while let Some(ev) = b_rx.recv().await {
            match ev {
                NodeEvent::LinkEstablished {
                    link_id,
                    is_initiator: true,
                    ..
                } => {
                    let _ = b_est_tx.send(link_id);
                }
                NodeEvent::LinkClosed {
                    link_id, reason, ..
                } => {
                    let _ = b_closed_tx.send((link_id, reason));
                }
                _ => {}
            }
        }
    });

    let fail =
        |note: &str, a_drain: tokio::task::JoinHandle<()>, b_drain: tokio::task::JoinHandle<()>| {
            a_drain.abort();
            b_drain.abort();
            RunOutcome {
                delivered: false,
                closer_self_notified: false,
                delivery_ms: None,
                note: note.to_string(),
            }
        };

    // A registers + announces its destination.
    let a_identity = Identity::generate(&mut rand_core::OsRng);
    let signing_key: [u8; 32] = a_identity.public_key_bytes()[32..64].try_into().unwrap();
    let a_dest = Destination::new(
        Some(a_identity),
        Direction::In,
        DestinationType::Single,
        "mvr",
        &["close77", "resp"],
    )
    .expect("A destination");
    let a_hash = *a_dest.hash();
    a.register_destination(a_dest);

    // Let the TCP peering settle, then announce.
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.announce_destination(&a_hash, Some(b"close77"))
        .await
        .expect("A announce");

    // B installs the path from the announce.
    let install_deadline = Instant::now() + Duration::from_secs(5);
    let mut path_ok = false;
    while Instant::now() < install_deadline {
        if b.has_path(&a_hash) {
            path_ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if !path_ok {
        return fail("B never installed path from A announce", a_drain, b_drain);
    }

    // B connects a link to A.
    let _handle = b.connect(&a_hash, &signing_key).await.expect("B connect");

    // Wait for B's initiator LinkEstablished.
    let b_link_id = match tokio::time::timeout(Duration::from_secs(5), b_est_rx.recv()).await {
        Ok(Some(id)) => id,
        _ => return fail("B link never established", a_drain, b_drain),
    };

    // Wait for A's responder LinkEstablished (responder link_id).
    let a_link_id = match tokio::time::timeout(Duration::from_secs(5), a_link_rx.recv()).await {
        Ok(Some(id)) => id,
        _ => return fail("A responder link never established", a_drain, b_drain),
    };

    // Brief settle so both ends are fully Active and no further handshake
    // traffic is in flight when we close.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A (responder) closes the link.
    let close_t = Instant::now();
    a.close_link(&a_link_id).await.expect("A close_link");

    // Tear A down per the mode. This races the queued close output against
    // the event-loop shutdown / runtime abort (the H3 seam). Hold A in an
    // Option so KeepAlive can stop it after B has waited.
    let mut a_opt = Some(a);
    match teardown {
        TeardownMode::StopImmediately => {
            let mut a = a_opt.take().unwrap();
            let _ = a.stop().await;
        }
        TeardownMode::DropImmediately => {
            drop(a_opt.take().unwrap());
        }
        TeardownMode::KeepAlive => {}
    }

    // A should self-notify (its own LinkClosed). Synchronous emit in
    // close_link, but it is delivered via the same dispatch the teardown
    // can pre-empt, so under StopImmediately it may be lost too.
    let closer_self_notified = matches!(
        tokio::time::timeout(Duration::from_secs(2), a_closed_rx.recv()).await,
        Ok(Some(id)) if id == a_link_id
    );

    // B waits (bounded) for its LinkClosed. Use a generous 3 s; loopback
    // delivery is sub-millisecond when it happens at all.
    let mut delivered = false;
    let mut delivery_ms = None;
    let wait_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_deadline {
        match tokio::time::timeout(Duration::from_millis(200), b_closed_rx.recv()).await {
            Ok(Some((id, _reason))) if id == b_link_id => {
                delivered = true;
                delivery_ms = Some(close_t.elapsed().as_millis());
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => continue,
        }
    }

    a_drain.abort();
    b_drain.abort();
    if let Some(mut a) = a_opt {
        let _ = a.stop().await;
    }
    let _ = b.stop().await;

    let _ = iter;
    RunOutcome {
        delivered,
        closer_self_notified,
        delivery_ms,
        note: String::new(),
    }
}

struct Tally {
    effective: usize,
    delivered: usize,
    self_notified: usize,
    setup_fail: usize,
}

/// Run N iterations in the given teardown mode, printing per-run +
/// summary lines and returning the tally.
async fn measure(label: &str, teardown: TeardownMode, n: usize) -> Tally {
    let mut delivered = 0usize;
    let mut self_notified = 0usize;
    let mut setup_fail = 0usize;

    for i in 0..n {
        let out = run_once_mode(i, teardown).await;
        if !out.note.is_empty() {
            setup_fail += 1;
            println!("CLOSE77_RUN[{label}] iter={i} SETUP_FAIL note={}", out.note);
            continue;
        }
        if out.delivered {
            delivered += 1;
        }
        if out.closer_self_notified {
            self_notified += 1;
        }
        println!(
            "CLOSE77_RUN[{label}] iter={i} delivered={} self_notified={} delivery_ms={}",
            out.delivered,
            out.closer_self_notified,
            out.delivery_ms
                .map(|m| m.to_string())
                .unwrap_or_else(|| "none".into()),
        );
    }

    let effective = n - setup_fail;
    println!(
        "CLOSE77_SUMMARY[{label}] n={n} setup_fail={setup_fail} effective={effective} \
         delivered={delivered} self_notified={self_notified} delivery_rate={:.2}",
        if effective > 0 {
            delivered as f64 / effective as f64
        } else {
            0.0
        }
    );
    Tally {
        effective,
        delivered,
        self_notified,
        setup_fail,
    }
}

/// CONTROL: responder stays alive while the initiator waits. The close is
/// always flushed, so delivery is 100%. Confirms the scaffold establishes
/// links and routes a graceful close correctly when nothing tears down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn responder_close_delivery_rate() {
    let n: usize = std::env::var("CLOSE77_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let t = measure("keepalive", TeardownMode::KeepAlive, n).await;

    assert_eq!(
        t.setup_fail, 0,
        "scaffold broke: {}/{n} runs failed setup before the close was tested",
        t.setup_fail
    );
    assert_eq!(
        t.delivered, t.effective,
        "control: with the responder kept alive, every graceful close should reach \
         the initiator ({}/{})",
        t.delivered, t.effective
    );
}

/// Codeberg #77 fix verification: responder `close_link`s then immediately
/// calls `stop()`. The queued close used to race the event-loop shutdown
/// branch + runtime abort and be dropped undispatched. With the bounded
/// graceful drain in the driver shutdown path (driver/mod.rs Branch 4 drains
/// action_dispatch_rx + flushes the interface outgoing queues before break),
/// every close now reaches the initiator over loopback TCP. Asserts 100%
/// delivery and that A always self-notifies (the LinkClosed riding in the same
/// TickOutput survives too).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn responder_close_then_teardown_delivery_rate() {
    let n: usize = std::env::var("CLOSE77_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);

    let t = measure("stop_immediately", TeardownMode::StopImmediately, n).await;

    assert_eq!(
        t.setup_fail, 0,
        "scaffold broke: {}/{n} runs failed setup before the close was tested",
        t.setup_fail
    );
    // A's own LinkClosed rides in the same TickOutput as the peer close, so the
    // graceful drain delivers both: self-notify must hold on every run.
    assert_eq!(
        t.self_notified, t.effective,
        "A should self-notify on every run (its LinkClosed rides in the drained \
         close TickOutput): {}/{}",
        t.self_notified, t.effective
    );
    assert_eq!(
        t.delivered, t.effective,
        "Codeberg #77 GREEN: responder-initiated close followed by an immediate \
         stop() must reach the initiator on every run now that the driver shutdown \
         path drains action_dispatch_rx and flushes the interface tasks before \
         abort ({}/{}).",
        t.delivered, t.effective
    );
}

/// Codeberg #77 fix verification, widest window: responder `close_link`s then
/// `drop`s its node. `Drop` used to call `runtime.shutdown_background()`
/// immediately, aborting the event loop before the queued close was ever
/// dispatched (~1/8 delivery). The fixed `Drop` first signals shutdown so the
/// event loop runs the same bounded drain+flush, then waits a bounded window
/// for the runner to finish before aborting the runtime. Asserts 100% delivery
/// and self-notify on every run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn responder_close_then_drop_delivery_rate() {
    let n: usize = std::env::var("CLOSE77_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);

    let t = measure("drop_immediately", TeardownMode::DropImmediately, n).await;

    assert_eq!(
        t.setup_fail, 0,
        "scaffold broke: {}/{n} runs failed setup before the close was tested",
        t.setup_fail
    );
    assert_eq!(
        t.self_notified, t.effective,
        "A should self-notify on every run (its LinkClosed rides in the drained \
         close TickOutput): {}/{}",
        t.self_notified, t.effective
    );
    assert_eq!(
        t.delivered, t.effective,
        "Codeberg #77 GREEN: responder-initiated close followed by dropping the node \
         must reach the initiator on every run now that Drop signals shutdown and \
         lets the event loop drain+flush the queued close before aborting the \
         runtime ({}/{}).",
        t.delivered, t.effective
    );
}
