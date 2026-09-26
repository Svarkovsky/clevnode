//! mvr for Bug #25 R1.a — hops=2 two-daemon forwarding chain.
//!
//! The previous batch's R1.a baseline mvr
//! (`rust_client_path_install_from_python.rs`) proved that a one-
//! Python-daemon topology installs paths correctly under
//! `enable_transport(false)`: the mechanism named in the n=20 A/B/A
//! report ("Rust transport-disabled client drops Python-forwarded
//! Announces before path-install") is too broad. This mvr adds the
//! most likely missing constraint from the field failure log at
//! `/tmp/bug25-aba/python/run-13/`: **hops = 2** via a two-Python-
//! daemon forwarding chain.
//!
//! Topology:
//!
//! ```
//!     Daemon A (origin)    Daemon B (relay)       Rust client
//!     ───────────────     ─────────────────      ──────────────
//!     register dest ────► TCPClientInterface ───► TCPClientInterface
//!                          to A (hops 0→1)         (enable_transport=false)
//!     announce (hops=0) ────────────────────►     (expects hops=2)
//! ```
//!
//! If this mvr goes red on master, the bug is hop-count-dependent
//! and the fix target is inside `handle_announce`'s `should_update`
//! block in `leviculum-core/src/transport.rs` (lines 1706–1744).
//!
//! If this mvr is ALSO green, escalate to Task C in the instructions
//! (own-echo interaction, then loop/race harness).

use std::time::Duration;

use leviculum_core::DestinationHash;
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

#[tokio::test]
async fn rust_client_installs_path_via_relay_hops2() {
    // Daemon A: origin of the announce. Stays the same rnsd config as
    // the baseline mvr — a fresh TCPServerInterface on an allocated
    // port.
    let daemon_a = TestDaemon::start().await.expect("start daemon A");

    // Daemon B: will relay announces from A to the Rust client.
    // Spawn second, then wire its TCPClientInterface to daemon A via
    // the existing harness `add_client_interface` JSON-RPC method
    // (see `leviculum-std/tests/rnsd_interop/harness.rs:1232`).
    let daemon_b = TestDaemon::start().await.expect("start daemon B");
    daemon_b
        .add_client_interface("127.0.0.1", daemon_a.rns_port(), Some("relay_to_A"))
        .await
        .expect("wire daemon B → daemon A");

    // Let the TCP peering settle. `TestDaemon::start_chain` at
    // harness.rs:1918 uses a 500 ms sleep after the wire-up for the
    // same reason.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Register destination on DAEMON A — announces for this hash
    // originate AT daemon A and propagate out via its
    // TCPServerInterface (where daemon B is a client).
    let dest_info = daemon_a
        .register_destination("mvr", &["r1a", "relay"])
        .await
        .expect("register destination on A");

    let dest_hash = parse_dest_hash(&dest_info.hash);

    // Rust client: transport disabled, one TCPClientInterface
    // pointing at DAEMON B (not A). This forces the announce to take
    // the two-hop path A → B → Rust.
    let storage = tempfile::tempdir().expect("tempdir");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust node");
    node.start().await.expect("start Rust node");

    // Let the Rust client's TCP handshake complete before the
    // announce fires.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Announce from daemon A. Expected propagation:
    //   A: hops=0 (origin) → TCPServerInterface out
    //   B: receives on its TCPClientInterface to A at hops=1
    //      → rebroadcasts on its own TCPServerInterface
    //   Rust client: receives at hops=2 via its TCPClientInterface to B
    daemon_a
        .announce_destination(&dest_info.hash, b"mvr-r1a-relay")
        .await
        .expect("announce from daemon A");

    // Wait up to 10 s (double the baseline mvr's 5 s) for the
    // two-hop propagation to complete. Localhost TCP is still very
    // fast; 10 s is generous.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut installed = false;
    while tokio::time::Instant::now() < deadline {
        if node.has_path(&dest_hash) {
            installed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        installed,
        "Rust client (enable_transport=false) did not install a path \
         for daemon-A-originated destination {} after relay through \
         daemon B within 10 s. Expected hops=2 at the Rust client.\n\n\
         See ~/.claude/report.md (2026-04-18 R1 stop-and-report) for \
         field context. Run with \
         `RUST_LOG=leviculum_core::transport=debug` to inspect the \
         Rust client's announce dispatch — `[PKT_RX] type=Announce \
         hops=2` should appear but `[PATH_ADD]` should not.",
        hex::encode(dest_hash.as_bytes()),
    );
}
