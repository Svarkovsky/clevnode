//! mvr for Codeberg #44 — direct proof that `wait_for_path`'s bounded
//! PATH_REQUEST fallback installs a path that never arrives passively.
//!
//! Topology (one hop, direct):
//!
//! ```
//!   Daemon A                          Rust client
//!   (origin)                          (enable_transport=false)
//!   ──────────                        ──────────────────────
//!   peer dest registered              TCPClient → A
//!   (NEVER announced)                 has_path == false forever passively
//!                                     wait_for_path → PATH_REQUEST → A answers
//!                                     via path_request_handler (local dest,
//!                                     path_response) → path installed
//! ```
//!
//! The peer destination on daemon A is registered but **never
//! announced**, so no passive announce for it can ever reach the
//! client. A plain passive poll therefore never installs the path
//! (asserted as a control). The only mechanism that can install it is
//! the explicit PATH_REQUEST that `wait_for_path` issues once the
//! passive sub-window elapses: Python's `path_request_handler` finds
//! the destination local to daemon A and answers with a path-response
//! announce (`Transport.py:2939-2941`), which is not subject to the
//! `inbound()` announce-forward ingress hold. This is the code path the
//! #44 fix relies on, verified in isolation.

use std::time::{Duration, Instant};

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
async fn wait_for_path_installs_via_request_when_never_announced() {
    let daemon_a = TestDaemon::start().await.expect("start daemon A");

    // Register a destination on daemon A but do NOT announce it. No
    // passive announce for this destination will ever exist.
    let peer_info = daemon_a
        .register_destination("mvr", &["c44", "unannounced"])
        .await
        .expect("register peer dest on A");
    let peer_hash = parse_dest_hash(&peer_info.hash);

    // Rust leaf client connected directly to daemon A.
    let storage = tempfile::tempdir().expect("tempdir");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon_a.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust node");
    node.start().await.expect("start Rust node");

    // Let the TCP handshake settle.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Control: a purely passive wait can never install this path,
    // because the destination was never announced. Poll for 2 s.
    let control_deadline = Instant::now() + Duration::from_secs(2);
    let mut passive_installed = false;
    while Instant::now() < control_deadline {
        if node.has_path(&peer_hash) {
            passive_installed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !passive_installed,
        "control violated: path installed passively for a destination \
         that was never announced ({}). The fallback proof would be \
         meaningless.",
        hex::encode(peer_hash.as_bytes()),
    );

    // Now the real assertion: wait_for_path must install the path via
    // its PATH_REQUEST fallback. Short passive sub-window (500 ms) so
    // the request fires quickly; overall 10 s budget.
    let t_fallback = Instant::now();
    let installed = node
        .wait_for_path(
            &peer_hash,
            Duration::from_secs(10),
            Duration::from_millis(500),
        )
        .await
        .expect("wait_for_path");
    let fallback_ms = t_fallback.elapsed().as_millis();

    eprintln!("FALLBACK installed={installed} elapsed_ms={fallback_ms}");

    assert!(
        installed,
        "wait_for_path did not install a path for the unannounced \
         destination {} via its PATH_REQUEST fallback within 10 s.",
        hex::encode(peer_hash.as_bytes()),
    );
}
