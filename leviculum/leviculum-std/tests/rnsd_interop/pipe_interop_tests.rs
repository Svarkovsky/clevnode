//! In-process integration tests for PipeInterface (Codeberg #95).
//!
//! These bridge two Rust nodes over a real subprocess pipe: each node runs a
//! `PipeInterface` whose `command` is a small Python stdio<->TCP relay
//! (`scripts/pipe_bridge.py`). One node listens, the other connects, and the
//! two relays splice the pipes together over loopback TCP. An announce emitted
//! on one node must therefore travel: node A -> A's pipe stdin -> A's relay ->
//! TCP -> B's relay -> B's pipe stdout -> node B. That end-to-end crossing is
//! the proof the interface frames, spawns, and pumps correctly against a live
//! peer.
//!
//! A second test drives the robustness path: a child that keeps exiting must be
//! respawned by the interface without ever taking the node down.

use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::{Destination, DestinationType, Direction, Identity, NodeEvent};

use crate::common::{init_tracing, temp_storage, wait_for_event};

/// Absolute path to the stdio<->TCP bridge helper.
const BRIDGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/pipe_bridge.py");

/// A loopback TCP port for the two bridge halves to rendezvous on. The number
/// crosses a process boundary through argv, so it cannot be a kept `:0`
/// listener; it comes from the host-wide allocator band above the ephemeral
/// range instead (Codeberg #221). `SO_REUSEADDR` on the listening half and the
/// connecting half's retry cover the allocator's own probe release.
fn free_tcp_port() -> u16 {
    crate::harness::port_alloc::free_tcp_port()
}

/// Positive path: an announce crosses the pipe bridge between two Rust nodes.
#[tokio::test]
async fn test_pipe_bridge_announce_crosses() {
    init_tracing();

    // Ensure the helper exists so a path/rename mistake fails loudly rather
    // than silently spawning nothing.
    assert!(
        std::path::Path::new(BRIDGE).exists(),
        "pipe bridge helper missing at {BRIDGE}"
    );

    let port = free_tcp_port();
    let storage_a = temp_storage("pipe_announce", "a");
    let storage_b = temp_storage("pipe_announce", "b");

    // Node A listens, node B connects — same bridge script, mirrored args
    // (the drop-in symmetry the interface is meant to provide).
    let cmd_a = format!("python3 {BRIDGE} listen {port}");
    let cmd_b = format!("python3 {BRIDGE} connect {port}");

    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_a.path().to_path_buf())
        .add_pipe_interface(cmd_a, Some(0.2))
        .build()
        .await
        .expect("build node A");

    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_b.path().to_path_buf())
        .add_pipe_interface(cmd_b, Some(0.2))
        .build()
        .await
        .expect("build node B");

    node_a.start().await.expect("start A");
    node_b.start().await.expect("start B");

    let mut events_b = node_b.take_event_receiver().expect("events B");

    // Both interfaces spawn their child immediately; give the connect-side
    // relay time to reach the listen-side over TCP before we announce.
    node_a
        .wait_for_interfaces_ready(Duration::from_secs(5))
        .await
        .expect("A interfaces ready");
    node_b
        .wait_for_interfaces_ready(Duration::from_secs(5))
        .await
        .expect("B interfaces ready");
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Register + announce a destination on node A.
    let identity_a = Identity::generate(&mut rand_core::OsRng);
    let dest_a = Destination::new(
        Some(identity_a),
        Direction::In,
        DestinationType::Single,
        "test",
        &["pipe_bridge"],
    )
    .expect("create destination");
    let dest_hash = *dest_a.hash();
    node_a.register_destination(dest_a);

    // Announce a few times: the first may race the bridge TCP handshake.
    let mut found = None;
    for _ in 0..5 {
        node_a
            .announce_destination(&dest_hash, Some(b"pipe-hello"))
            .await
            .expect("announce");
        found = wait_for_event(&mut events_b, Duration::from_secs(3), |event| {
            if let NodeEvent::AnnounceReceived { announce, .. } = event {
                if *announce.destination_hash() == dest_hash {
                    return Some(announce.app_data().to_vec());
                }
            }
            None
        })
        .await;
        if found.is_some() {
            break;
        }
    }

    node_a.stop().await.ok();
    node_b.stop().await.ok();

    let app_data = found.expect("node B should receive node A's announce across the pipe bridge");
    assert_eq!(
        app_data, b"pipe-hello",
        "app_data must survive the HDLC round-trip across the pipe"
    );

    // node B learned a path to A's destination through the pipe interface.
    assert!(
        node_b.has_path(&dest_hash),
        "node B should have a path to the announced destination via the pipe"
    );
}

/// Robustness path: a child that keeps exiting is respawned by the interface,
/// and the node never crashes.
#[tokio::test]
async fn test_pipe_child_exit_respawns_no_crash() {
    init_tracing();

    let storage = temp_storage("pipe_respawn", "node");

    // A child that exits almost immediately, forcing the interface to respawn
    // it over and over. `sh -c 'exit 0'` reads no stdin and closes stdout at
    // once, exercising the stdout-EOF -> respawn path repeatedly.
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage.path().to_path_buf())
        .add_pipe_interface("sh -c 'exit 0'".to_string(), Some(0.1))
        .build()
        .await
        .expect("build node");

    node.start().await.expect("start node");

    // Let the interface cycle through several spawn/exit/respawn iterations.
    tokio::time::sleep(Duration::from_millis(600)).await;

    // The node must still be alive and able to register + announce a
    // destination (which would panic/error if the interface task had died and
    // poisoned the driver). We don't assert delivery — there is no peer — only
    // that the stack keeps running across the child churn.
    let identity = Identity::generate(&mut rand_core::OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "test",
        &["pipe_respawn"],
    )
    .expect("create destination");
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    node.announce_destination(&dest_hash, Some(b"still-alive"))
        .await
        .expect("node must still accept announces across child respawns");

    node.stop()
        .await
        .expect("node stops cleanly after respawn churn");
}
