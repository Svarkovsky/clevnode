//! mvr for Bug #25 R1 — Rust transport-disabled client fails to install
//! paths from announces forwarded by a Python-RNS rnsd.
//!
//! Named mechanism from the n=20 A/B/A report: in the
//! `lora_link_python` scenario, `lnstest selftest` constructs two leaf
//! clients with `enable_transport(false)` connected over
//! `TCPClientInterface` to Python rnsd daemons. The clients receive
//! Announces for remote destinations (visible as `[PKT_RX]
//! type=Announce` in the daemon stderr), but their local
//! `Transport::path_table` stays empty for the full 300 s discovery
//! budget and the selftest fails with "Phase 2 timeout".
//!
//! The same selftest client talking to a Rust lnsd installs the path
//! correctly (Interop pass rate 85 % vs Python-Python 55 %). The
//! defect is therefore in the announce-handling path on the Rust
//! side, triggered by how Python forwards announces over TCP when
//! the receiving Rust node has `enable_transport = false`.
//!
//! This mvr reproduces the failure without LoRa, Docker, or the
//! `lnstest selftest` tool — just a Python `rnsd` from
//! `reference/Reticulum` (spawned via the existing
//! `rnsd_interop::TestDaemon`) plus one Rust `ReticulumNode` built
//! with `enable_transport(false)` mirroring the selftest client's
//! construction.
//!
//! **Acceptance**: the test is red on master HEAD (no path installed
//! within 5 s of the Python announce), green once the path-install
//! code path is fixed.

use std::time::Duration;

use leviculum_core::DestinationHash;
use leviculum_std::driver::ReticulumNodeBuilder;

const TRUNCATED_HASHBYTES: usize = 16;

// Inline minimal copies of helpers from rnsd_interop/common.rs to keep
// this mvr self-contained — #[path] imports of common.rs pull in a
// web of unrelated modules (harness, test_daemon, link helpers) that
// doesn't resolve cleanly from under `tests/mvr/`.
use crate::harness::TestDaemon;

fn parse_dest_hash(hex_str: &str) -> DestinationHash {
    let bytes: [u8; TRUNCATED_HASHBYTES] = hex::decode(hex_str)
        .expect("hex decode")
        .try_into()
        .expect("correct hash length");
    DestinationHash::new(bytes)
}

#[tokio::test]
async fn rust_client_installs_path_from_python_announce() {
    let daemon = TestDaemon::start().await.expect("start daemon");

    let dest_info = daemon
        .register_destination("mvr", &["r1", "pathinstall"])
        .await
        .expect("register destination");

    let dest_hash = parse_dest_hash(&dest_info.hash);

    // Build the Rust client with enable_transport(false), exactly as
    // `lnstest selftest` does at `leviculum-cli/src/selftest.rs:620`.
    let storage = tempfile::tempdir().expect("tempdir");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build node");

    node.start().await.expect("start node");

    // Give the TCP client time to connect and handshake before the
    // Python announce fires. 500 ms is generous for localhost TCP.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Python announces its registered destination over the shared-
    // instance TCP path — the Rust client should see it, validate
    // it, and install a path_table entry.
    daemon
        .announce_destination(&dest_info.hash, b"mvr-r1-path-install")
        .await
        .expect("python announce");

    // Wait up to 5 s for the path to install. Localhost TCP with a
    // fresh pair of nodes should install the path within milliseconds
    // of the announce; 5 s is generous.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
        "Rust client with enable_transport(false) did not install a path \
         for the Python-announced destination {} within 5 s. \
         See ~/.claude/report.md (2026-04-18 A/B/A n=20 report). \
         Run with `RUST_LOG=leviculum_core::transport=debug` to see the \
         dispatch: `[PKT_RX]` arrives but `[ANN_RX]` does not, so \
         `handle_announce` is either short-circuited early or never \
         reached on this config combination.",
        hex::encode(dest_hash.as_bytes()),
    );
}
