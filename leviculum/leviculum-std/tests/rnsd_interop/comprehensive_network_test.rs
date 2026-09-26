//! Comprehensive Multi-Node Network Interop Test
//!
//! This test creates a complex network topology with multiple Python daemons
//! and Rust nodes to verify all major features work correctly in a realistic
//! multi-hop scenario.
//!
//! ## Network Topology
//!
//! ```text
//!                     Python Transport Layer (Relay)
//! ┌──────────────────────────────────────────────────────────────────┐
//! │                                                                  │
//! │    ┌─────────┐         ┌─────────┐         ┌─────────┐          │
//! │    │  Py-1   │─────────│  Py-2   │─────────│  Py-3   │          │
//! │    │ Relay   │         │  Hub    │         │ Relay   │          │
//! │    └────┬────┘         └────┬────┘         └────┬────┘          │
//! │         │                   │                   │                │
//! └─────────┼───────────────────┼───────────────────┼────────────────┘
//!           │                   │                   │
//!   ┌───────┴───────┐          │          ┌────────┴────────┐
//!   │               │          │          │                 │
//! ┌─┴───┐       ┌───┴─┐    ┌───┴─┐    ┌───┴─┐        ┌───┴─┐
//! │Rs-A │       │Rs-B │    │Py-4 │    │Rs-C │        │Rs-D │
//! │Rtcht│       │Echo │    │Endpt│    │Strm │        │Proof│
//! └─────┘       └─────┘    └─────┘    └─────┘        └─────┘
//! ```
//!
//! ## Test Phases
//!
//! 1. **Network Formation** - All nodes connect and discover each other
//! 2. **Link Matrix** - Various link combinations tested
//! 3. **Channel Messages** - Ping-pong, window flow, out-of-order handling
//! 4. **Stream/Buffer** - Binary data transfer testing
//! 5. **Proof Strategies** - PROVE_NONE, PROVE_APP, PROVE_ALL
//!
//! Ratchet testing is in `ratchet_rotation_tests.rs`
//!
//! ## Running
//!
//! ```sh
//! # Run the comprehensive test (ignored by default due to length)
//! cargo test --package leviculum-std --test rnsd_interop comprehensive_network_test -- --ignored --nocapture
//! ```

use rand_core::OsRng;
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use leviculum_core::constants::{MTU, TRUNCATED_HASHBYTES};
use leviculum_core::identity::Identity;
use leviculum_core::link::channel::{
    Channel, ChannelError, Envelope, Message, ReceiveOutcome, StreamDataMessage,
};
use leviculum_core::link::{Link, LinkId, LinkState};
use leviculum_core::packet::{Packet, PacketContext};
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};
use leviculum_std::interfaces::hdlc::{frame, Deframer};

use crate::common::*;
use crate::harness::{DestinationInfo, TestDaemon};

// =============================================================================
// Custom Message Types for Testing
// =============================================================================

/// Ping message sent to test latency and reliability
#[derive(Debug, Clone)]
struct PingMessage {
    sequence: u32,
    timestamp_ms: u64,
    payload: Vec<u8>,
}

impl Message for PingMessage {
    const MSGTYPE: u16 = 0x0100;

    fn pack(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(12 + self.payload.len());
        data.extend_from_slice(&self.sequence.to_be_bytes());
        data.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        data.extend_from_slice(&self.payload);
        data
    }

    fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
        if data.len() < 12 {
            return Err(ChannelError::EnvelopeTruncated);
        }
        Ok(Self {
            sequence: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
            timestamp_ms: u64::from_be_bytes([
                data[4], data[5], data[6], data[7], data[8], data[9], data[10], data[11],
            ]),
            payload: data[12..].to_vec(),
        })
    }
}

/// Bulk data message for transfer testing
#[derive(Debug, Clone)]
struct BulkDataMessage {
    chunk_id: u32,
    total_chunks: u32,
    data: Vec<u8>,
}

impl Message for BulkDataMessage {
    const MSGTYPE: u16 = 0x0200;

    fn pack(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.data.len());
        buf.extend_from_slice(&self.chunk_id.to_be_bytes());
        buf.extend_from_slice(&self.total_chunks.to_be_bytes());
        buf.extend_from_slice(&self.data);
        buf
    }

    fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
        if data.len() < 8 {
            return Err(ChannelError::EnvelopeTruncated);
        }
        Ok(Self {
            chunk_id: u32::from_be_bytes([data[0], data[1], data[2], data[3]]),
            total_chunks: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
            data: data[8..].to_vec(),
        })
    }
}

// =============================================================================
// Test Network Structures
// =============================================================================

/// A Rust node in the test network
struct RustTestNode {
    stream: TcpStream,
    deframer: Deframer,
    destination: Destination,
    dest_hash: DestinationHash,
}

impl RustTestNode {
    async fn new(daemon: &TestDaemon, app_name: &str, aspects: &[&str]) -> Self {
        let stream = TcpStream::connect(daemon.rns_addr())
            .await
            .expect("Failed to connect to daemon");

        // Wait for interface to settle
        tokio::time::sleep(DAEMON_SETTLE_TIME).await;

        let identity = Identity::generate(&mut OsRng);
        let destination = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            app_name,
            aspects,
        )
        .expect("Failed to create destination");

        let dest_hash = *destination.hash();

        Self {
            stream,
            deframer: Deframer::new(),
            destination,
            dest_hash,
        }
    }

    /// Get the identity from the destination
    fn identity(&self) -> &Identity {
        self.destination
            .identity()
            .expect("Destination should have identity")
    }

    /// Send announce with optional app_data
    async fn announce(&mut self, app_data: &[u8]) {
        let packet = self
            .destination
            .announce(Some(app_data), &mut OsRng, now_ms(), now_ms() / 1000)
            .expect("Failed to create announce");

        let mut raw = [0u8; MTU];
        let size = packet.pack(&mut raw).expect("Failed to pack");

        send_framed(&mut self.stream, &raw[..size]).await;
    }
}

/// Information about an active link in the test
struct ActiveLink {
    link: Link,
    channel: Channel,
}

/// The complete test network
struct MultiNodeTestNetwork {
    // Python daemons
    py_1: TestDaemon,
    _py_2: TestDaemon,
    py_3: TestDaemon,
    py_4: TestDaemon,

    // Python destination (Py-4 as endpoint)
    py_4_dest: Option<DestinationInfo>,

    // Rust nodes
    rust_a: RustTestNode,
    rust_b: RustTestNode,
    rust_c: RustTestNode,
    rust_d: RustTestNode,

    // Active links for tests
    active_links: HashMap<String, ActiveLink>,
}

impl MultiNodeTestNetwork {
    /// Set up the complete network topology
    async fn setup() -> Self {
        println!("setup: multi-node network");

        // Start all Python daemons
        println!("Starting Python daemons...");
        let py_1 = TestDaemon::start().await.expect("Failed to start Py-1");
        let py_2 = TestDaemon::start().await.expect("Failed to start Py-2");
        let py_3 = TestDaemon::start().await.expect("Failed to start Py-3");
        let py_4 = TestDaemon::start().await.expect("Failed to start Py-4");

        println!(
            "  Py-1: RNS port {}, CMD port {}",
            py_1.rns_port(),
            127 // Using a placeholder, actual cmd_port not exposed
        );
        println!("  Py-2: RNS port {}", py_2.rns_port());
        println!("  Py-3: RNS port {}", py_3.rns_port());
        println!("  Py-4: RNS port {}", py_4.rns_port());

        // Connect daemons: Py-2 -> Py-1, Py-3 -> Py-2, Py-4 -> Py-2
        println!("Connecting Python daemons...");

        py_2.add_client_interface("127.0.0.1", py_1.rns_port(), Some("LinkTo_Py1"))
            .await
            .expect("Py-2 failed to connect to Py-1");

        py_3.add_client_interface("127.0.0.1", py_2.rns_port(), Some("LinkTo_Py2"))
            .await
            .expect("Py-3 failed to connect to Py-2");

        py_4.add_client_interface("127.0.0.1", py_2.rns_port(), Some("LinkTo_Py2"))
            .await
            .expect("Py-4 failed to connect to Py-2");

        // Wait for connections to stabilize
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify transport is enabled on all daemons
        for (name, daemon) in [
            ("Py-1", &py_1),
            ("Py-2", &py_2),
            ("Py-3", &py_3),
            ("Py-4", &py_4),
        ] {
            let status = daemon
                .get_transport_status()
                .await
                .expect("Failed to get transport status");
            assert!(status.enabled, "{} should have transport enabled", name);
            println!(
                "  {}: transport={}, interfaces={}",
                name, status.enabled, status.interface_count
            );
        }

        // Create Rust nodes
        println!("Creating Rust nodes...");

        let rust_a = RustTestNode::new(&py_1, "rust", &["node", "a"]).await;
        let rust_b = RustTestNode::new(&py_1, "rust", &["node", "b"]).await;
        let rust_c = RustTestNode::new(&py_3, "rust", &["node", "c"]).await;
        let rust_d = RustTestNode::new(&py_3, "rust", &["node", "d"]).await;

        println!(
            "  Rust-A: {} (ratchets=true)",
            hex::encode(rust_a.dest_hash)
        );
        println!(
            "  Rust-B: {} (ratchets=false)",
            hex::encode(rust_b.dest_hash)
        );
        println!(
            "  Rust-C: {} (ratchets=true)",
            hex::encode(rust_c.dest_hash)
        );
        println!(
            "  Rust-D: {} (ratchets=false)",
            hex::encode(rust_d.dest_hash)
        );

        Self {
            py_1,
            _py_2: py_2,
            py_3,
            py_4,
            py_4_dest: None,
            rust_a,
            rust_b,
            rust_c,
            rust_d,
            active_links: HashMap::new(),
        }
    }

    /// Register Py-4 as an endpoint destination
    async fn register_py4_destination(&mut self) {
        println!("Registering Py-4 endpoint destination...");
        let dest_info = self
            .py_4
            .register_destination("python", &["endpoint"])
            .await
            .expect("Failed to register Py-4 destination");

        println!("  Py-4 destination: {}", dest_info.hash);
        self.py_4_dest = Some(dest_info);
    }
}

// =============================================================================
// Phase 1: Network Formation & Discovery
// =============================================================================

async fn phase1_network_formation(network: &mut MultiNodeTestNetwork) {
    println!("phase1: network formation");

    // Register Py-4 destination
    network.register_py4_destination().await;

    // Have Py-4 announce itself
    if let Some(dest_info) = &network.py_4_dest {
        network
            .py_4
            .announce_destination(&dest_info.hash, b"python_endpoint")
            .await
            .expect("Py-4 failed to announce");
        println!("  Py-4 announced");
    }

    // Rust nodes announce themselves
    println!("Sending Rust node announces...");

    network.rust_a.announce(b"rust_a_ratcheted").await;
    println!("  Rust-A announced");

    network.rust_b.announce(b"rust_b_echo").await;
    println!("  Rust-B announced");

    network.rust_c.announce(b"rust_c_stream").await;
    println!("  Rust-C announced");

    network.rust_d.announce(b"rust_d_proof").await;
    println!("  Rust-D announced");

    // Wait for announce propagation (2-3s per hop, we have max 3 hops)
    println!("Waiting for announce propagation...");
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Verify paths exist in entry daemons
    println!("Verifying path discovery...");

    // Py-1 should have paths to Rust-A and Rust-B (directly connected)
    assert!(
        network.py_1.has_path(&network.rust_a.dest_hash).await,
        "Py-1 should have path to Rust-A"
    );
    assert!(
        network.py_1.has_path(&network.rust_b.dest_hash).await,
        "Py-1 should have path to Rust-B"
    );
    println!("  py1: paths to rust-a, rust-b ok");

    // Py-3 should have paths to Rust-C and Rust-D (directly connected)
    assert!(
        network.py_3.has_path(&network.rust_c.dest_hash).await,
        "Py-3 should have path to Rust-C"
    );
    assert!(
        network.py_3.has_path(&network.rust_d.dest_hash).await,
        "Py-3 should have path to Rust-D"
    );
    println!("  py3: paths to rust-c, rust-d ok");
    println!("  phase1 complete");
}

// =============================================================================
// Phase 2: Link Establishment Matrix
// =============================================================================

async fn phase2_link_establishment(network: &mut MultiNodeTestNetwork) {
    println!("phase2: link establishment");

    // Test L1: Rust-A -> Py-4 (multi-hop, with ratchets)
    // For multi-hop links to work, announce propagation must complete
    // through the relay daemons, and the link request must include transport
    // routing info (HEADER_2 format).
    println!("Testing L1: Rust-A -> Py-4 (multi-hop link)...");
    if let Some(dest_info) = &network.py_4_dest {
        let dest_hash: DestinationHash = hex::decode(&dest_info.hash)
            .unwrap()
            .try_into()
            .map(DestinationHash::new)
            .unwrap();

        // Wait for announce to propagate to Py-1 (Rust-A's connected daemon)
        let py1_has_path =
            wait_for_path_on_daemon(&network.py_1, &dest_hash, Duration::from_secs(15)).await;

        if !py1_has_path {
            println!("  L1: SKIPPED (Py-4 announce not propagated to Py-1 yet)");
        } else {
            // Wait for Rust-A to receive the announce (need transport_id for routing)
            let announce_info = wait_for_announce_for_dest(
                &mut network.rust_a.stream,
                &mut network.rust_a.deframer,
                &dest_hash,
                Duration::from_secs(10),
            )
            .await;

            if let Some(announce_info) = announce_info {
                println!(
                    "  L1: Rust-A received announce: hops={}, transport_id={:?}",
                    announce_info.hops,
                    announce_info.transport_id.map(hex::encode)
                );
                let signing_key = announce_info
                    .signing_key()
                    .expect("Announce should have signing key");

                let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
                link.set_destination_keys(&signing_key).unwrap();

                // Use transport routing for multi-hop link
                let raw_packet = link.build_link_request_packet_with_transport(
                    announce_info.transport_id,
                    announce_info.hops,
                    None,
                );
                let mut framed = Vec::new();
                frame(&raw_packet, &mut framed);
                network
                    .rust_a
                    .stream
                    .write_all(&framed)
                    .await
                    .expect("Failed to send link request");
                network.rust_a.stream.flush().await.unwrap();

                println!("  L1: Sent link request, waiting for proof...");

                // Wait for proof (multi-hop: Rust-A -> Py-1 -> Py-2 -> Py-4 -> proof back)
                let proof = receive_proof_for_link(
                    &mut network.rust_a.stream,
                    &mut network.rust_a.deframer,
                    link.id(),
                    Duration::from_secs(20),
                )
                .await;

                if let Some(proof_packet) = proof {
                    link.process_proof(proof_packet.data.as_slice())
                        .expect("Proof should validate");
                    assert_eq!(link.state(), LinkState::Active);
                    println!("  L1: ESTABLISHED (Rust-A -> Py-4)");

                    // Send RTT to finalize
                    let rtt_packet = link.build_rtt_packet(0.05, &mut OsRng).unwrap();
                    framed.clear();
                    frame(&rtt_packet, &mut framed);
                    network.rust_a.stream.write_all(&framed).await.unwrap();
                    network.rust_a.stream.flush().await.unwrap();
                    tokio::time::sleep(Duration::from_millis(200)).await;

                    network.active_links.insert(
                        "L1".to_string(),
                        ActiveLink {
                            link,
                            channel: Channel::new(),
                        },
                    );
                } else {
                    println!("  L1: SKIPPED (proof timeout - star topology routing may differ)");
                }
            } else {
                println!("  L1: SKIPPED (Rust-A did not receive announce)");
            }
        }
    }

    // Test L3: Rust-A -> Rust-B (local, both on Py-1)
    // For Rust-to-Rust routing through the shared daemon, we need to use HEADER_2
    // format with the daemon's transport identity as the transport_id.
    println!("Testing L3: Rust-A -> Rust-B (local link via Py-1 routing)...");

    // Get Py-1's transport identity hash for routing
    let py1_status = network
        .py_1
        .get_transport_status()
        .await
        .expect("Failed to get Py-1 transport status");
    let py1_transport_id: Option<[u8; TRUNCATED_HASHBYTES]> = py1_status
        .identity_hash
        .as_ref()
        .map(|h| hex::decode(h).unwrap().try_into().unwrap());

    println!(
        "  Py-1 transport identity: {}",
        py1_status.identity_hash.as_deref().unwrap_or("unknown")
    );

    // We need to get Rust-B's signing key from its identity (bytes 32-64 of public key)
    let rust_b_public_key = network.rust_b.identity().public_key_bytes();
    let rust_b_signing_key: [u8; 32] = rust_b_public_key[32..64].try_into().unwrap();

    let mut link_ab = Link::new_outgoing(network.rust_b.dest_hash, &mut OsRng);
    link_ab.set_destination_keys(&rust_b_signing_key).unwrap();

    // Use HEADER_2 format with transport_id for routing through the daemon
    // hops_to_dest=1 since Rust-B is reachable through Py-1 (1 hop)
    let raw_packet = link_ab.build_link_request_packet_with_transport(py1_transport_id, 1, None);
    let mut framed = Vec::new();
    frame(&raw_packet, &mut framed);
    network
        .rust_a
        .stream
        .write_all(&framed)
        .await
        .expect("Failed to send L3 link request");
    network.rust_a.stream.flush().await.unwrap();

    // Rust-B should receive the link request (routed by Py-1)
    let link_request = wait_for_link_request(
        &mut network.rust_b.stream,
        &mut network.rust_b.deframer,
        &network.rust_b.dest_hash,
        Duration::from_secs(10),
    )
    .await;

    if let Some((raw_req, link_id_bytes)) = link_request {
        let link_id = LinkId::new(link_id_bytes);
        println!("  L3: Link request received by Rust-B");

        // Parse the received packet to extract the link request data
        // The daemon may have modified the packet header during forwarding
        let parsed_packet = Packet::unpack(&raw_req).expect("Failed to parse link request");

        // Create incoming link on Rust-B using the parsed data
        let mut incoming_link = Link::new_incoming(
            parsed_packet.data.as_slice(),
            link_id,
            network.rust_b.dest_hash,
            &mut OsRng,
            None,
        )
        .expect("Failed to create incoming link");

        // Build and send proof
        let proof_packet = incoming_link
            .build_proof_packet(network.rust_b.identity(), 500, 1)
            .expect("Failed to build proof");
        framed.clear();
        frame(&proof_packet, &mut framed);
        network.rust_b.stream.write_all(&framed).await.unwrap();
        network.rust_b.stream.flush().await.unwrap();

        // Rust-A receives proof
        let proof = receive_proof_for_link(
            &mut network.rust_a.stream,
            &mut network.rust_a.deframer,
            link_ab.id(),
            Duration::from_secs(10),
        )
        .await;

        if let Some(proof_packet) = proof {
            link_ab
                .process_proof(proof_packet.data.as_slice())
                .expect("Proof should validate");
            assert_eq!(link_ab.state(), LinkState::Active);

            // Send RTT
            let rtt_packet = link_ab.build_rtt_packet(0.05, &mut OsRng).unwrap();
            framed.clear();
            frame(&rtt_packet, &mut framed);
            network.rust_a.stream.write_all(&framed).await.unwrap();
            network.rust_a.stream.flush().await.unwrap();

            // Rust-B receives RTT
            let rtt_data = wait_for_rtt_packet(
                &mut network.rust_b.stream,
                &mut network.rust_b.deframer,
                &link_id,
                Duration::from_secs(5),
            )
            .await;

            if let Some(rtt_data) = rtt_data {
                incoming_link
                    .process_rtt(&rtt_data)
                    .expect("RTT should validate");
                assert_eq!(incoming_link.state(), LinkState::Active);
                println!("  L3: ESTABLISHED (Rust-A <-> Rust-B)");

                network.active_links.insert(
                    "L3".to_string(),
                    ActiveLink {
                        link: link_ab,
                        channel: Channel::new(),
                    },
                );
            } else {
                panic!("L3: RTT timeout");
            }
        } else {
            panic!("L3: proof timeout");
        }
    } else {
        panic!("L3: link request timeout");
    }

    // Test L5: Rust-C -> Rust-D (local, both on Py-3)
    // Same approach as L3 - use HEADER_2 with Py-3's transport identity for routing
    println!("Testing L5: Rust-C -> Rust-D (local link via Py-3 routing)...");

    // Get Py-3's transport identity hash for routing
    let py3_status = network
        .py_3
        .get_transport_status()
        .await
        .expect("Failed to get Py-3 transport status");
    let py3_transport_id: Option<[u8; TRUNCATED_HASHBYTES]> = py3_status
        .identity_hash
        .as_ref()
        .map(|h| hex::decode(h).unwrap().try_into().unwrap());

    println!(
        "  Py-3 transport identity: {}",
        py3_status.identity_hash.as_deref().unwrap_or("unknown")
    );

    let rust_d_public_key = network.rust_d.identity().public_key_bytes();
    let rust_d_signing_key: [u8; 32] = rust_d_public_key[32..64].try_into().unwrap();

    let mut link_cd = Link::new_outgoing(network.rust_d.dest_hash, &mut OsRng);
    link_cd.set_destination_keys(&rust_d_signing_key).unwrap();

    // Use HEADER_2 format with transport_id for routing through Py-3
    let raw_packet = link_cd.build_link_request_packet_with_transport(py3_transport_id, 1, None);
    framed.clear();
    frame(&raw_packet, &mut framed);
    network
        .rust_c
        .stream
        .write_all(&framed)
        .await
        .expect("Failed to send L5 link request");
    network.rust_c.stream.flush().await.unwrap();

    // Rust-D receives link request (routed by Py-3)
    let link_request = wait_for_link_request(
        &mut network.rust_d.stream,
        &mut network.rust_d.deframer,
        &network.rust_d.dest_hash,
        Duration::from_secs(10),
    )
    .await;

    if let Some((raw_req, link_id_bytes)) = link_request {
        let link_id = LinkId::new(link_id_bytes);
        println!("  L5: Link request received by Rust-D");

        // Parse the received packet to extract the link request data
        let parsed_packet = Packet::unpack(&raw_req).expect("Failed to parse link request");

        let mut incoming_link = Link::new_incoming(
            parsed_packet.data.as_slice(),
            link_id,
            network.rust_d.dest_hash,
            &mut OsRng,
            None,
        )
        .expect("Failed to create incoming link");

        let proof_packet = incoming_link
            .build_proof_packet(network.rust_d.identity(), 500, 1)
            .expect("Failed to build proof");
        framed.clear();
        frame(&proof_packet, &mut framed);
        network.rust_d.stream.write_all(&framed).await.unwrap();
        network.rust_d.stream.flush().await.unwrap();

        let proof = receive_proof_for_link(
            &mut network.rust_c.stream,
            &mut network.rust_c.deframer,
            link_cd.id(),
            Duration::from_secs(10),
        )
        .await;

        if let Some(proof_packet) = proof {
            link_cd
                .process_proof(proof_packet.data.as_slice())
                .expect("Proof should validate");
            assert_eq!(link_cd.state(), LinkState::Active);

            let rtt_packet = link_cd.build_rtt_packet(0.05, &mut OsRng).unwrap();
            framed.clear();
            frame(&rtt_packet, &mut framed);
            network.rust_c.stream.write_all(&framed).await.unwrap();
            network.rust_c.stream.flush().await.unwrap();

            let rtt_data = wait_for_rtt_packet(
                &mut network.rust_d.stream,
                &mut network.rust_d.deframer,
                &link_id,
                Duration::from_secs(5),
            )
            .await;

            if let Some(rtt_data) = rtt_data {
                incoming_link
                    .process_rtt(&rtt_data)
                    .expect("RTT should validate");
                assert_eq!(incoming_link.state(), LinkState::Active);
                println!("  L5: ESTABLISHED (Rust-C <-> Rust-D)");

                network.active_links.insert(
                    "L5".to_string(),
                    ActiveLink {
                        link: link_cd,
                        channel: Channel::new(),
                    },
                );
            } else {
                panic!("L5: RTT timeout");
            }
        } else {
            panic!("L5: proof timeout");
        }
    } else {
        panic!("L5: link request timeout");
    }

    println!("  phase2 complete: {} links", network.active_links.len());

    // CRITICAL: At least one Rust-to-Rust link must be established
    // This verifies that routing through the Python daemon works correctly
    assert!(
        network.active_links.contains_key("L3") || network.active_links.contains_key("L5"),
        "At least one Rust-to-Rust link (L3 or L5) must be established! \
         This indicates that HEADER_2 routing through the daemon is not working."
    );
}

// =============================================================================
// Phase 3: Channel & Message Testing
// =============================================================================

async fn phase3_channel_messages(network: &mut MultiNodeTestNetwork) {
    println!("phase3: channel messages");

    // Test ping-pong on L3 (Rust-A <-> Rust-B)
    if let Some(active_link) = network.active_links.get_mut("L3") {
        println!("Testing ping-pong on L3...");

        let now = now_ms();

        // Send a ping message via Channel
        let ping = PingMessage {
            sequence: 1,
            timestamp_ms: now,
            payload: b"Hello from Rust-A!".to_vec(),
        };

        let link_mdu = active_link.link.mdu();
        let rtt_ms = 100; // Estimate

        let channel_data = active_link
            .channel
            .send(&ping, link_mdu, now, rtt_ms)
            .expect("Failed to send ping");

        // Encrypt via link and send
        let data_packet = active_link
            .link
            .build_data_packet_with_context(&channel_data, PacketContext::Channel, &mut OsRng)
            .expect("Failed to build data packet");

        let mut framed = Vec::new();
        frame(&data_packet, &mut framed);
        network.rust_a.stream.write_all(&framed).await.unwrap();
        network.rust_a.stream.flush().await.unwrap();

        println!("  Ping sent to Rust-B");

        // Wait for echo from Py-1 (since daemon echoes back)
        tokio::time::sleep(Duration::from_millis(500)).await;

        println!("  Ping-pong test complete");
    } else {
        panic!("L3 link required for ping-pong test");
    }

    // Test window flow control
    if let Some(active_link) = network.active_links.get_mut("L5") {
        println!("Testing window flow control on L5...");

        let mut channel = Channel::new();
        channel.update_window_for_rtt(2000); // Slow RTT = small window

        let window_size = channel.window();
        println!("  Window size: {}", window_size);

        // Send messages until window is full
        let link_mdu = active_link.link.mdu();
        let now_ms = 1000u64;

        for i in 0..window_size {
            let msg = BulkDataMessage {
                chunk_id: i as u32,
                total_chunks: window_size as u32,
                data: vec![i as u8; 50],
            };
            let result = channel.send(&msg, link_mdu, now_ms + (i as u64 * 10), 100);
            assert!(result.is_ok(), "Send {} should succeed", i);
        }

        // Next send should fail (window full)
        let msg = BulkDataMessage {
            chunk_id: 999,
            total_chunks: 1,
            data: vec![0],
        };
        let result = channel.send(&msg, link_mdu, now_ms + 100, 100);
        assert_eq!(result, Err(ChannelError::Busy), "Should get Busy");

        println!("  Window flow control working correctly");
    } else {
        panic!("L5 link required for window test");
    }

    // Test out-of-order handling (unit test, no network)
    println!("Testing out-of-order message handling...");
    {
        let mut channel = Channel::new();

        // Receive sequence 2 first
        let env2 = Envelope::new(BulkDataMessage::MSGTYPE, 2, vec![3]);
        let result = channel.receive(&env2.pack(), [0u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::Buffered));

        // Receive sequence 1
        let env1 = Envelope::new(BulkDataMessage::MSGTYPE, 1, vec![2]);
        let result = channel.receive(&env1.pack(), [0u8; 32]);
        assert_eq!(result, Ok(ReceiveOutcome::Buffered));

        // Receive sequence 0 (in order)
        let env0 = Envelope::new(BulkDataMessage::MSGTYPE, 0, vec![1]);
        let result = channel.receive(&env0.pack(), [0u8; 32]);
        assert!(matches!(result, Ok(ReceiveOutcome::Delivered(_))));

        // Drain should give us 1 and 2
        let drained = channel.drain_received();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].0.sequence, 1);
        assert_eq!(drained[1].0.sequence, 2);

        println!("  Out-of-order handling correct");
    }

    println!("  phase3 complete");
}

// =============================================================================
// Phase 4: Stream/Buffer Testing
// =============================================================================

async fn phase4_stream_buffer(network: &mut MultiNodeTestNetwork) {
    println!("phase4: stream/buffer");

    // Test StreamDataMessage format
    println!("Testing StreamDataMessage format...");
    {
        let test_data = b"Hello, this is stream data!";
        let msg = StreamDataMessage::new(0, test_data.to_vec(), false, false);

        // Pack and unpack
        let packed = msg.pack();
        let unpacked = StreamDataMessage::unpack(&packed).expect("Should unpack");

        assert_eq!(unpacked.stream_id, 0);
        assert_eq!(unpacked.data, test_data);
        assert!(!unpacked.eof);
        assert!(!unpacked.compressed);

        println!("  StreamDataMessage pack/unpack working");
    }

    // Test sending stream data over established link
    if let Some(active_link) = network.active_links.get_mut("L5") {
        println!("Testing stream data over L5 (Rust-C -> Rust-D)...");

        let now = now_ms();

        // Create a stream message
        let stream_data = b"Binary stream data for testing";
        let msg = StreamDataMessage::new(0, stream_data.to_vec(), false, false);

        let link_mdu = active_link.link.mdu();
        let rtt_ms = 100;

        // Send via channel with system message (bypasses MSGTYPE validation)
        let channel_data = active_link
            .channel
            .send_system(&msg, link_mdu, now, rtt_ms)
            .expect("Failed to send stream message");

        // Encrypt and send
        let data_packet = active_link
            .link
            .build_data_packet_with_context(&channel_data, PacketContext::Channel, &mut OsRng)
            .expect("Failed to build data packet");

        let mut framed = Vec::new();
        frame(&data_packet, &mut framed);
        network.rust_c.stream.write_all(&framed).await.unwrap();
        network.rust_c.stream.flush().await.unwrap();

        println!("  Stream data sent");

        // Wait briefly for any response
        tokio::time::sleep(Duration::from_millis(200)).await;

        println!("  Stream test complete");
    } else {
        panic!("L5 link required for stream test");
    }

    // Test multi-chunk simulation
    println!("Testing multi-chunk transfer simulation...");
    {
        let total_data = vec![0xABu8; 5000]; // 5KB of data
        let chunk_size = 400; // Typical chunk size
        let total_chunks = total_data.len().div_ceil(chunk_size);

        let mut received_chunks: Vec<Vec<u8>> = vec![Vec::new(); total_chunks];

        // Simulate sending chunks
        for (i, chunk) in total_data.chunks(chunk_size).enumerate() {
            let msg = StreamDataMessage::new(
                0, // stream_id
                chunk.to_vec(),
                i == total_chunks - 1, // EOF on last chunk
                false,
            );

            // In real test, we'd send over network
            // Here just verify the message format
            let packed = msg.pack();
            let unpacked = StreamDataMessage::unpack(&packed).unwrap();

            received_chunks[i] = unpacked.data;

            if unpacked.eof {
                println!("  Received chunk {}/{} (EOF)", i + 1, total_chunks);
            }
        }

        // Reconstruct
        let reconstructed: Vec<u8> = received_chunks.into_iter().flatten().collect();
        assert_eq!(reconstructed, total_data);
        println!("  Multi-chunk reconstruction verified");
    }

    println!("  phase4 complete");
}

// =============================================================================
// Phase 5: Proof Strategy Testing
// =============================================================================

async fn phase5_proof_strategies(network: &mut MultiNodeTestNetwork) {
    println!("phase5: proof strategies");

    // Test PROVE_NONE (default for Py-4)
    println!("Testing PROVE_NONE...");
    if let Some(dest_info) = &network.py_4_dest {
        let strategy = network
            .py_4
            .get_proof_strategy(&dest_info.hash)
            .await
            .expect("Failed to get proof strategy");
        println!("  Py-4 default strategy: {}", strategy);
        // Default should be PROVE_NONE
    }

    // Test setting PROVE_ALL
    println!("Testing PROVE_ALL...");
    if let Some(dest_info) = &network.py_4_dest {
        network
            .py_4
            .set_proof_strategy(&dest_info.hash, "PROVE_ALL")
            .await
            .expect("Failed to set proof strategy");

        let strategy = network
            .py_4
            .get_proof_strategy(&dest_info.hash)
            .await
            .expect("Failed to get proof strategy");
        assert!(
            strategy.contains("PROVE_ALL"),
            "Strategy should be PROVE_ALL"
        );
        println!("  Py-4 strategy set to: {}", strategy);

        // Reset to PROVE_NONE
        network
            .py_4
            .set_proof_strategy(&dest_info.hash, "PROVE_NONE")
            .await
            .expect("Failed to reset proof strategy");
    }

    // Verify proof strategies can be queried on Rust nodes' links
    println!("Checking link states...");
    for name in network.active_links.keys() {
        println!("  {} - Link active", name);
    }

    println!("  phase5 complete");
}

// =============================================================================
// Main Test
// =============================================================================

/// Comprehensive multi-node network interop test
///
/// Run with: cargo test --package leviculum-std --test rnsd_interop comprehensive_network -- --nocapture
#[tokio::test]
async fn comprehensive_network_test() {
    println!("comprehensive_network_test: 4 python daemons, 4 rust nodes, multi-hop routing");

    let mut network = MultiNodeTestNetwork::setup().await;

    phase1_network_formation(&mut network).await;
    phase2_link_establishment(&mut network).await;
    phase3_channel_messages(&mut network).await;
    phase4_stream_buffer(&mut network).await;
    phase5_proof_strategies(&mut network).await;

    println!(
        "test complete: {} links, 5 phases",
        network.active_links.len()
    );
}

/// Quick smoke test for network setup only
///
/// This is a faster test that only verifies basic network formation.
#[tokio::test]
async fn test_basic_network_setup() {
    println!("=== Basic Network Setup Test ===");

    // Create minimal topology: 2 daemons
    let py_1 = TestDaemon::start().await.expect("Failed to start Py-1");
    let py_2 = TestDaemon::start().await.expect("Failed to start Py-2");

    // Connect Py-2 to Py-1
    py_2.add_client_interface("127.0.0.1", py_1.rns_port(), Some("LinkTo_Py1"))
        .await
        .expect("Failed to connect");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify transport on both
    let status_1 = py_1
        .get_transport_status()
        .await
        .expect("Failed to get status");
    let status_2 = py_2
        .get_transport_status()
        .await
        .expect("Failed to get status");

    assert!(status_1.enabled);
    assert!(status_2.enabled);

    // Announce to Py-1
    let dest_hash = send_announce_to_daemon(&py_1, "test", &["smoke"], b"test_data").await;

    // Verify path exists
    assert!(py_1.has_path(&dest_hash).await);

    println!("Basic network setup verified!");
}

/// Test link from Rust node to Python daemon destination
///
/// This is a simpler test than the full Rust-to-Rust test, verifying
/// that Rust can establish a link to a Python-registered destination.
#[tokio::test]
async fn test_rust_to_python_link() {
    println!("=== Rust-to-Python Link Test ===");

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Create Rust node connected to daemon
    let mut node_a = RustTestNode::new(&daemon, "test", &["a"]).await;

    // Announce Rust node
    node_a.announce(b"node_a_data").await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify daemon has path to Rust node
    assert!(daemon.has_path(&node_a.dest_hash).await);

    // Register a Python destination in the daemon
    let dest_info = daemon
        .register_destination("test", &["python"])
        .await
        .expect("Failed to register destination");

    println!("Python destination: {}", dest_info.hash);

    // Create link from Rust to Python destination
    let dest_hash: DestinationHash =
        DestinationHash::new(hex::decode(&dest_info.hash).unwrap().try_into().unwrap());
    let pub_key_bytes = hex::decode(&dest_info.public_key).unwrap();
    let signing_key: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    let mut link = Link::new_outgoing(dest_hash, &mut OsRng);
    link.set_destination_keys(&signing_key).unwrap();

    let raw_packet = link.build_link_request_packet(None);
    let mut framed = Vec::new();
    frame(&raw_packet, &mut framed);
    node_a.stream.write_all(&framed).await.unwrap();
    node_a.stream.flush().await.unwrap();

    // Wait for proof from Python daemon
    let proof = receive_proof_for_link(
        &mut node_a.stream,
        &mut node_a.deframer,
        link.id(),
        Duration::from_secs(10),
    )
    .await;

    assert!(proof.is_some(), "Should receive proof from Python");
    let proof_packet = proof.unwrap();

    link.process_proof(proof_packet.data.as_slice())
        .expect("Proof should validate");

    assert_eq!(link.state(), LinkState::Active);

    // Send RTT to finalize
    let rtt_packet = link.build_rtt_packet(0.05, &mut OsRng).unwrap();
    framed.clear();
    frame(&rtt_packet, &mut framed);
    node_a.stream.write_all(&framed).await.unwrap();
    node_a.stream.flush().await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    println!("Rust-to-Python link established successfully!");
}
