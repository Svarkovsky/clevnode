//! mvr for Bug #25 R1 Task C.2 — loop harness, race-condition probe.
//!
//! Prior mvrs all pass on master:
//!
//! - `rust_client_path_install_from_python.rs` (hops=1 direct).
//! - `rust_client_path_install_via_relay.rs` (hops=2 relay).
//! - `rust_client_path_install_with_own_echo.rs` (hops=2 + own-echo).
//!
//! The field failure was 9/20 fails on hardware (45 %), not 100 %,
//! which points at a timing race rather than a deterministic code-
//! path gate. This mvr loops the most realistic of the prior shapes
//! (two-daemon + own-echo) 25 times per `#[tokio::test]` invocation
//! with randomised jitter between daemon-spawn, Rust-client-connect,
//! and announce-emit. A 45 % hardware fail rate should surface at
//! least a few failures in 25 iterations unless the race is much
//! narrower than the hardware reading suggests.
//!
//! Kept under `#[ignore]` so the default test suite does not spend
//! ~100 s looping when this mvr does not actually reproduce the
//! bug. Unignored by the fix commit if a deterministic repro is
//! achieved.

use std::time::Duration;

use leviculum_core::{Destination, DestinationHash, DestinationType, Direction, Identity};
use leviculum_std::driver::ReticulumNodeBuilder;
use rand_core::{OsRng, RngCore};

use crate::harness::TestDaemon;

const TRUNCATED_HASHBYTES: usize = 16;
const LOOP_ITERATIONS: usize = 25;

fn parse_dest_hash(hex_str: &str) -> DestinationHash {
    let bytes: [u8; TRUNCATED_HASHBYTES] = hex::decode(hex_str)
        .expect("hex decode")
        .try_into()
        .expect("correct hash length");
    DestinationHash::new(bytes)
}

/// Random sleep in [min_ms, max_ms] — used to perturb the sequence
/// of daemon-connect, announce-emit, client-connect events so a
/// timing race can surface.
async fn jittered_sleep(min_ms: u64, max_ms: u64) {
    let span = max_ms.saturating_sub(min_ms).max(1);
    let extra = OsRng.next_u64() % span;
    tokio::time::sleep(Duration::from_millis(min_ms + extra)).await;
}

async fn one_iteration(iteration: usize) -> Result<(), String> {
    let daemon_a = TestDaemon::start()
        .await
        .map_err(|e| format!("iter {iteration}: start daemon A: {e}"))?;
    let daemon_b = TestDaemon::start()
        .await
        .map_err(|e| format!("iter {iteration}: start daemon B: {e}"))?;
    daemon_b
        .add_client_interface("127.0.0.1", daemon_a.rns_port(), Some("relay_to_A"))
        .await
        .map_err(|e| format!("iter {iteration}: wire B → A: {e}"))?;

    jittered_sleep(200, 800).await;

    let peer_info = daemon_a
        .register_destination("mvr", &["r1c2", "peer"])
        .await
        .map_err(|e| format!("iter {iteration}: register peer dest: {e}"))?;
    let peer_hash = parse_dest_hash(&peer_info.hash);

    let storage = tempfile::tempdir().map_err(|e| format!("iter {iteration}: tempdir: {e}"))?;
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon_b.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .map_err(|e| format!("iter {iteration}: build node: {e}"))?;
    node.start()
        .await
        .map_err(|e| format!("iter {iteration}: start node: {e}"))?;

    let own_identity = Identity::generate(&mut rand_core::OsRng);
    let own_dest = Destination::new(
        Some(own_identity),
        Direction::In,
        DestinationType::Single,
        "mvr",
        &["r1c2", "own"],
    )
    .map_err(|e| format!("iter {iteration}: create own dest: {e}"))?;
    let own_hash = *own_dest.hash();
    node.register_destination(own_dest);

    jittered_sleep(100, 600).await;

    // Race variant: sometimes announce own FIRST, sometimes peer
    // first. Randomise to cover both orderings.
    let own_first = OsRng.next_u64() & 1 == 0;
    if own_first {
        node.announce_destination(&own_hash, Some(b"loop-own"))
            .await
            .map_err(|e| format!("iter {iteration}: own announce: {e}"))?;
        jittered_sleep(50, 500).await;
        daemon_a
            .announce_destination(&peer_info.hash, b"loop-peer")
            .await
            .map_err(|e| format!("iter {iteration}: peer announce: {e}"))?;
    } else {
        daemon_a
            .announce_destination(&peer_info.hash, b"loop-peer")
            .await
            .map_err(|e| format!("iter {iteration}: peer announce: {e}"))?;
        jittered_sleep(50, 500).await;
        node.announce_destination(&own_hash, Some(b"loop-own"))
            .await
            .map_err(|e| format!("iter {iteration}: own announce: {e}"))?;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if node.has_path(&peer_hash) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    Err(format!(
        "iter {iteration}: path for peer {} NOT installed within 10 s",
        hex::encode(peer_hash.as_bytes()),
    ))
}

#[tokio::test]
#[ignore = "Bug #25 R1 Task C.2 loop harness — ~100 s runtime"]
async fn rust_client_path_install_race_loop() {
    let mut failures: Vec<String> = Vec::new();
    for i in 0..LOOP_ITERATIONS {
        match one_iteration(i).await {
            Ok(()) => {
                println!("LOOP iter={i} PASS");
            }
            Err(msg) => {
                println!("LOOP iter={i} FAIL: {msg}");
                failures.push(msg);
            }
        }
    }
    let n = LOOP_ITERATIONS;
    let fails = failures.len();
    println!(
        "\nLOOP SUMMARY: {} PASS / {} FAIL / {}",
        n - fails,
        fails,
        n
    );
    assert!(
        failures.is_empty(),
        "{fails}/{n} iterations failed to install the peer path. \
         First few failures:\n{}",
        failures
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
    );
}
