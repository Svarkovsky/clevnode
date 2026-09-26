//! mvr for Bug #25 R1 Task C.1 — own-echo + peer-announce interaction.
//!
//! The two prior mvrs in this family both pass on master:
//!
//! - `rust_client_path_install_from_python.rs` (hops=1 direct) — green.
//! - `rust_client_path_install_via_relay.rs` (hops=2 via two daemons) — green.
//!
//! This mvr adds the next missing constraint from the n=20 field
//! failure: the Rust client registers AND announces its own
//! destination. The field-failure log shows the selftest client
//! receiving Announces for BOTH its OWN destination (`319942b3`,
//! round-tripped through the mesh) AND the peer's destination
//! (`46fcf94a`). If path-install for the peer interacts with
//! own-echo suppression state in some hops ≥ 2-only way, this mvr
//! will surface it.
//!
//! Topology (extends the relay mvr):
//!
//! ```
//!   Daemon A                Daemon B               Rust client
//!   (origin)                (relay)                (enable_transport=false)
//!   ──────────              ──────────             ──────────────────────
//!   peer dest registered    TCPClient → A          TCPClient → B
//!   announce peer ────────► forwards  ─────────►   receives peer @ hops=2
//!                                                  (plus own-dest registered)
//!                                                  own announce → B → A → B
//!                                                  own-echo returns @ hops=2
//! ```
//!
//! Expected: the Rust client installs a path for daemon A's
//! destination AND correctly ignores its own-echo.

use std::time::{Duration, Instant};

use leviculum_core::{Destination, DestinationHash, DestinationType, Direction, Identity};
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::harness::TestDaemon;

const TRUNCATED_HASHBYTES: usize = 16;

fn parse_dest_hash(hex_str: &str) -> DestinationHash {
    let bytes: [u8; TRUNCATED_HASHBYTES] = hex::decode(hex_str)
        .expect("hex decode")
        .try_into()
        .expect("correct hash length");
    DestinationHash::new(bytes)
}

/// Formerly `#[ignore]`: ~25 % of runs failed because the Python
/// relay (daemon B) held A's forwarded peer announce under ingress
/// limiting. When B's daemon-to-daemon peer interface is younger
/// than `IC_NEW_TIME` it uses the stricter `IC_BURST_FREQ_NEW`
/// rate, and if the client own-echo and the upstream peer announce
/// arrive within one burst window on loopback TCP, Python
/// `Transport.inbound()` routes the peer announce through
/// `hold_announce()`. The held announce is only released after
/// `IC_BURST_HOLD` (tens of seconds), well past the client 10 s path
/// budget (Codeberg #44). The Rust side was provably innocent:
/// `packets_received` did not tick for the held announce.
///
/// The fix is client-side: the path-wait now falls back to an
/// explicit PATH_REQUEST when the announce does not arrive
/// passively (see `ReticulumNode::wait_for_path`). Python answers a
/// PATH_REQUEST over its path-response code path, which is not
/// subject to the `inbound()` announce-forward hold, so the path is
/// delivered inside the budget. This test now passes reliably.
///
/// Note: the current vendored Python (`IC_BURST_HOLD`,
/// `IC_BURST_MIN_SAMPLES=6`) only rarely activates the burst hold on
/// the two announces this two-node topology emits, so on most runs
/// the path installs passively and the fallback is not exercised
/// here. The fallback itself is proven directly, in isolation, by
/// `client_wait_for_path_request_fallback`.
///
/// To run manually:
///   cargo test -p leviculum-std --test mvr \
///     rust_client_path_install_with_own_echo \
///     -- --test-threads=1
#[tokio::test]
async fn rust_client_installs_peer_path_while_own_echoes() {
    let t0 = Instant::now();
    let mut cps: Vec<(&'static str, Duration)> = Vec::with_capacity(16);
    cps.push(("start", t0.elapsed()));

    let daemon_a = TestDaemon::start().await.expect("start daemon A");
    cps.push(("daemon_a_ready", t0.elapsed()));
    let daemon_b = TestDaemon::start().await.expect("start daemon B");
    cps.push(("daemon_b_ready", t0.elapsed()));
    daemon_b
        .add_client_interface("127.0.0.1", daemon_a.rns_port(), Some("relay_to_A"))
        .await
        .expect("wire B → A");
    cps.push(("daemon_b_peered_to_a", t0.elapsed()));

    tokio::time::sleep(Duration::from_millis(500)).await;
    cps.push(("after_peer_settle_500ms", t0.elapsed()));

    // Peer destination on daemon A (what the Rust client needs to
    // learn).
    let peer_info = daemon_a
        .register_destination("mvr", &["r1c1", "peer"])
        .await
        .expect("register peer dest on A");
    let peer_hash = parse_dest_hash(&peer_info.hash);
    cps.push(("peer_registered", t0.elapsed()));

    // Rust client.
    let storage = tempfile::tempdir().expect("tempdir");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust node");
    node.start().await.expect("start Rust node");
    cps.push(("client_started", t0.elapsed()));

    // Register an own-destination on the Rust client. Use a fresh
    // identity so the daemon network treats it as a leaf dest.
    let own_identity = Identity::generate(&mut rand_core::OsRng);
    let own_dest = Destination::new(
        Some(own_identity),
        Direction::In,
        DestinationType::Single,
        "mvr",
        &["r1c1", "own"],
    )
    .expect("create own dest");
    let own_hash = *own_dest.hash();
    node.register_destination(own_dest);
    cps.push(("own_registered", t0.elapsed()));

    // Let the Rust client's TCP handshake complete.
    tokio::time::sleep(Duration::from_millis(500)).await;
    cps.push(("after_client_settle_500ms", t0.elapsed()));

    // Fire own announce FIRST — so by the time daemon A's peer
    // announce arrives, the Rust client has already processed its
    // own announce round-tripping through the mesh.
    node.announce_destination(&own_hash, Some(b"mvr-r1c1-own"))
        .await
        .expect("own announce");
    cps.push(("own_announce_emitted", t0.elapsed()));

    // Give own-announce time to round-trip through B → A → B and
    // come back as an echo. 2 s is generous for localhost TCP.
    tokio::time::sleep(Duration::from_secs(2)).await;
    cps.push(("after_own_echo_2s", t0.elapsed()));

    // Snapshot transport stats right before the peer announce. On
    // the failing path, the comparison between this snapshot and
    // the wait_for_path_end snapshot tells us whether the announce
    // ever reached the Rust client's process_incoming at all.
    let stats_pre_peer = node.transport_stats();
    let paths_pre_peer = node.path_count();
    cps.push(("stats_pre_peer", t0.elapsed()));

    // Now the peer's announce from daemon A.
    daemon_a
        .announce_destination(&peer_info.hash, b"mvr-r1c1-peer")
        .await
        .expect("peer announce from A");
    cps.push(("peer_announce_emitted", t0.elapsed()));

    // Wait up to 10 s for the Rust client to install the peer path.
    // `wait_for_path` waits passively first and only falls back to an
    // explicit PATH_REQUEST (every 1 s) if the announce is held upstream
    // (Codeberg #44), so the healthy path is unchanged.
    cps.push(("wait_for_path_start", t0.elapsed()));
    let installed = node
        .wait_for_path(&peer_hash, Duration::from_secs(10), Duration::from_secs(1))
        .await
        .expect("wait_for_path");
    cps.push((
        if installed {
            "wait_for_path_end_installed"
        } else {
            "wait_for_path_end_timeout"
        },
        t0.elapsed(),
    ));

    // Snapshot at end only, NOT per-poll. Per-poll stats reads
    // tripled the lock-contention rate and masked the race
    // (3/3 pass in a perturbation check).
    let stats_end = node.transport_stats();
    let paths_end = node.path_count();

    // Dump checkpoints to stderr so post-hoc grep/awk can parse
    // them. Uses eprintln so it's visible under --nocapture.
    for (name, d) in &cps {
        eprintln!("CHECKPOINT STEP={name} elapsed={}", d.as_millis());
    }
    eprintln!(
        "STATS pre_peer pkt_rx={} ann_proc={} pkt_drop={} paths={}",
        stats_pre_peer.packets_received(),
        stats_pre_peer.announces_processed(),
        stats_pre_peer.packets_dropped(),
        paths_pre_peer
    );
    eprintln!(
        "STATS end      pkt_rx={} ann_proc={} pkt_drop={} paths={}",
        stats_end.packets_received(),
        stats_end.announces_processed(),
        stats_end.packets_dropped(),
        paths_end
    );
    eprintln!("INSTALLED={installed}");

    assert!(
        installed,
        "Rust client did not install path for daemon A's peer \
         destination {} after hops=2 relay, in the presence of its \
         own-echo traffic.\n\n\
         See ~/.claude/report.md (R1 stop-and-report series).",
        hex::encode(peer_hash.as_bytes()),
    );
}
