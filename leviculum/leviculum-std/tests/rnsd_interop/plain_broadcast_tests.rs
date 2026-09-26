//! Plain broadcast forwarding tests through a shared instance.
//!
//! Tests that a Rust daemon correctly forwards PLAIN BROADCAST data packets
//! between local clients (Unix socket) and network interfaces (TCP).
//!
//! Topology:
//! ```text
//! Python daemon B (TCP server) ← TCP ← Rust daemon (shared instance) ← Unix socket ← Python daemon A (client)
//! ```
//!
//! See Codeberg issue #24.

use std::net::SocketAddr;
use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;

use crate::common::{init_tracing, temp_storage, DAEMON_PROCESS_TIME};
use crate::harness::{find_available_ports, TestDaemon};

/// Test: Python local client sends a plain broadcast → Python remote node receives it.
///
/// This exercises the local-client → all-network-interfaces forwarding path
/// (Python Transport.py:1390-1393) which is missing in Rust.
#[tokio::test]
async fn test_plain_broadcast_local_client_to_network() {
    init_tracing();

    // Phase 1: Port allocation
    let (ports, _port_alloc) = find_available_ports::<4>()
        .await
        .expect("Failed to allocate ports");
    let [daemon_tcp_port, py_b_rns_port, py_b_cmd_port, py_a_cmd_port] = ports;
    let instance_name = format!("broadcast_l2n_{}", std::process::id());

    // Phase 2: Start Python daemon B (remote node, TCP server)
    let py_remote = TestDaemon::start_with_ports(py_b_rns_port, py_b_cmd_port)
        .await
        .expect("Failed to start Python remote daemon");

    // Register a PLAIN destination on the remote to receive broadcasts
    let _remote_dest_hash = py_remote
        .register_plain_destination("broadcast_test", &["echo"])
        .await
        .expect("Failed to register plain destination on remote");

    // Phase 3: Start Rust daemon (shared instance + TCP client to B)
    let py_b_addr: SocketAddr = format!("127.0.0.1:{}", py_b_rns_port).parse().unwrap();
    let daemon_tcp_addr: SocketAddr = format!("127.0.0.1:{}", daemon_tcp_port).parse().unwrap();

    let _storage = temp_storage("test_plain_broadcast_local_client_to_network", "daemon");
    let mut daemon_node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .share_instance(true)
        .instance_name(instance_name.clone())
        .add_tcp_server(daemon_tcp_addr)
        .add_tcp_client(py_b_addr)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust daemon node");
    daemon_node
        .start()
        .await
        .expect("Failed to start Rust daemon node");

    // Wait for TCP connection + Unix socket to be ready
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Phase 4: Start Python daemon A (shared instance client)
    // Python detects the existing Unix socket and connects as client.
    // Its py_a_rns_port is unused (shared instance client skips TCP).
    let (py_a_ports, _py_a_port_alloc_2) = find_available_ports::<2>().await.expect("ports");
    let py_a_rns_port = py_a_ports[0];
    let py_local =
        TestDaemon::start_with_shared_instance_ports(py_a_rns_port, py_a_cmd_port, &instance_name)
            .await
            .expect("Failed to start Python shared instance client");

    // Let Python A settle as shared instance client
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Phase 5: Send plain broadcast from local client (A)
    let test_data = b"hello broadcast world";
    py_local
        .send_plain_packet("broadcast_test", &["echo"], test_data)
        .await
        .expect("Failed to send plain broadcast from local client");

    // Phase 6: Verify remote (B) received the broadcast
    let mut received = false;
    for _ in 0..15 {
        tokio::time::sleep(DAEMON_PROCESS_TIME).await;
        let packets = py_remote
            .get_received_plain_packets()
            .await
            .expect("Failed to get received plain packets");
        if packets.iter().any(|p| p.data == test_data.to_vec()) {
            received = true;
            break;
        }
    }

    assert!(
        received,
        "Python remote (B) should have received the plain broadcast from local client (A)"
    );
}

/// Test: Python remote node sends a plain broadcast → Python local client receives it.
///
/// This exercises the network → local-client-interfaces forwarding path
/// (Python Transport.py:1396-1398) which is missing in Rust.
#[tokio::test]
async fn test_plain_broadcast_network_to_local_client() {
    init_tracing();

    // Phase 1: Port allocation
    let (ports, _port_alloc) = find_available_ports::<4>()
        .await
        .expect("Failed to allocate ports");
    let [daemon_tcp_port, py_b_rns_port, py_b_cmd_port, py_a_cmd_port] = ports;
    let instance_name = format!("broadcast_n2l_{}", std::process::id());

    // Phase 2: Start Python daemon B (remote node, TCP server)
    let py_remote = TestDaemon::start_with_ports(py_b_rns_port, py_b_cmd_port)
        .await
        .expect("Failed to start Python remote daemon");

    // Phase 3: Start Rust daemon (shared instance + TCP client to B)
    let py_b_addr: SocketAddr = format!("127.0.0.1:{}", py_b_rns_port).parse().unwrap();
    let daemon_tcp_addr: SocketAddr = format!("127.0.0.1:{}", daemon_tcp_port).parse().unwrap();

    let _storage = temp_storage("test_plain_broadcast_network_to_local_client", "daemon");
    let mut daemon_node = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .share_instance(true)
        .instance_name(instance_name.clone())
        .add_tcp_server(daemon_tcp_addr)
        .add_tcp_client(py_b_addr)
        .storage_path(_storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build Rust daemon node");
    daemon_node
        .start()
        .await
        .expect("Failed to start Rust daemon node");

    // Wait for TCP connection + Unix socket to be ready
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Phase 4: Start Python daemon A (shared instance client)
    let (py_a_ports, _py_a_port_alloc) = find_available_ports::<2>().await.expect("ports");
    let py_a_rns_port = py_a_ports[0];
    let py_local =
        TestDaemon::start_with_shared_instance_ports(py_a_rns_port, py_a_cmd_port, &instance_name)
            .await
            .expect("Failed to start Python shared instance client");

    // Let Python A settle as shared instance client
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Register PLAIN destination on the local client to receive broadcasts
    let _local_dest_hash = py_local
        .register_plain_destination("broadcast_test", &["echo"])
        .await
        .expect("Failed to register plain destination on local client");

    // Phase 5: Send plain broadcast from remote (B)
    let test_data = b"hello from network";
    py_remote
        .send_plain_packet("broadcast_test", &["echo"], test_data)
        .await
        .expect("Failed to send plain broadcast from remote");

    // Phase 6: Verify local client (A) received the broadcast
    let mut received = false;
    for _ in 0..15 {
        tokio::time::sleep(DAEMON_PROCESS_TIME).await;
        let packets = py_local
            .get_received_plain_packets()
            .await
            .expect("Failed to get received plain packets");
        if packets.iter().any(|p| p.data == test_data.to_vec()) {
            received = true;
            break;
        }
    }

    assert!(
        received,
        "Python local client (A) should have received the plain broadcast from remote (B)"
    );
}
