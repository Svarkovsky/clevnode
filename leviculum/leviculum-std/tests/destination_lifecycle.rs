//! Destination-lifecycle conformance — leviculum#54.
//!
//! Executable contract for `ReticulumNode::unregister_destination`, written
//! before the verb existed and kept as the permanent proof of its semantics.
//! Filed from CIRISEdge#499, where a scoped address is derived per MLS
//! `(group, epoch)` and rotated make-before-break: the **seal** phase must
//! mean "this address is no longer ours", or the unlinkability window the
//! rotation exists to close never closes.
//!
//! Three questions, each answered by measurement rather than by assertion in
//! a doc comment:
//!
//! - **D1 — sealing actually seals.** A peer that already learned the address
//!   (it holds a path; the announce is long gone) dials it again after
//!   deregistration and gets no endpoint. This is the disclosure edge is
//!   removing: an observer who keeps probing a retired address must stop
//!   getting a live answer.
//! - **D2 — idempotence.** Edge's seal is timing-driven and may fire twice;
//!   a second deregistration, and one for a hash never registered, are
//!   no-ops, and the node keeps serving its other destinations afterwards.
//! - **D3 — established links survive.** Rotation is deliberately
//!   non-disruptive to live traffic: a `DestinationHash` is used to dial and
//!   to listen, never per packet, and an established link is keyed by
//!   `LinkId`. Deregistering the destination must not tear down a link
//!   already carrying data.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use leviculum_core::{Destination, DestinationType, Direction, Identity};
use leviculum_std::driver::{ReticulumNode, ReticulumNodeBuilder};
use leviculum_std::{EventReceiver, NodeEvent};

/// Port band disjoint from the bench (61000+) and scoped-transit (59000+).
static PORT_COUNTER: AtomicU16 = AtomicU16::new(58000);

fn next_port() -> u16 {
    loop {
        let candidate = PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
        if candidate >= 58900 {
            PORT_COUNTER.store(58000, Ordering::Relaxed);
            continue;
        }
        if StdTcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return candidate;
        }
    }
}

struct TestNode {
    node: ReticulumNode,
    rx: EventReceiver,
}

async fn start(builder: ReticulumNodeBuilder) -> TestNode {
    let storage = tempfile::tempdir().expect("tempdir");
    let mut node = builder
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build node");
    std::mem::forget(storage);
    node.start().await.expect("start node");
    let rx = node.take_event_receiver().expect("event rx");
    TestNode { node, rx }
}

/// The listener that owns the destination under test. Announces flow
/// server → client here, and a dial-out interface does not ingress-limit, so
/// this needs no burst-limiter carve-out — unlike a harness whose *listener*
/// receives the announces.
async fn server(port: u16) -> TestNode {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    start(
        ReticulumNodeBuilder::new()
            .enable_transport(false)
            .add_tcp_server(addr),
    )
    .await
}

async fn client(port: u16) -> TestNode {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    start(
        ReticulumNodeBuilder::new()
            .enable_transport(false)
            .add_tcp_client(addr),
    )
    .await
}

fn make_destination(aspect: &str) -> (Destination, [u8; 32], leviculum_core::DestinationHash) {
    let identity = Identity::generate(&mut rand_core::OsRng);
    let signing_key: [u8; 32] = identity.public_key_bytes()[32..64].try_into().unwrap();
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "lifecycle",
        &[aspect],
    )
    .expect("destination");
    let hash = *dest.hash();
    (dest, signing_key, hash)
}

async fn saw_event(
    rx: &mut EventReceiver,
    window: Duration,
    mut pred: impl FnMut(&NodeEvent) -> bool,
) -> bool {
    let dl = tokio::time::Instant::now() + window;
    loop {
        match tokio::time::timeout_at(dl, rx.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return true;
                }
            }
            Ok(None) | Err(_) => return false,
        }
    }
}

fn announce_of(hash: leviculum_core::DestinationHash) -> impl FnMut(&NodeEvent) -> bool {
    move |ev| {
        matches!(ev, NodeEvent::AnnounceReceived { announce, .. }
            if *announce.destination_hash() == *hash.as_bytes())
    }
}

/// Publish `dest` on `srv` and wait until `cli` holds a path to it.
async fn publish_and_learn(
    srv: &TestNode,
    cli: &mut TestNode,
    dest: Destination,
    hash: leviculum_core::DestinationHash,
) {
    srv.node.register_destination(dest);
    srv.node
        .announce_destination(&hash, Some(b"lifecycle"))
        .await
        .expect("announce");
    assert!(
        saw_event(&mut cli.rx, Duration::from_secs(8), announce_of(hash)).await,
        "the client must learn the destination before the test can retire it"
    );
}

/// D1 — a sealed address stops answering, even to a peer that already has
/// the path (which is exactly the observer the rotation is defending against).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deregistered_destination_stops_accepting_new_links() {
    let port = next_port();
    let srv = server(port).await;
    let mut cli = client(port).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let (dest, sk, hash) = make_destination("d1");
    publish_and_learn(&srv, &mut cli, dest, hash).await;

    // Baseline: while registered, the address answers.
    let live = cli.node.connect(&hash, &sk).await.expect("dial");
    cli.node
        .await_link_established(live.link_id())
        .await
        .expect("a registered destination must accept a link");

    // Seal.
    srv.node.unregister_destination(&hash);

    // The peer still holds the path — nothing revoked its knowledge — so it
    // can still dial. It must simply find nobody home.
    assert!(
        cli.node.has_path(&hash),
        "precondition: the observer still knows the address (only the endpoint retired)"
    );
    let probe = cli.node.connect(&hash, &sk).await.expect("dial after seal");
    let outcome = tokio::time::timeout(
        Duration::from_secs(4),
        cli.node.await_link_established(probe.link_id()),
    )
    .await;
    assert!(
        outcome.is_err(),
        "a sealed address must not establish a new link, got {outcome:?}"
    );
}

/// D2 — idempotent, and harmless for a hash that was never registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deregistration_is_idempotent_and_an_unknown_hash_is_a_noop() {
    let port = next_port();
    let srv = server(port).await;
    let mut cli = client(port).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let (dest, _sk, hash) = make_destination("d2");
    publish_and_learn(&srv, &mut cli, dest, hash).await;

    srv.node.unregister_destination(&hash);
    // Second seal (edge's timing-driven path may fire twice) and a hash that
    // was never ours: both must be no-ops, not panics.
    srv.node.unregister_destination(&hash);
    let (_other, _osk, never_registered) = make_destination("d2-never");
    srv.node.unregister_destination(&never_registered);

    // The node is still a working node afterwards: a different destination
    // registers, announces, and is reachable.
    let (keep, keep_sk, keep_hash) = make_destination("d2-keep");
    publish_and_learn(&srv, &mut cli, keep, keep_hash).await;
    let handle = cli.node.connect(&keep_hash, &keep_sk).await.expect("dial");
    cli.node
        .await_link_established(handle.link_id())
        .await
        .expect("deregistrations must not disturb the node's other destinations");
}

/// D3 — an established link keeps working after its destination is retired.
/// Rotation must not cut live traffic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_established_link_survives_deregistration() {
    let port = next_port();
    let mut srv = server(port).await;
    let mut cli = client(port).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    let (dest, sk, hash) = make_destination("d3");
    publish_and_learn(&srv, &mut cli, dest, hash).await;

    let handle = cli.node.connect(&hash, &sk).await.expect("dial");
    cli.node
        .await_link_established(handle.link_id())
        .await
        .expect("establish before sealing");

    // Seal the address out from under the live link.
    srv.node.unregister_destination(&hash);

    handle
        .try_send(b"still-flowing-after-seal")
        .await
        .expect("send on the established link");
    assert!(
        saw_event(&mut srv.rx, Duration::from_secs(8), |ev| matches!(
            ev,
            NodeEvent::MessageReceived { .. } | NodeEvent::LinkDataReceived { .. }
        ))
        .await,
        "an established link is keyed by LinkId and must survive its destination's retirement"
    );
}
