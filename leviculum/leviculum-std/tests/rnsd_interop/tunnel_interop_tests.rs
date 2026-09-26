//! Interop: a Rust node initiates the tunnel synthesize handshake toward a
//! Python `rnsd`, and Python validates it and (re)builds the tunnel (Codeberg
//! #64 initiator side).
//!
//! Topology: `Py-Daemon (TCP server) ← Rust-Node (TCP client / tunnel initiator)`.
//!
//! On connect the Rust node sends the `rnstransport.tunnel.synthesize` PLAIN
//! broadcast. Python's `tunnel_synthesize_handler` validates the signature and
//! calls `handle_tunnel`, populating `Transport.tunnels`. We assert:
//!  1. Python's tunnel table becomes non-empty (a bad signature or wrong wire
//!     format would leave it empty), proving wire + semantic compatibility.
//!  2. The `tunnel_id` Python keyed by equals the one the initiator advertised
//!     (`full_hash(public_key || interface_hash)`), tying Python's tunnel to
//!     our synthesize specifically.
//!  3. After a Python restart on the same port, the Rust client reconnects and
//!     re-initiates the synthesize, so Python rebuilds the tunnel — proving the
//!     reconnect path also fires across the interop boundary.
//!
//! Python-side path restore on reconnect is covered deterministically by the
//! in-process Rust<->Rust test (`transport::tunnel_restore_tests`): a Python
//! restart wipes Python's in-memory tunnel paths, so it cannot demonstrate
//! restore here. What this test does cover is the wire/semantic validation of
//! our synthesize by the reference stack, in both the connect and reconnect
//! directions.
//!
//! ## Running
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop tunnel_interop_tests -- --nocapture
//! ```

use std::collections::HashMap;
use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;

use crate::harness::TestDaemon;

/// Poll Python's tunnel table until it is non-empty or the deadline elapses.
async fn wait_for_tunnels(
    daemon: &TestDaemon,
    timeout: Duration,
) -> HashMap<String, serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let tunnels = daemon.get_tunnels().await.unwrap_or_default();
        if !tunnels.is_empty() || tokio::time::Instant::now() >= deadline {
            return tunnels;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn test_rust_initiates_tunnel_to_python() {
    let mut daemon = TestDaemon::start().await.expect("start daemon");

    let storage = crate::common::temp_storage("test_rust_initiates_tunnel_to_python", "node");
    let mut rust_node = ReticulumNodeBuilder::new()
        .enable_transport(false)
        .add_tcp_client(daemon.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build Rust node");
    rust_node.start().await.expect("start Rust node");

    // 1) On connect the Rust node initiates the synthesize; Python validates it
    //    and builds the tunnel.
    let tunnels = wait_for_tunnels(&daemon, Duration::from_secs(15)).await;
    assert!(
        !tunnels.is_empty(),
        "Python should build a tunnel from the Rust node's synthesize"
    );

    // 2) The tunnel id Python keyed by equals the one our node advertised.
    let ours: Vec<String> = rust_node.tunnel_ids().iter().map(hex::encode).collect();
    assert!(
        !ours.is_empty(),
        "Rust node should have advertised a tunnel id"
    );
    assert!(
        ours.iter().any(|id| tunnels.contains_key(id)),
        "Python's tunnel id should match the initiator's advertised id: python={:?} ours={:?}",
        tunnels.keys().collect::<Vec<_>>(),
        ours
    );

    // 3) Restart Python on the same port: the Rust client reconnects and
    //    re-initiates the synthesize, so Python rebuilds the tunnel.
    daemon.restart().await.expect("restart daemon");
    let tunnels_after = wait_for_tunnels(&daemon, Duration::from_secs(30)).await;
    assert!(
        !tunnels_after.is_empty(),
        "Rust client should re-initiate the synthesize on reconnect so Python rebuilds the tunnel"
    );
    assert!(
        ours.iter().any(|id| tunnels_after.contains_key(id)),
        "rebuilt tunnel id should match the initiator's advertised id: python={:?} ours={:?}",
        tunnels_after.keys().collect::<Vec<_>>(),
        ours
    );
}
