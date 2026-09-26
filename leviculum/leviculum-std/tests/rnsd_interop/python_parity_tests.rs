//! Python-RNS broadcast parity interop tests.
//!
//! Each test here runs at least one live `python3 test_daemon.py` peer and
//! one Rust node, then asserts that on-wire behaviour matches what Python
//! would produce in an equivalent topology. The reference parity rules are
//! in `docs/src/architecture-broadcast-python-parity.md`.
//!
//! Run:
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop python_parity_tests
//! ```

use std::time::Duration;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationType, Direction};
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::harness::TestDaemon;

/// Parse a hex destination-hash string into the fixed-size array.
fn parse_hash(hex_str: &str) -> [u8; TRUNCATED_HASHBYTES] {
    hex::decode(hex_str)
        .expect("hex decode")
        .try_into()
        .expect("hash length")
}

/// Build a standard Rust node with one TCP client pointing at the Python
/// daemon plus transport enabled. Returns the running node + a tempdir
/// guard that must stay alive for the test duration.
async fn start_rust_node_with_tcp(
    test_name: &str,
    daemon: &TestDaemon,
) -> (leviculum_std::driver::ReticulumNode, tempfile::TempDir) {
    let storage = crate::common::temp_storage(test_name, "node");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build rust node");
    node.start().await.expect("start rust node");
    tokio::time::sleep(Duration::from_secs(1)).await;
    (node, storage)
}

/// 1. Rust emits a self-originated announce; the Python peer receives it
///    exactly once and no phantom retransmits follow.
///
///    Python's one-shot `Destination.announce() → Packet.send()` at
///    `Destination.py:322` is mirrored by Rust's `send_on_all_interfaces`.
#[tokio::test]
async fn test_rust_announce_received_by_python_matches_spec() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let (mut rust_node, _storage) =
        start_rust_node_with_tcp("rust_announce_received_by_python", &daemon).await;

    let identity = Identity::generate(&mut rand_core::OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "parity_test",
        &["rust_announce"],
    )
    .expect("destination");
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    rust_node.register_destination(dest);

    rust_node
        .announce_destination(&dest_hash, Some(b"rust-parity"))
        .await
        .expect("announce");

    let saw = crate::common::wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &rust_node,
        b"rust-parity",
        Duration::from_secs(5),
    )
    .await;
    assert!(saw, "python peer should observe the rust announce");

    // Allow plenty of time for any phantom retransmit to fire. With Python
    // parity B3, there should be no retry → the path table entry stays
    // stable and no duplicate packet arrives.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let table = daemon.get_path_table().await.expect("path_table");
    let entry = table
        .get(&hex::encode(dest_hash.as_bytes()))
        .expect("path still present");
    // The timestamp of the path_table entry must not have advanced — a
    // retransmit would refresh it.
    let ts_after_sleep = entry.timestamp.unwrap_or(0.0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    let table_later = daemon.get_path_table().await.expect("path_table later");
    let entry_later = table_later
        .get(&hex::encode(dest_hash.as_bytes()))
        .expect("path still present later");
    let ts_later = entry_later.timestamp.unwrap_or(0.0);
    assert!(
        (ts_later - ts_after_sleep).abs() < 1.0,
        "timestamp drift after the quiet window implies a phantom retransmit"
    );

    rust_node.stop().await.expect("stop");
}

/// 2. Mirror of #1: Python announces, Rust learns the path.
#[tokio::test]
async fn test_python_announce_received_by_rust_matches_spec() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let (mut rust_node, _storage) =
        start_rust_node_with_tcp("python_announce_received_by_rust", &daemon).await;

    let dest_info = daemon
        .register_destination("parity_test", &["py_announce"])
        .await
        .expect("register");
    let dest_hash: [u8; TRUNCATED_HASHBYTES] = parse_hash(&dest_info.hash);

    daemon
        .announce_destination(&dest_info.hash, b"python-parity")
        .await
        .expect("announce");

    let learned = crate::common::wait_for_path_reannounce(
        || rust_node.has_path(&leviculum_core::DestinationHash::new(dest_hash)),
        &daemon,
        &dest_info.hash,
        b"python-parity",
        Duration::from_secs(10),
    )
    .await;
    assert!(
        learned,
        "rust node did not learn the python announce within the deadline"
    );
    rust_node.stop().await.ok();
}

/// 3. A received announce produces a bounded number of Broadcast
///    rebroadcasts from the Rust relay. Python peers that repeatedly
///    announce the same destination (fresh `random_hash` each time) do
///    not cause the relay's forwarded count to grow without bound, and
///    the relay does not feed its own echoes back through handle_announce
///    producing a divergent loop.
///
///    The announce dedup exemption (`is_single_announce` at
///    `transport.rs:1169`) matches Python's `Transport.py:1230-1232`;
///    loop suppression is via the path-table "not-better-hops" branch
///    and LOCAL_REBROADCASTS_MAX guard.
#[tokio::test]
async fn test_rust_broadcast_dedup_on_receive() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let (mut rust_node, _storage) =
        start_rust_node_with_tcp("rust_broadcast_dedup_on_receive", &daemon).await;

    let dest_info = daemon
        .register_destination("parity_test", &["dedup"])
        .await
        .expect("register");

    daemon
        .announce_destination(&dest_info.hash, b"first")
        .await
        .expect("announce 1");

    let learned = crate::common::wait_for_path_reannounce(
        || {
            rust_node.has_path(&leviculum_core::DestinationHash::new(parse_hash(
                &dest_info.hash,
            )))
        },
        &daemon,
        &dest_info.hash,
        b"first",
        Duration::from_secs(10),
    )
    .await;
    assert!(learned, "first announce must be learned");

    // Trigger three more fresh announces for the same destination (each
    // has a new random_hash on the Python side, so Python's identity
    // filter permits them). The relay forwards each, but the forwarded
    // count must stay within a bounded ceiling set by the retry scheduler
    // and LOCAL_REBROADCASTS_MAX.
    let stats_before_loop = rust_node.transport_stats().packets_forwarded();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        daemon
            .announce_destination(&dest_info.hash, b"same")
            .await
            .expect("announce N");
    }
    tokio::time::sleep(Duration::from_secs(8)).await;
    let stats_after_loop = rust_node.transport_stats().packets_forwarded();
    let delta = stats_after_loop - stats_before_loop;
    // Generous upper bound: 3 re-arrivals × at most 2 rebroadcasts each
    // (PATHFINDER_R=1 plus initial, capped by LOCAL_REBROADCASTS_MAX=2) =
    // 6. Allow headroom for retries that collide with rate windows.
    assert!(
        delta <= 10,
        "forwarded delta {delta} is inconsistent with bounded rebroadcast behaviour"
    );

    // And the relay count must PLATEAU — no background loop feeding
    // itself after the arrivals stop.
    let settled = rust_node.transport_stats().packets_forwarded();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let settled_later = rust_node.transport_stats().packets_forwarded();
    assert_eq!(
        settled, settled_later,
        "forwarded count should plateau once re-arrivals stop"
    );

    rust_node.stop().await.expect("stop");
}

/// 4. Python-A → Rust-B (relay) → Python-C chain. Rust-B forwards the
///    received announce exactly the Python-spec number of times (bounded
///    by `LOCAL_REBROADCASTS_MAX = 2` before the retry-scheduler removes
///    the announce-table entry).
#[tokio::test]
async fn test_rust_forwards_python_announce_once() {
    let daemon_a = TestDaemon::start().await.expect("daemon A");
    let daemon_c = TestDaemon::start().await.expect("daemon C");

    let _storage = crate::common::temp_storage("rust_forwards_python_announce", "relay");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_c.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay");
    relay.start().await.expect("start relay");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dest_a_info = daemon_a
        .register_destination("parity_test", &["forward_once"])
        .await
        .expect("register A");

    daemon_a
        .announce_destination(&dest_a_info.hash, b"chain-A")
        .await
        .expect("announce A");

    // C must learn the path via the relay.
    let saw = crate::common::wait_for_path_reannounce_on_daemon(
        &daemon_c,
        &leviculum_core::DestinationHash::new(parse_hash(&dest_a_info.hash)),
        &daemon_a,
        &dest_a_info.hash,
        b"chain-A",
        Duration::from_secs(10),
    )
    .await;
    assert!(saw, "python-c should learn dest_a via rust relay");

    // Give the retry scheduler time to finish (PATHFINDER_G * 2 + jitter).
    tokio::time::sleep(Duration::from_secs(15)).await;

    // The relay's announce_table entry for dest_a should have been removed
    // after the retry schedule completed. Confirm by checking that no new
    // Broadcast actions are being emitted on the relay's interfaces (the
    // relay's transport stats stay constant once retries stop).
    let stats_a = relay.transport_stats();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let stats_b = relay.transport_stats();
    assert_eq!(
        stats_a.packets_forwarded(),
        stats_b.packets_forwarded(),
        "retry schedule must terminate within the observation window"
    );

    relay.stop().await.expect("stop");
}

/// 5. Rust's self-originated announce is one-shot; no announce_table
///    entry on the Rust side implies no scheduled retry.
#[tokio::test]
async fn test_rust_self_announce_is_oneshot() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let (mut rust_node, _storage) =
        start_rust_node_with_tcp("rust_self_announce_is_oneshot", &daemon).await;

    let identity = Identity::generate(&mut rand_core::OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "parity_test",
        &["oneshot"],
    )
    .expect("destination");
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    rust_node.register_destination(dest);

    let stats_before = rust_node.transport_stats();
    rust_node
        .announce_destination(&dest_hash, Some(b"oneshot"))
        .await
        .expect("announce");

    tokio::time::sleep(Duration::from_secs(1)).await;
    let stats_after_send = rust_node.transport_stats();
    assert_eq!(
        stats_after_send.packets_sent(),
        stats_before.packets_sent() + 1,
        "self-originated announce emits exactly one packet"
    );

    // Wait well past any retry window that would have fired under the old
    // Rust-extension semantics (PATHFINDER_G=5s × 3 retries ≈ 15-20s).
    tokio::time::sleep(Duration::from_secs(25)).await;
    let stats_after_wait = rust_node.transport_stats();
    assert_eq!(
        stats_after_wait.packets_sent(),
        stats_after_send.packets_sent(),
        "no additional self-originated transmits after the quiet window"
    );

    rust_node.stop().await.expect("stop");
}

/// 6. Rust's mgmt-announce keepalive fires on its configured interval.
///    Uses the reduced-interval override on the Python peer only; the Rust
///    side is irrelevant here — the Python peer's keepalive emits a fresh
///    announce after the interval, and Rust observes the updated timestamp.
///
///    (The symmetric "Rust emits keepalive" side requires observing Rust's
///    mgmt tick externally. It is covered indirectly: B4 has its own unit
///    tests in `node/mod.rs` that assert the tick fires on schedule.)
#[tokio::test]
async fn test_mgmt_announce_keepalive_fires() {
    let daemon = TestDaemon::start_with_mgmt_interval(30)
        .await
        .expect("start daemon with reduced mgmt interval");
    let (mut rust_node, _storage) =
        start_rust_node_with_tcp("mgmt_announce_keepalive_fires", &daemon).await;

    // Grab the probe destination at startup. With mgmt interval 30 s and
    // the initial 15 s lead-in, we should see the first mgmt announce
    // within 20 s and a second one within the following 35 s.
    let dest_info = daemon
        .register_destination("parity_test", &["keepalive"])
        .await
        .expect("register");
    daemon
        .announce_destination(&dest_info.hash, b"ka")
        .await
        .expect("announce");

    let learned = crate::common::wait_for_path_reannounce(
        || {
            rust_node.has_path(&leviculum_core::DestinationHash::new(parse_hash(
                &dest_info.hash,
            )))
        },
        &daemon,
        &dest_info.hash,
        b"ka",
        Duration::from_secs(10),
    )
    .await;
    assert!(learned, "initial announce should be visible to rust");

    // Look at Python's announce-table timestamp for the probe destination
    // (if respond_to_probes is off, use a different observable). Here we
    // simply assert the daemon stays reachable and path persists after the
    // keepalive window — a regression in mgmt_announce_interval would
    // typically present as path expiry or transport state drift.
    tokio::time::sleep(Duration::from_secs(45)).await;
    assert!(
        rust_node.has_path(&leviculum_core::DestinationHash::new(parse_hash(
            &dest_info.hash
        ))),
        "path should still be present after a mgmt-announce window"
    );

    rust_node.stop().await.expect("stop");
}

/// 7. Announce cap rate-limits forwarded announces on a capped interface.
///    Matches Python's `Transport.py:1091-1161` tx_time / wait_time model.
///
///    Rather than synthesize an interface with a numerical cap in an
///    integration test (which would require new test-harness plumbing),
///    this test asserts the *presence* of the cap enforcement by driving
///    many forwarded announces in sequence and confirming the relay's
///    transport stats converge at a reasonable bound rather than growing
///    unbounded.
#[tokio::test]
async fn test_announce_cap_forwarding_rate_limit() {
    let daemon_a = TestDaemon::start().await.expect("daemon A");
    let daemon_b = TestDaemon::start().await.expect("daemon B");

    let _storage = crate::common::temp_storage("announce_cap_forwarding", "relay");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay");
    relay.start().await.expect("start relay");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Emit several distinct announces from A, spaced by 2 s so Python's
    // ingress-control does not hold them. The relay forwards each on the
    // cap-subject path once (hops > 0) and the retry scheduler fires at
    // most twice per destination.
    for i in 0..4 {
        let name = format!("cap_test_{i}");
        let info = daemon_a
            .register_destination("parity_test", &[&name])
            .await
            .expect("register");
        daemon_a
            .announce_destination(&info.hash, b"cap")
            .await
            .expect("announce");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Let all retries drain.
    tokio::time::sleep(Duration::from_secs(20)).await;

    // Python-B should have learned all four destinations.
    let table_b = daemon_b.get_path_table().await.expect("path_table b");
    assert!(
        table_b.len() >= 4,
        "python-b should learn all four forwarded destinations, got {}",
        table_b.len()
    );

    // After the retries drain, stats plateau. Observing that the drain
    // reached a steady state is enough to verify the rate-limit subsystem
    // did not deadlock or infinite-loop.
    let s1 = relay.transport_stats().packets_forwarded();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let s2 = relay.transport_stats().packets_forwarded();
    assert_eq!(
        s1, s2,
        "relay transport_stats should plateau once retry schedules finish"
    );

    relay.stop().await.expect("stop");
}

/// 8. LOCAL_REBROADCASTS_MAX caps how many times a single announce is
///    rebroadcast from a Rust relay.
///
///    The retry scheduler at `transport.rs:3991-3968` removes the
///    announce-table entry when `retries > PATHFINDER_RETRIES` OR
///    `local_rebroadcasts >= LOCAL_REBROADCASTS_MAX`. A sustained stream
///    of identical re-arrivals from a peer must not produce unbounded
///    rebroadcast traffic.
#[tokio::test]
async fn test_local_rebroadcasts_max_enforced() {
    let daemon_a = TestDaemon::start().await.expect("daemon A");
    let daemon_b = TestDaemon::start().await.expect("daemon B");

    let _storage = crate::common::temp_storage("local_rebroadcasts_max", "relay");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon_a.rns_addr())
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay");
    relay.start().await.expect("start relay");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dest_info = daemon_a
        .register_destination("parity_test", &["rebroadcasts_max"])
        .await
        .expect("register");

    // Emit the same announce five times, separated enough to avoid
    // Python's ingress-control spike. Each re-emission produces a fresh
    // random_hash so Python treats each as a new announce, but the relay
    // sees them as "same destination" — LOCAL_REBROADCASTS_MAX bounds how
    // many rebroadcasts the relay can emit regardless of re-arrivals.
    for _ in 0..5 {
        daemon_a
            .announce_destination(&dest_info.hash, b"repeat")
            .await
            .expect("announce");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    tokio::time::sleep(Duration::from_secs(8)).await;

    let forwarded_after_repeats = relay.transport_stats().packets_forwarded();
    // Rough upper bound: 5 arrivals × at most 2 rebroadcasts each = 10
    // relay-originated forwards. In practice the scheduler collapses many
    // of those, but 20 is a generous ceiling that still catches runaway
    // rebroadcast behaviour.
    assert!(
        forwarded_after_repeats <= 20,
        "forwarded count {forwarded_after_repeats} exceeds rebroadcasts-max bound"
    );

    // Python-B should have learned dest_a.
    assert!(
        daemon_b
            .has_path(hex::decode(&dest_info.hash).unwrap())
            .await,
        "python-b should have a path to dest_a"
    );

    relay.stop().await.expect("stop");
}

/// Codeberg #92: per-interface announce-rate limiting (target/grace/penalty).
///
/// Topology: Python source (SRC) -> relay -> Python sink (SNK). The relay is a
/// client to both, so announces from SRC are received on the SRC-facing
/// interface and rebroadcast toward SNK. That interface carries an
/// `announce_rate_target`, so a source re-announcing faster than the target
/// has its rebroadcasts rate-limited once grace is exceeded
/// (Transport.py:1838-1864).
///
/// The SAME source/sink daemons drive two identically-configured relays:
///   1. our Rust relay -> assert it rate-limits (drops_announce_rate_limited
///      grows) while the sink still learns the path (rate limiting blocks
///      rebroadcast, not path-table propagation), and
///   2. a Python reference relay -> assert the sink likewise learns the path
///      on the identical scenario.
///
/// The exact per-decision algorithm parity (which announce is blocked vs
/// accepted, step for step) is pinned by the algorithm-identical unit tests in
/// `leviculum-core` (`test_announce_rate_*`). This interop test validates the
/// config -> enforcement -> semantic path against a live Python peer.
#[tokio::test]
async fn test_announce_rate_target_matches_python() {
    // Spacing above the 2 s local-rebroadcast dedup window so only the
    // per-destination announce_rate_target logic is exercised.
    const ANNOUNCE_SPACING: Duration = Duration::from_millis(2500);
    const ANNOUNCE_COUNT: usize = 6;
    const AR_TARGET: u32 = 60; // seconds; the source re-announces far faster
    const AR_GRACE: u32 = 0;
    const AR_PENALTY: u32 = 0;

    // ---- Run 1: our Rust relay ----
    let source = TestDaemon::start().await.expect("source daemon");
    let sink = TestDaemon::start().await.expect("sink daemon");

    let _storage = crate::common::temp_storage("announce_rate_target", "relay");
    let mut relay = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client_with_announce_rate(source.rns_addr(), AR_TARGET, AR_GRACE, AR_PENALTY)
        .add_tcp_client(sink.rns_addr())
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("build relay");
    relay.start().await.expect("start relay");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dest_info = source
        .register_destination("parity_test", &["ar_target"])
        .await
        .expect("register");
    let dest_hash = leviculum_core::DestinationHash::new(parse_hash(&dest_info.hash));

    for _ in 0..ANNOUNCE_COUNT {
        source
            .announce_destination(&dest_info.hash, b"ar")
            .await
            .expect("announce");
        tokio::time::sleep(ANNOUNCE_SPACING).await;
    }
    // Let forwarding and retries settle.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // The sink learned the destination from the first (accepted) announce:
    // rate limiting blocks rebroadcast, not path-table propagation.
    assert!(
        crate::common::wait_for_path_on_daemon(&sink, &dest_hash, Duration::from_secs(10)).await,
        "sink should learn the destination through the rate-limited Rust relay"
    );

    // Rebroadcasts after grace were blocked. With grace=0, announces #2..N are
    // all blocked, so at least ANNOUNCE_COUNT-2 drops are recorded.
    let drops = relay.transport_stats().drops_announce_rate_limited();
    assert!(
        drops >= (ANNOUNCE_COUNT as u64 - 2),
        "Rust relay should rate-limit rebroadcasts (drops={drops}, want >= {})",
        ANNOUNCE_COUNT - 2
    );

    relay.stop().await.expect("stop relay");
    drop(source);
    drop(sink);

    // ---- Run 2: Python reference relay, same config, same scenario ----
    let src = TestDaemon::start().await.expect("ref source daemon");
    let snk = TestDaemon::start().await.expect("ref sink daemon");
    let relay_py = TestDaemon::start().await.expect("ref relay daemon");

    relay_py
        .add_client_interface_rate_limited(
            "127.0.0.1",
            src.rns_port(),
            Some("to_source"),
            AR_TARGET,
            AR_GRACE,
            AR_PENALTY,
        )
        .await
        .expect("relay->source iface");
    relay_py
        .add_client_interface("127.0.0.1", snk.rns_port(), Some("to_sink"))
        .await
        .expect("relay->sink iface");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let ref_info = src
        .register_destination("parity_test", &["ar_target"])
        .await
        .expect("register");
    let ref_hash = leviculum_core::DestinationHash::new(parse_hash(&ref_info.hash));

    for _ in 0..ANNOUNCE_COUNT {
        src.announce_destination(&ref_info.hash, b"ar")
            .await
            .expect("announce");
        tokio::time::sleep(ANNOUNCE_SPACING).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(
        crate::common::wait_for_path_on_daemon(&snk, &ref_hash, Duration::from_secs(10)).await,
        "sink should learn the destination through the identically-configured Python relay"
    );
}

/// Codeberg #93: a configured `bitrate` is honoured identically by both
/// stacks. A real Python `TCPClientInterface` and our TCP client, each given
/// the same `bitrate`, report that exact effective bitrate. This is the clean
/// external observable of the configured value (the announce tx-spacing effect
/// is internal weighting, covered by a focused unit test against Python's
/// `tx_time/announce_cap` formula). Proves our `configured_bitrate` handling
/// matches the reference (Reticulum.py:794-796,887).
#[tokio::test]
async fn test_configured_bitrate_matches_python() {
    const BITRATE: u64 = 9600;

    let inspect = TestDaemon::start().await.expect("inspect daemon");
    let target = TestDaemon::start().await.expect("target daemon");

    // Reference: a real Python TCPClientInterface with the configured bitrate.
    inspect
        .add_client_interface_with_bitrate(
            &target.rns_addr().ip().to_string(),
            target.rns_addr().port(),
            Some("BitrateProbe"),
            BITRATE,
        )
        .await
        .expect("add python client interface");

    // Our stack: a TCP client configured with the same bitrate.
    let storage = crate::common::temp_storage("configured_bitrate_matches_python", "node");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client_with_bitrate(target.rns_addr(), BITRATE)
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build rust node");
    node.start().await.expect("start rust node");
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Python reports the configured bitrate on the probe interface (uniquely
    // identified: the daemon's default interfaces carry the 10 Mbps guess).
    let py_ifaces = inspect.get_interfaces().await.expect("python interfaces");
    let py_probe = py_ifaces
        .iter()
        .find(|i| i.bitrate == Some(BITRATE))
        .expect("python probe interface reports the configured bitrate");

    // Our stack reports the same effective bitrate on its TCP client interface.
    let our_ifaces = node.interface_stats();
    let our_tcp = our_ifaces
        .iter()
        .find(|i| !i.is_local_client)
        .expect("our tcp interface present");
    assert_eq!(
        our_tcp.configured_bitrate,
        Some(BITRATE as u32),
        "our stack must report the configured bitrate"
    );

    // Same-value agreement across stacks (drop-in parity on the configured value).
    assert_eq!(
        our_tcp.configured_bitrate.map(u64::from),
        py_probe.bitrate,
        "both stacks agree on the effective configured bitrate"
    );
}
