//! Codeberg #88 blackhole enforcement interop test.
//!
//! A Python operator (`rnpath.py -B/-U`, the real vendor utility) manages the
//! blackhole set of our running lnsd over the shared-instance RPC, and the
//! daemon must then ENFORCE it on live traffic (Python read sites:
//! Identity.py:574-577 announce drop, Transport.py:3423/3494-3513 path
//! removal):
//!
//!   1. blackholing an identity removes its already-learned path row,
//!   2. a fresh announce from the blackholed identity is dropped (no path,
//!      drops_blackholed_announce ticks),
//!   3. NEGATIVE guard: another identity's announces keep flowing while the
//!      blackhole is active,
//!   4. unblackholing restores normal announce handling.
//!
//! ## Topology
//!
//! ```text
//!   injector (raw TCP) --announces--> lnsd DUT <--RPC-- rnpath.py (operator)
//! ```
//!
//! Announces are paced well below the 3 Hz ingress-burst threshold so the
//! #87 hold-and-release machinery never engages; every assertion polls for
//! its condition (eventual consistency) instead of sleeping fixed amounts.
//!
//! Marked `#[ignore]`: spawns Python tooling. Run with:
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop \
//!     blackhole_interop -- --include-ignored --test-threads=1
//! ```

use std::time::Duration;

use rand_core::OsRng;
use tokio::net::TcpStream;

use leviculum_core::constants::MTU;
use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};

use crate::common::{cleanup_config_dir, init_tracing, now_ms, send_framed};
use crate::rpc_interop_tests::{
    python_client_ready, run_python_tool, start_rust_daemon_with_rpc, RNPATH_PY,
};

/// Build a signed announce for a caller-supplied identity, so the test can
/// blackhole exactly that identity via rnpath. Each call produces a fresh
/// random hash (new packet hash), so re-announces are not deduplicated.
fn build_announce_for_identity(identity: &Identity, aspect: &str) -> (Vec<u8>, DestinationHash) {
    let mut dest = Destination::new(
        Some(identity.clone()),
        Direction::In,
        DestinationType::Single,
        "bhinterop",
        &[aspect],
    )
    .expect("create destination");
    let packet = dest
        .announce(Some(b"bh"), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("create announce");
    let mut raw = [0u8; MTU];
    let size = packet.pack(&mut raw).expect("pack announce");
    (raw[..size].to_vec(), *dest.hash())
}

/// Send one announce, paced below the ingress-burst threshold. The #87
/// hold-and-release limiter holds announces for unknown destinations when the
/// per-interface announce frequency crosses ~3 Hz on a new interface; a held
/// announce is only released after a 15 s penalty, which would make the
/// assertions below time out for the wrong reason. A fixed 1.5 s pre-send gap
/// keeps the measured frequency well under the threshold (same spacing fix as
/// the #87 discovery tests).
async fn send_announce_paced(stream: &mut TcpStream, raw: &[u8]) {
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    send_framed(stream, raw).await;
}

/// Poll until `condition` holds or the deadline passes; returns the final
/// evaluation. Never couples an assertion to a fixed sleep.
async fn wait_until<F: FnMut() -> bool>(mut condition: F, deadline: Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        if condition() {
            return true;
        }
        if start.elapsed() >= deadline {
            return condition();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// End-to-end blackhole enforcement driven by the Python operator tooling.
#[tokio::test]
#[ignore = "spawns Python tooling; reviewer runs after the tier3"]
async fn python_operator_blackhole_is_enforced_by_lnsd() {
    init_tracing();

    let (node, instance_name, tcp_addr, identity_bytes, _storage) =
        start_rust_daemon_with_rpc().await;
    let config_dir = python_client_ready(&instance_name, &identity_bytes).await;

    // Raw TCP injector: one connection, kept open for the whole test so the
    // interface (and its ingress state) is stable.
    let mut injector = TcpStream::connect(tcp_addr)
        .await
        .expect("connect injector");

    let id_victim = Identity::generate(&mut OsRng);
    let id_bystander = Identity::generate(&mut OsRng);
    let victim_hash_hex = hex::encode(id_victim.hash());

    // Step 1: the victim announces BEFORE being blackholed and a path is
    // learned. This proves ingest works, and arms the path-removal check.
    let (raw, victim_dest) = build_announce_for_identity(&id_victim, "victim");
    send_announce_paced(&mut injector, &raw).await;
    assert!(
        wait_until(|| node.has_path(&victim_dest), Duration::from_secs(10)).await,
        "pre-blackhole announce must create a path"
    );

    // Step 2: the Python operator blackholes the victim identity via the
    // shared-instance RPC (real rnpath.py against our daemon).
    let output = run_python_tool(RNPATH_PY, &["-B", &victim_hash_hex], &config_dir).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("Blackholed identity"),
        "rnpath -B must succeed, got status {:?} stdout {:?} stderr {:?}",
        output.status.code(),
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );

    // The already-learned path row is removed (remove_blackholed_paths,
    // Transport.py:3423).
    assert!(
        wait_until(|| !node.has_path(&victim_dest), Duration::from_secs(10)).await,
        "blackholing must remove the victim's existing path row"
    );

    // Step 3: a fresh announce from the blackholed identity is DROPPED.
    let drops_before = node.transport_stats().drops_blackholed_announce();
    let (raw, victim_dest_2) = build_announce_for_identity(&id_victim, "victim");
    assert_eq!(
        victim_dest, victim_dest_2,
        "same identity+aspect, same dest"
    );
    send_announce_paced(&mut injector, &raw).await;
    assert!(
        wait_until(
            || node.transport_stats().drops_blackholed_announce() > drops_before,
            Duration::from_secs(10)
        )
        .await,
        "the blackholed identity's announce must tick drops_blackholed_announce"
    );
    assert!(
        !node.has_path(&victim_dest),
        "the blackholed identity's announce must not create a path"
    );

    // Step 4: NEGATIVE guard: while the blackhole is active, a different
    // identity's announce is processed normally.
    let (raw, bystander_dest) = build_announce_for_identity(&id_bystander, "bystander");
    send_announce_paced(&mut injector, &raw).await;
    assert!(
        wait_until(|| node.has_path(&bystander_dest), Duration::from_secs(10)).await,
        "a non-blackholed identity's announce must still create a path"
    );
    assert!(
        !node.has_path(&victim_dest),
        "the victim must stay pathless while the bystander is processed"
    );

    // Step 5: the operator lifts the blackhole; the victim announces normally
    // again.
    let output = run_python_tool(RNPATH_PY, &["-U", &victim_hash_hex], &config_dir).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("Lifted blackhole"),
        "rnpath -U must succeed, got status {:?} stdout {:?} stderr {:?}",
        output.status.code(),
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );

    let (raw, victim_dest_3) = build_announce_for_identity(&id_victim, "victim");
    assert_eq!(victim_dest, victim_dest_3);
    send_announce_paced(&mut injector, &raw).await;
    assert!(
        wait_until(|| node.has_path(&victim_dest), Duration::from_secs(10)).await,
        "after unblackhole the victim's announce must create a path again"
    );

    cleanup_config_dir(&config_dir);
}
