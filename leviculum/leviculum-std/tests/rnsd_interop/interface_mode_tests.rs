//! Interface-mode announce-propagation interop tests (Codeberg #91).
//!
//! Each test runs a live Python `test_daemon.py` peer as the reference
//! observer and a Rust node whose single TCP interface toward that peer is
//! configured with a Reticulum propagation mode. The Rust node announces a
//! local destination; whether the Python peer learns the path is exactly the
//! per-mode `Transport.outbound()` rule (Transport.py:1193-1245):
//!
//! - `access_point`: never emits announces, so the Python peer never learns
//!   the path (a Python rnsd with an AP interface behaves identically).
//! - `roaming`: emits local-origin announces, so the peer learns the path.
//! - `gateway` / `full`: behave like a normal interface; the peer learns it.
//!
//! Run:
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop interface_mode_tests
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use leviculum_core::identity::Identity;
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};
use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::wait_for_path_on_daemon;
use crate::harness::{pick_free_tcp_port, TestDaemon};

/// Build a Rust transport node with a single TCP client interface toward the
/// Python daemon, carrying the given propagation mode.
async fn start_rust_node_with_mode(
    test_name: &str,
    daemon: &TestDaemon,
    mode: &str,
) -> (leviculum_std::driver::ReticulumNode, tempfile::TempDir) {
    let storage = crate::common::temp_storage(test_name, "node");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_client(daemon.rns_addr())
        .interface_mode(mode)
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build rust node");
    node.start().await.expect("start rust node");
    tokio::time::sleep(Duration::from_secs(1)).await;
    (node, storage)
}

/// Register a fresh local Single destination on the node and announce it.
/// Returns its destination hash.
async fn announce_local_dest(
    node: &mut leviculum_std::driver::ReticulumNode,
    aspect: &str,
) -> DestinationHash {
    let identity = Identity::generate(&mut rand_core::OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mode_test",
        &[aspect],
    )
    .expect("destination");
    dest.set_accepts_links(true);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    node.announce_destination(&dest_hash, Some(b"mode-test"))
        .await
        .expect("announce");
    DestinationHash::new(*dest_hash.as_bytes())
}

/// access_point: the Rust node must withhold the announce on the AP interface,
/// so the Python peer never learns the path. Mirrors Python Transport.py:1195,
/// where an AP-mode interface blocks announce broadcasts unconditionally.
#[tokio::test]
async fn test_access_point_interface_withholds_local_announce() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let (mut node, _storage) =
        start_rust_node_with_mode("mode_ap_withholds", &daemon, "access_point").await;

    let dest_hash = announce_local_dest(&mut node, "ap").await;

    // Generous window: the initial announce plus every retry-scheduler firing
    // is gated, so the peer must NOT learn the path within it.
    let learned = wait_for_path_on_daemon(&daemon, &dest_hash, Duration::from_secs(6)).await;
    assert!(
        !learned,
        "access_point interface must withhold the announce; Python peer learned the path"
    );

    node.stop().await.ok();
}

/// roaming: a local-origin announce IS emitted on a roaming interface (Python
/// Transport.py:1199-1207 allows instance-local destinations), so the Python
/// peer learns the path.
#[tokio::test]
async fn test_roaming_interface_propagates_local_announce() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let (mut node, _storage) =
        start_rust_node_with_mode("mode_roaming_local", &daemon, "roaming").await;

    let dest_hash = announce_local_dest(&mut node, "roam").await;

    let learned = crate::common::wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &node,
        b"mode-test",
        Duration::from_secs(6),
    )
    .await;
    assert!(
        learned,
        "roaming interface must emit local-origin announces; Python peer did not learn the path"
    );

    node.stop().await.ok();
}

/// gateway: behaves like a normal interface for announce propagation (falls
/// through to the default branch in Transport.outbound), so the peer learns it.
/// This is the positive control distinguishing AP (withheld) from the modes
/// that still propagate.
#[tokio::test]
async fn test_gateway_interface_propagates_local_announce() {
    let daemon = TestDaemon::start().await.expect("start daemon");
    let (mut node, _storage) =
        start_rust_node_with_mode("mode_gateway_prop", &daemon, "gateway").await;

    let dest_hash = announce_local_dest(&mut node, "gw").await;

    let learned = crate::common::wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &node,
        b"mode-test",
        Duration::from_secs(6),
    )
    .await;
    assert!(
        learned,
        "gateway interface must propagate announces; Python peer did not learn the path"
    );

    node.stop().await.ok();
}

// =========================================================================
// Codeberg #104: server-side mode stamping.
//
// The three tests above configure the mode on a Rust TCP *client* interface.
// These configure the mode on a Rust TCP *server* interface and let a live
// Python daemon connect INTO it. The mode must be stamped onto the
// spawned-per-connection (inbound) interface, so the inbound-side announce
// rules apply to that Python peer exactly as they would if it had connected to
// a Python rnsd whose TCPServerInterface carried the same mode
// (TCPInterface.py:625: `spawned_interface.mode = self.mode`).
// =========================================================================

/// Build a Rust transport node with a single TCP *server* interface carrying
/// the given propagation mode, bound on a free local port. Returns the node,
/// the bound port (so a Python peer can connect into it), and the storage dir.
async fn start_rust_server_node_with_mode(
    test_name: &str,
    mode: &str,
) -> (leviculum_std::driver::ReticulumNode, u16, tempfile::TempDir) {
    let storage = crate::common::temp_storage(test_name, "node");
    let port = pick_free_tcp_port().expect("free tcp port");
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().expect("addr");
    let mut node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .add_tcp_server(addr)
        .interface_mode(mode)
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("build rust server node");
    node.start().await.expect("start rust server node");
    tokio::time::sleep(Duration::from_secs(1)).await;
    (node, port, storage)
}

/// access_point server: the mode stamped onto the spawned inbound interface
/// withholds the local announce, so the Python peer that connected into the
/// Rust AP server never learns the path. This is exactly what an inbound peer
/// sees from a Python rnsd whose TCPServerInterface is in access_point mode.
#[tokio::test]
async fn test_access_point_server_withholds_announce_to_inbound_peer() {
    let (mut node, port, _storage) =
        start_rust_server_node_with_mode("mode_ap_server", "access_point").await;

    // Python daemon connects INTO the Rust AP server as a TCP client. On the
    // Rust side this spawns a per-connection interface that must inherit the
    // listener's access_point mode.
    let daemon = TestDaemon::start().await.expect("start daemon");
    daemon
        .add_client_interface("127.0.0.1", port, Some("ToRustAP"))
        .await
        .expect("python connects to rust AP server");
    // Let the connection establish and the spawned interface register.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dest_hash = announce_local_dest(&mut node, "apserver").await;

    let learned = wait_for_path_on_daemon(&daemon, &dest_hash, Duration::from_secs(6)).await;
    assert!(
        !learned,
        "access_point server must withhold announces on the spawned inbound \
         interface; the Python peer learned the path"
    );

    node.stop().await.ok();
}

/// gateway server (positive control): a gateway-mode listener propagates, so
/// the spawned inbound interface still emits the local announce and the Python
/// peer learns the path. Distinguishes real mode stamping from a spawned
/// interface that silently dropped everything.
#[tokio::test]
async fn test_gateway_server_propagates_announce_to_inbound_peer() {
    let (mut node, port, _storage) =
        start_rust_server_node_with_mode("mode_gw_server", "gateway").await;

    let daemon = TestDaemon::start().await.expect("start daemon");
    daemon
        .add_client_interface("127.0.0.1", port, Some("ToRustGW"))
        .await
        .expect("python connects to rust gateway server");
    tokio::time::sleep(Duration::from_secs(1)).await;

    let dest_hash = announce_local_dest(&mut node, "gwserver").await;

    let learned = crate::common::wait_for_node_reannounce_on_daemon(
        &daemon,
        &dest_hash,
        &node,
        b"mode-test",
        Duration::from_secs(6),
    )
    .await;
    assert!(
        learned,
        "gateway server must propagate announces on the spawned inbound \
         interface; the Python peer did not learn the path"
    );

    node.stop().await.ok();
}
