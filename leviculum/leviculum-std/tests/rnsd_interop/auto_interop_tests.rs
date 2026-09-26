//! In-process integration tests for AutoInterface.
//!
//! These tests use multicast loopback to run two or more Rust nodes on the same
//! machine without a Python daemon or real network. Each test uses unique ports
//! and group_id to avoid parallel test collisions.
//!
//! Tests skip gracefully if no suitable NIC with IPv6 link-local is available.

use std::path::PathBuf;
use std::time::Duration;

use leviculum_std::driver::ReticulumNodeBuilder;
use leviculum_std::interfaces::auto_interface::{enumerate_nics, AutoInterfaceConfig};
use leviculum_std::{Destination, DestinationType, Direction, Identity, NodeEvent};

use crate::common::{
    generate_test_payload, verify_test_payload, wait_for_data_event, wait_for_event,
    wait_for_link_established, wait_for_path_on_node, wait_for_responder_established_link,
    CHANNEL_OVERHEAD,
};
use crate::harness::port_alloc;

/// Create a unique temp storage path for a test node.
///
/// Each node needs its own storage to get a unique transport identity.
fn temp_storage(test_name: &str, node: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "reticulum_auto_test_{}_{}_{}",
        test_name,
        node,
        std::process::id()
    ));
    // Clean up any leftover from previous runs
    let _ = std::fs::remove_dir_all(&path);
    path
}

// =========================================================================
// Constants
// =========================================================================

/// AutoInterface link MDU: floor((1196 - 1 - 19 - 48) / 16) * 16 - 1 = 1119
const AUTO_LINK_MDU: usize = 1119;

/// Max channel payload over AutoInterface: 1119 - 6 = 1113
const AUTO_MAX_CHANNEL_PAYLOAD: usize = AUTO_LINK_MDU - CHANNEL_OVERHEAD;

// =========================================================================
// Helpers
// =========================================================================

/// A discovery port whose successor is drawn from the allocator as well.
///
/// AutoInterface derives the unicast discovery port as `discovery_port + 1`,
/// matching Python —
/// `unicast_discovery_port` (leviculum-std/src/interfaces/auto_interface/mod.rs:434)
/// — so one test occupies two consecutive numbers, and the allocator has to
/// hand out both or the successor goes to the next caller.
fn free_discovery_port() -> u16 {
    port_alloc::free_udp_port_pair()
}

/// Create AutoInterface config for a test node.
///
/// Port layout per test:
///   discovery_port     = multicast discovery port (shared by all nodes in a test)
///   discovery_port + 1 = unicast discovery port, derived by the interface
///   data_port          = unique per node for same-machine disambiguation
///
/// Each node in a test must get its own `data_port` so they don't collide.
/// This avoids SO_REUSEPORT ambiguity for unicast data delivery.
///
/// Both numbers come from the host-wide allocator rather than a literal.
/// Until Codeberg #207 this file pinned 39000 and 39100-39704, all inside the
/// default `ip_local_port_range` (32768-60999): the kernel hands those numbers
/// out to anything in the workspace asking for a free port, so a concurrent
/// test could be given one of ours and the resulting red was unattributable —
/// nobody chose the number twice. `port_alloc` allocates from 61000-65000,
/// above that ceiling, under a counter shared by every test process on the
/// host.
fn auto_config_for_test(
    test_name: &str,
    discovery_port: u16,
    data_port: u16,
) -> AutoInterfaceConfig {
    AutoInterfaceConfig {
        group_id: format!("test_{}", test_name).into_bytes(),
        discovery_port,
        data_port,
        multicast_loopback: true,
        ..Default::default()
    }
}

/// Configs for the `N` nodes of one test: one shared multicast discovery port,
/// one data port each.
fn auto_configs_for_test<const N: usize>(test_name: &str) -> [AutoInterfaceConfig; N] {
    let discovery_port = free_discovery_port();
    std::array::from_fn(|_| {
        auto_config_for_test(test_name, discovery_port, port_alloc::free_udp_port())
    })
}

/// Check if we have NICs suitable for AutoInterface multicast tests.
/// Returns `true` if tests should run, `false` to skip.
///
/// Also initializes tracing (idempotent) so `RUST_LOG=debug` works.
fn have_suitable_nics() -> bool {
    crate::common::init_tracing();

    // No ports allocated here: `enumerate_nics` reads the NIC list and the
    // device filters, and binds nothing.
    let config = AutoInterfaceConfig {
        group_id: b"test_check".to_vec(),
        multicast_loopback: true,
        ..Default::default()
    };
    let nics = enumerate_nics(&config);
    if nics.is_empty() {
        tracing::warn!("SKIP: no suitable NICs for AutoInterface test");
        false
    } else {
        tracing::info!(
            "AutoInterface test: found {} NIC(s): {}",
            nics.len(),
            nics.iter()
                .map(|n| format!("{}({})", n.name, n.link_local))
                .collect::<Vec<_>>()
                .join(", ")
        );
        true
    }
}

// =========================================================================
// Test 1: Mutual Discovery
// =========================================================================

/// Two nodes discover each other via multicast and an announce is received.
#[tokio::test]
async fn test_auto_mutual_discovery() {
    if !have_suitable_nics() {
        return;
    }

    let [config_a, config_b] = auto_configs_for_test("discovery");
    let storage_a = temp_storage("discovery", "a");
    let storage_b = temp_storage("discovery", "b");

    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_a.clone())
        .add_auto_interface_with_config(config_a)
        .build()
        .await
        .expect("build node A");

    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_b.clone())
        .add_auto_interface_with_config(config_b)
        .build()
        .await
        .expect("build node B");

    node_a.start().await.expect("start A");
    node_b.start().await.expect("start B");

    let mut events_b = node_b.take_event_receiver().expect("events B");

    // Wait for peer discovery (multicast announce interval ~1.6s)
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Register and announce a destination on node A
    let identity_a = Identity::generate(&mut rand_core::OsRng);
    let dest_a = Destination::new(
        Some(identity_a),
        Direction::In,
        DestinationType::Single,
        "test",
        &["auto_discovery"],
    )
    .expect("create destination");
    let dest_hash = *dest_a.hash();
    node_a.register_destination(dest_a);
    node_a
        .announce_destination(&dest_hash, Some(b"discovery"))
        .await
        .expect("announce");

    // Wait for AnnounceReceived on node B
    let found = wait_for_event(&mut events_b, Duration::from_secs(10), |event| {
        if let NodeEvent::AnnounceReceived { announce, .. } = event {
            if *announce.destination_hash() == dest_hash {
                return Some(());
            }
        }
        None
    })
    .await;

    node_a.stop().await.ok();
    node_b.stop().await.ok();
    let _ = std::fs::remove_dir_all(&storage_a);
    let _ = std::fs::remove_dir_all(&storage_b);

    assert!(
        found.is_some(),
        "Node B should receive announce from Node A"
    );
}

// =========================================================================
// Test 2: Announce Propagation with App Data
// =========================================================================

/// Verify that announce app_data is correctly propagated between nodes.
#[tokio::test]
async fn test_auto_announce_propagation() {
    if !have_suitable_nics() {
        return;
    }

    let [config_a, config_b] = auto_configs_for_test("announce");
    let storage_a = temp_storage("announce", "a");
    let storage_b = temp_storage("announce", "b");

    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_a.clone())
        .add_auto_interface_with_config(config_a)
        .build()
        .await
        .expect("build node A");

    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_b.clone())
        .add_auto_interface_with_config(config_b)
        .build()
        .await
        .expect("build node B");

    node_a.start().await.expect("start A");
    node_b.start().await.expect("start B");

    let mut events_b = node_b.take_event_receiver().expect("events B");

    // Wait for peer discovery
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Register + announce with app_data
    let identity_a = Identity::generate(&mut rand_core::OsRng);
    let dest_a = Destination::new(
        Some(identity_a),
        Direction::In,
        DestinationType::Single,
        "test",
        &["auto_announce"],
    )
    .expect("create destination");
    let dest_hash = *dest_a.hash();
    node_a.register_destination(dest_a);
    node_a
        .announce_destination(&dest_hash, Some(b"auto-announce"))
        .await
        .expect("announce");

    // Wait for AnnounceReceived on node B and check app_data
    let result = wait_for_event(&mut events_b, Duration::from_secs(10), |event| {
        if let NodeEvent::AnnounceReceived { announce, .. } = event {
            if *announce.destination_hash() == dest_hash {
                return Some((*announce.destination_hash(), announce.app_data().to_vec()));
            }
        }
        None
    })
    .await;

    node_a.stop().await.ok();
    node_b.stop().await.ok();
    let _ = std::fs::remove_dir_all(&storage_a);
    let _ = std::fs::remove_dir_all(&storage_b);

    let (received_hash, received_app_data) = result.expect("should receive announce");
    assert_eq!(received_hash, dest_hash, "destination hash mismatch");
    assert_eq!(received_app_data, b"auto-announce", "app_data should match");
}

// =========================================================================
// Test 3: Link + Bidirectional Data
// =========================================================================

/// Establish a link over AutoInterface and exchange data in both directions.
#[tokio::test]
async fn test_auto_link_bidirectional_data() {
    if !have_suitable_nics() {
        return;
    }

    let [config_a, config_b] = auto_configs_for_test("link_data");
    let storage_a = temp_storage("link_data", "a");
    let storage_b = temp_storage("link_data", "b");

    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_a.clone())
        .add_auto_interface_with_config(config_a)
        .build()
        .await
        .expect("build node A");

    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_b.clone())
        .add_auto_interface_with_config(config_b)
        .build()
        .await
        .expect("build node B");

    node_a.start().await.expect("start A");
    node_b.start().await.expect("start B");

    let mut events_a = node_a.take_event_receiver().expect("events A");
    let mut events_b = node_b.take_event_receiver().expect("events B");

    // Wait for peer discovery
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Node B: create destination that accepts links, register and announce
    let identity_b = Identity::generate(&mut rand_core::OsRng);
    let mut dest_b = Destination::new(
        Some(identity_b),
        Direction::In,
        DestinationType::Single,
        "test",
        &["auto_link"],
    )
    .expect("create destination B");
    dest_b.set_accepts_links(true);
    let dest_hash_b = *dest_b.hash();
    node_b.register_destination(dest_b);
    node_b
        .announce_destination(&dest_hash_b, Some(b"link-test"))
        .await
        .expect("announce B");

    // Node A: wait for path, then extract signing key from announce
    let signing_key = wait_for_event(&mut events_a, Duration::from_secs(10), |event| {
        if let NodeEvent::AnnounceReceived { announce, .. } = event {
            if *announce.destination_hash() == dest_hash_b {
                let mut key = [0u8; 32];
                key.copy_from_slice(&announce.public_key()[32..64]);
                return Some(key);
            }
        }
        None
    })
    .await
    .expect("should receive announce with signing key");

    // Also wait for path to be registered
    assert!(
        wait_for_path_on_node(&node_a, &dest_hash_b, Duration::from_secs(5)).await,
        "Node A should have path to B"
    );

    // Node A: connect to B
    let handle_a = node_a
        .connect(&dest_hash_b, &signing_key)
        .await
        .expect("connect A→B");
    let link_id_a = handle_a.link_id();

    // Node B: incoming link is auto-accepted and proved by the core; wait for it
    // to establish, then mint a writable handle.
    let link_id_b = wait_for_responder_established_link(&mut events_b, Duration::from_secs(10))
        .await
        .expect("B should establish incoming link");
    let handle_b = node_b.link_handle(&link_id_b);

    // Wait for LinkEstablished on A (B already established above).
    assert!(
        wait_for_link_established(&mut events_a, link_id_a, Duration::from_secs(10)).await,
        "Link should establish on A side"
    );

    // A→B: send 256-byte payload
    let payload_a_to_b = generate_test_payload(256);
    handle_a.send(&payload_a_to_b).await.expect("send A→B");

    let received_on_b = wait_for_data_event(&mut events_b, &link_id_b, Duration::from_secs(10))
        .await
        .expect("B should receive data from A");
    assert!(
        verify_test_payload(&received_on_b),
        "A→B payload should match"
    );
    assert_eq!(received_on_b.len(), 256, "A→B payload size mismatch");

    // B→A: send 128-byte payload
    let payload_b_to_a = generate_test_payload(128);
    handle_b.send(&payload_b_to_a).await.expect("send B→A");

    let received_on_a = wait_for_data_event(&mut events_a, link_id_a, Duration::from_secs(10))
        .await
        .expect("A should receive data from B");
    assert!(
        verify_test_payload(&received_on_a),
        "B→A payload should match"
    );
    assert_eq!(received_on_a.len(), 128, "B→A payload size mismatch");

    node_a.stop().await.ok();
    node_b.stop().await.ok();
    let _ = std::fs::remove_dir_all(&storage_a);
    let _ = std::fs::remove_dir_all(&storage_b);
}

// =========================================================================
// Test 4: MTU Negotiation
// =========================================================================

/// Verify link MTU negotiation values and send a max-size payload.
#[tokio::test]
async fn test_auto_mtu_negotiation() {
    if !have_suitable_nics() {
        return;
    }

    let [config_a, config_b] = auto_configs_for_test("mtu");
    let storage_a = temp_storage("mtu", "a");
    let storage_b = temp_storage("mtu", "b");

    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_a.clone())
        .add_auto_interface_with_config(config_a)
        .build()
        .await
        .expect("build node A");

    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_b.clone())
        .add_auto_interface_with_config(config_b)
        .build()
        .await
        .expect("build node B");

    node_a.start().await.expect("start A");
    node_b.start().await.expect("start B");

    let mut events_a = node_a.take_event_receiver().expect("events A");
    let mut events_b = node_b.take_event_receiver().expect("events B");

    // Wait for peer discovery
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Node B: create link-accepting destination
    let identity_b = Identity::generate(&mut rand_core::OsRng);
    let mut dest_b = Destination::new(
        Some(identity_b),
        Direction::In,
        DestinationType::Single,
        "test",
        &["auto_mtu"],
    )
    .expect("create destination B");
    dest_b.set_accepts_links(true);
    let dest_hash_b = *dest_b.hash();
    node_b.register_destination(dest_b);
    node_b
        .announce_destination(&dest_hash_b, Some(b"mtu-test"))
        .await
        .expect("announce B");

    // Node A: wait for announce and extract signing key
    let signing_key = wait_for_event(&mut events_a, Duration::from_secs(10), |event| {
        if let NodeEvent::AnnounceReceived { announce, .. } = event {
            if *announce.destination_hash() == dest_hash_b {
                let mut key = [0u8; 32];
                key.copy_from_slice(&announce.public_key()[32..64]);
                return Some(key);
            }
        }
        None
    })
    .await
    .expect("should receive announce");

    assert!(
        wait_for_path_on_node(&node_a, &dest_hash_b, Duration::from_secs(5)).await,
        "Node A should have path to B"
    );

    // Connect
    let handle_a = node_a
        .connect(&dest_hash_b, &signing_key)
        .await
        .expect("connect A→B");
    let link_id_a = handle_a.link_id();

    let link_id_b = wait_for_responder_established_link(&mut events_b, Duration::from_secs(10))
        .await
        .expect("B should establish incoming link");

    assert!(
        wait_for_link_established(&mut events_a, link_id_a, Duration::from_secs(10)).await,
        "Link should establish on A"
    );

    // Verify MTU values
    let negotiated_mtu = node_a.link_negotiated_mtu(link_id_a);
    let mdu = node_a.link_mdu(link_id_a);

    assert_eq!(
        negotiated_mtu,
        Some(1196),
        "AutoInterface negotiated MTU should be 1196"
    );
    assert_eq!(
        mdu,
        Some(AUTO_LINK_MDU),
        "AutoInterface link MDU should be {}",
        AUTO_LINK_MDU
    );

    // Send max-size channel payload
    let max_payload = generate_test_payload(AUTO_MAX_CHANNEL_PAYLOAD);
    handle_a.send(&max_payload).await.expect("send max payload");

    let received = wait_for_data_event(&mut events_b, &link_id_b, Duration::from_secs(10))
        .await
        .expect("B should receive max payload");
    assert!(verify_test_payload(&received), "max payload should match");
    assert_eq!(
        received.len(),
        AUTO_MAX_CHANNEL_PAYLOAD,
        "received size mismatch"
    );

    node_a.stop().await.ok();
    node_b.stop().await.ok();
    let _ = std::fs::remove_dir_all(&storage_a);
    let _ = std::fs::remove_dir_all(&storage_b);
}

// =========================================================================
// Test 5: Peer Timeout
// =========================================================================

/// Verify that when one node stops, the other detects the loss.
#[tokio::test]
async fn test_auto_peer_timeout() {
    if !have_suitable_nics() {
        return;
    }

    let [config_a, config_b] = auto_configs_for_test("timeout");
    let storage_a = temp_storage("timeout", "a");
    let storage_b = temp_storage("timeout", "b");

    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_a.clone())
        .add_auto_interface_with_config(config_a)
        .build()
        .await
        .expect("build node A");

    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_b.clone())
        .add_auto_interface_with_config(config_b)
        .build()
        .await
        .expect("build node B");

    node_a.start().await.expect("start A");
    node_b.start().await.expect("start B");

    let mut events_a = node_a.take_event_receiver().expect("events A");
    let mut events_b = node_b.take_event_receiver().expect("events B");

    // Wait for peer discovery
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Node B: create link-accepting destination, announce
    let identity_b = Identity::generate(&mut rand_core::OsRng);
    let mut dest_b = Destination::new(
        Some(identity_b),
        Direction::In,
        DestinationType::Single,
        "test",
        &["auto_timeout"],
    )
    .expect("create destination B");
    dest_b.set_accepts_links(true);
    let dest_hash_b = *dest_b.hash();
    node_b.register_destination(dest_b);
    node_b
        .announce_destination(&dest_hash_b, Some(b"timeout-test"))
        .await
        .expect("announce B");

    // Node A: wait for announce, connect
    let signing_key = wait_for_event(&mut events_a, Duration::from_secs(10), |event| {
        if let NodeEvent::AnnounceReceived { announce, .. } = event {
            if *announce.destination_hash() == dest_hash_b {
                let mut key = [0u8; 32];
                key.copy_from_slice(&announce.public_key()[32..64]);
                return Some(key);
            }
        }
        None
    })
    .await
    .expect("should receive announce");

    assert!(
        wait_for_path_on_node(&node_a, &dest_hash_b, Duration::from_secs(5)).await,
        "Node A should have path to B"
    );

    let handle_a = node_a
        .connect(&dest_hash_b, &signing_key)
        .await
        .expect("connect A→B");
    let link_id_a = handle_a.link_id();

    let _link_id_b = wait_for_responder_established_link(&mut events_b, Duration::from_secs(10))
        .await
        .expect("B should establish incoming link");

    assert!(
        wait_for_link_established(&mut events_a, link_id_a, Duration::from_secs(10)).await,
        "Link should establish on A"
    );

    // Stop Node B, this kills its sockets
    node_b.stop().await.ok();

    // Node A should detect the peer loss via InterfaceDown or LinkClosed
    // Peer timeout = 22s, peer job interval = 4s, plus margin
    let detected = wait_for_event(
        &mut events_a,
        Duration::from_secs(35),
        |event| match event {
            NodeEvent::LinkClosed { .. } => Some(()),
            NodeEvent::InterfaceDown { .. } => Some(()),
            _ => None,
        },
    )
    .await;

    node_a.stop().await.ok();
    let _ = std::fs::remove_dir_all(&storage_a);
    let _ = std::fs::remove_dir_all(&storage_b);

    assert!(detected.is_some(), "Node A should detect peer B loss");
}

// =========================================================================
// Test 6: Three-Node Mesh
// =========================================================================

/// Three nodes in the same group all discover each other and receive announces.
///
/// Each node uses a unique data_port so that same-machine peers are disambiguated
/// by (IP, data_port) in the peer map.
#[tokio::test]
async fn test_auto_three_node_mesh() {
    if !have_suitable_nics() {
        return;
    }

    let [config_a, config_b, config_c] = auto_configs_for_test("mesh");
    let storage_a = temp_storage("mesh", "a");
    let storage_b = temp_storage("mesh", "b");
    let storage_c = temp_storage("mesh", "c");

    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_a.clone())
        .add_auto_interface_with_config(config_a)
        .build()
        .await
        .expect("build node A");

    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_b.clone())
        .add_auto_interface_with_config(config_b)
        .build()
        .await
        .expect("build node B");

    let mut node_c = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_c.clone())
        .add_auto_interface_with_config(config_c)
        .build()
        .await
        .expect("build node C");

    node_a.start().await.expect("start A");
    node_b.start().await.expect("start B");
    node_c.start().await.expect("start C");

    let mut events_b = node_b.take_event_receiver().expect("events B");
    let mut events_c = node_c.take_event_receiver().expect("events C");

    // Wait for three-way discovery
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Node A announces
    let identity_a = Identity::generate(&mut rand_core::OsRng);
    let dest_a = Destination::new(
        Some(identity_a),
        Direction::In,
        DestinationType::Single,
        "test",
        &["auto_mesh"],
    )
    .expect("create destination A");
    let dest_hash = *dest_a.hash();
    node_a.register_destination(dest_a);
    node_a
        .announce_destination(&dest_hash, Some(b"mesh"))
        .await
        .expect("announce A");

    // Wait for B and C to receive the announce concurrently
    // (sequential waits can miss events that arrive during the other wait)
    let dest_hash_b = dest_hash;
    let dest_hash_c = dest_hash;
    let (found_b, found_c) = tokio::join!(
        wait_for_event(&mut events_b, Duration::from_secs(10), move |event| {
            if let NodeEvent::AnnounceReceived { announce, .. } = event {
                if *announce.destination_hash() == dest_hash_b {
                    return Some(());
                }
            }
            None
        }),
        wait_for_event(&mut events_c, Duration::from_secs(10), move |event| {
            if let NodeEvent::AnnounceReceived { announce, .. } = event {
                if *announce.destination_hash() == dest_hash_c {
                    return Some(());
                }
            }
            None
        }),
    );

    node_a.stop().await.ok();
    node_b.stop().await.ok();
    node_c.stop().await.ok();
    let _ = std::fs::remove_dir_all(&storage_a);
    let _ = std::fs::remove_dir_all(&storage_b);
    let _ = std::fs::remove_dir_all(&storage_c);

    assert!(found_b.is_some(), "Node B should receive announce from A");
    assert!(found_c.is_some(), "Node C should receive announce from A");
}

// =========================================================================
// Test 7: Group Isolation
// =========================================================================

/// Nodes in different groups (same ports) cannot hear each other.
///
/// With SO_REUSEPORT all three bind the same ports. Isolation comes from
/// different group_id → different multicast address (derived from SHA-256 of
/// group_id). Even if multicast addresses collided, discovery token
/// verification rejects tokens with wrong group_id.
#[tokio::test]
async fn test_auto_group_isolation() {
    if !have_suitable_nics() {
        return;
    }

    // All three share the discovery port on purpose — that is what makes this
    // a test of group isolation rather than of port isolation.
    let discovery_port = free_discovery_port();
    // Nodes A + B: group "test_alpha", different data_ports for same-machine disambiguation
    let config_a = AutoInterfaceConfig {
        group_id: b"test_alpha".to_vec(),
        discovery_port,
        data_port: port_alloc::free_udp_port(),
        multicast_loopback: true,
        ..Default::default()
    };
    let config_b = AutoInterfaceConfig {
        group_id: b"test_alpha".to_vec(),
        discovery_port,
        data_port: port_alloc::free_udp_port(),
        multicast_loopback: true,
        ..Default::default()
    };
    // Node C: group "test_beta", different group → isolation by multicast address + token
    let config_c = AutoInterfaceConfig {
        group_id: b"test_beta".to_vec(),
        discovery_port,
        data_port: port_alloc::free_udp_port(),
        multicast_loopback: true,
        ..Default::default()
    };
    let storage_a = temp_storage("isolation", "a");
    let storage_b = temp_storage("isolation", "b");
    let storage_c = temp_storage("isolation", "c");

    let mut node_a = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_a.clone())
        .add_auto_interface_with_config(config_a)
        .build()
        .await
        .expect("build node A");

    let mut node_b = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_b.clone())
        .add_auto_interface_with_config(config_b)
        .build()
        .await
        .expect("build node B");

    let mut node_c = ReticulumNodeBuilder::new()
        .enable_transport(true)
        .storage_path(storage_c.clone())
        .add_auto_interface_with_config(config_c)
        .build()
        .await
        .expect("build node C");

    node_a.start().await.expect("start A");
    node_b.start().await.expect("start B");
    node_c.start().await.expect("start C");

    let mut events_b = node_b.take_event_receiver().expect("events B");
    let mut events_c = node_c.take_event_receiver().expect("events C");

    // Wait for intra-group discovery
    tokio::time::sleep(Duration::from_secs(4)).await;

    // Node A announces
    let identity_a = Identity::generate(&mut rand_core::OsRng);
    let dest_a = Destination::new(
        Some(identity_a),
        Direction::In,
        DestinationType::Single,
        "test",
        &["auto_isolation"],
    )
    .expect("create destination A");
    let dest_hash = *dest_a.hash();
    node_a.register_destination(dest_a);
    node_a
        .announce_destination(&dest_hash, Some(b"isolation"))
        .await
        .expect("announce A");

    // Node B (same group) should receive the announce
    let found_b = wait_for_event(&mut events_b, Duration::from_secs(5), |event| {
        if let NodeEvent::AnnounceReceived { announce, .. } = event {
            if *announce.destination_hash() == dest_hash {
                return Some(());
            }
        }
        None
    })
    .await;

    // Node C (different group) should NOT receive the announce
    // Wait an additional 5s, if C receives anything, isolation is broken
    let found_c = wait_for_event(&mut events_c, Duration::from_secs(5), |event| {
        if let NodeEvent::AnnounceReceived { announce, .. } = event {
            if *announce.destination_hash() == dest_hash {
                return Some(());
            }
        }
        None
    })
    .await;

    node_a.stop().await.ok();
    node_b.stop().await.ok();
    node_c.stop().await.ok();
    let _ = std::fs::remove_dir_all(&storage_a);
    let _ = std::fs::remove_dir_all(&storage_b);
    let _ = std::fs::remove_dir_all(&storage_c);

    assert!(
        found_b.is_some(),
        "Node B (same group) should receive announce"
    );
    assert!(
        found_c.is_none(),
        "Node C (different group) should NOT receive announce"
    );
}
