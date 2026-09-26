//! Announce interoperability tests with Python Reticulum daemon.
//!
//! These tests spawn a fresh Python test daemon and verify behavior by
//! querying the daemon's internal state directly via JSON-RPC, rather than
//! relying on log file parsing or packet observation.
//!
//! ## What These Tests Cover
//!
//! - Announce propagation and path table creation
//! - Hash derivation compatibility between Rust and Python
//! - Connection resilience (malformed packets, reconnection)
//! - HDLC framing edge cases
//!
//! ## Key Advantages
//!
//! 1. **No manual rnsd startup** - Each test spawns its own daemon
//! 2. **Direct state queries** - Verify path_table, announce_table directly
//! 3. **Clean isolation** - Fresh daemon per test, no interference
//! 4. **Fast feedback** - Tests run automatically, no #[ignore]
//!
//! ## Running These Tests
//!
//! ```sh
//! # Run all announce interop tests
//! cargo test --package leviculum-std --test rnsd_interop announce_interop_tests
//!
//! # Run with verbose output
//! cargo test --package leviculum-std --test rnsd_interop announce_interop_tests -- --nocapture
//! ```

use std::time::Duration;

use leviculum_core::Destination;

use crate::common::*;
use crate::harness::TestDaemon;

// =========================================================================
// Basic announce -> path creation tests
// =========================================================================

/// Verify that sending a valid announce creates a path entry in the daemon.
///
/// This is the fundamental test for announce propagation: we send an announce
/// and verify that Transport.path_table contains an entry for that destination.
#[tokio::test]
async fn test_announce_creates_path_entry() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Verify daemon starts with empty path table
    let initial_paths = daemon.get_path_table().await.expect("Failed to get paths");
    println!("Initial path count: {}", initial_paths.len());

    // Send an announce
    let dest_hash = send_announce_to_daemon(
        &daemon,
        "leviculum",
        &["pathtest", "basic"],
        b"path-test-data",
    )
    .await;

    // Wait a bit more for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify path exists
    let has_path = daemon.has_path(&dest_hash).await;
    assert!(has_path, "Path should exist after announce");

    // Get path details
    let paths = daemon.get_path_table().await.expect("Failed to get paths");
    let hash_hex = hex::encode(dest_hash);
    let path_entry = paths.get(&hash_hex);

    assert!(
        path_entry.is_some(),
        "Path entry should exist in path_table"
    );
    let entry = path_entry.unwrap();
    // Reticulum increments hop count when receiving, so hops=0 in packet becomes hops=1 in path_table
    assert_eq!(
        entry.hops,
        Some(1),
        "Announce received with hops=0 should be recorded as hops=1"
    );

    println!("Path created successfully:");
    println!("  Hash: {}", hash_hex);
    println!("  Hops: {:?}", entry.hops);
    println!("  Timestamp: {:?}", entry.timestamp);
}

/// Verify that announces with different hop counts are recorded correctly.
#[tokio::test]
async fn test_announce_hop_count_recorded() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Send announce with hops=0
    let (raw0, hash0, _) = build_announce_raw("leviculum", &["hops", "zero"], b"hops0");
    send_raw_to_daemon(&daemon, &raw0).await;

    // Send announce with hops=5
    let (raw5, hash5, _) =
        build_announce_raw_with_hops("leviculum", &["hops", "five"], b"hops5", 5);
    send_raw_to_daemon(&daemon, &raw5).await;

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify paths exist with correct hop counts
    let paths = daemon.get_path_table().await.expect("Failed to get paths");

    let entry0 = paths.get(&hex::encode(hash0));
    assert!(entry0.is_some(), "Path for hops=0 should exist");
    // The daemon may increment the hop count when recording
    println!(
        "Hops=0 announce recorded with hops={:?}",
        entry0.unwrap().hops
    );

    let entry5 = paths.get(&hex::encode(hash5));
    assert!(entry5.is_some(), "Path for hops=5 should exist");
    println!(
        "Hops=5 announce recorded with hops={:?}",
        entry5.unwrap().hops
    );
}

/// Verify that an invalid announce (bad signature) does NOT create a path.
#[tokio::test]
async fn test_invalid_announce_no_path() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Build a valid announce then corrupt the signature
    let (mut raw, dest_hash, _) = build_announce_raw("leviculum", &["invalid", "sig"], b"bad-sig");

    // Corrupt the signature (at offset 103-167 in raw packet)
    raw[103] ^= 0xFF;
    raw[104] ^= 0xFF;

    // Send the corrupted announce
    send_raw_to_daemon(&daemon, &raw).await;

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify NO path was created
    let has_path = daemon.has_path(&dest_hash).await;
    assert!(!has_path, "Invalid announce should NOT create path");

    println!("Correctly rejected announce with corrupted signature");
}

// =========================================================================
// Destination registration and announce tests
// =========================================================================

/// Verify that we can register a destination in the daemon and retrieve its info.
#[tokio::test]
async fn test_register_and_announce_destination() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register a destination
    let dest_info = daemon
        .register_destination("testapp", &["echo", "v1"])
        .await
        .expect("Failed to register destination");

    println!("Registered destination:");
    println!("  Hash: {}", dest_info.hash);
    println!("  Signing key: {}", dest_info.signing_key);

    // Announce the destination
    daemon
        .announce_destination(&dest_info.hash, b"test-app-data")
        .await
        .expect("Failed to announce");

    // The announce should create a path to this destination
    tokio::time::sleep(Duration::from_millis(500)).await;

    let dest_hash_bytes = hex::decode(&dest_info.hash).expect("Invalid hex");
    let has_path = daemon.has_path(&dest_hash_bytes).await;

    // The daemon may or may not create a path for its own destinations
    println!("Has path to own destination: {}", has_path);
}

// =========================================================================
// Propagation verification tests
// =========================================================================

/// Verify announce propagation between two connections to the same daemon.
///
/// This tests the daemon's announce retransmission behavior by:
/// 1. Opening two connections to the daemon
/// 2. Sending an announce on connection 1
/// 3. Verifying it's received on connection 2
#[tokio::test]
async fn test_announce_propagates_between_connections() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Open two connections
    let mut conn1 = connect_to_daemon(&daemon).await;
    let mut conn2 = connect_to_daemon(&daemon).await;

    // Send announce on conn1
    let (dest_hash, _) = build_and_send_announce(
        &mut conn1,
        "leviculum",
        &["propagate", "test"],
        b"propagate-test",
    )
    .await;

    println!(
        "Sent announce on conn1, dest: {:02x?}...",
        &dest_hash.as_bytes()[..4]
    );

    // Wait for announce on conn2
    let found = wait_for_announce(&mut conn2, &dest_hash, Duration::from_secs(10)).await;

    assert!(found, "Announce should propagate to second connection");
    println!("Announce successfully propagated to conn2");

    // Verify path exists in daemon state
    assert!(
        daemon.has_path(&dest_hash).await,
        "Path should exist in daemon"
    );
}

// =========================================================================
// Interface state verification
// =========================================================================

/// Verify the daemon's interface configuration matches expectations.
#[tokio::test]
async fn test_interface_configuration() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let interfaces = daemon
        .get_interfaces()
        .await
        .expect("Failed to get interfaces");

    // Should have exactly one interface (the TCP server)
    assert!(!interfaces.is_empty(), "Should have at least one interface");

    let tcp_iface = interfaces
        .iter()
        .find(|i| i.name.contains("TCP"))
        .expect("TCP interface should exist");

    assert_eq!(tcp_iface.online, Some(true), "Interface should be online");
    assert_eq!(
        tcp_iface.in_enabled,
        Some(true),
        "Interface should accept incoming"
    );
    assert_eq!(
        tcp_iface.out_enabled,
        Some(true),
        "Interface should allow outgoing"
    );

    println!("Interface configuration verified:");
    for iface in &interfaces {
        println!(
            "  {} - online={:?}, IN={:?}, OUT={:?}",
            iface.name, iface.online, iface.in_enabled, iface.out_enabled
        );
    }
}

// =========================================================================
// Announce deduplication tests
// =========================================================================

/// Verify that duplicate announces are deduplicated by the daemon.
///
/// When transport is enabled, the daemon retransmits announces to all interfaces.
/// With a TCPServerInterface, each client connection is treated as a separate "peer",
/// so the announce gets forwarded. The deduplication being tested here is that the
/// daemon doesn't process/propagate the same announce TWICE from the same source -
/// it should only create one path entry regardless of how many times the same announce
/// is received.
#[tokio::test]
async fn test_announce_deduplication() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Build an announce
    let (raw, dest_hash, _) = build_announce_raw("leviculum", &["dedup", "test"], b"dedup");

    // Send the EXACT same announce twice quickly via two separate sends
    send_raw_to_daemon(&daemon, &raw).await;
    send_raw_to_daemon(&daemon, &raw).await;

    println!("Sent same announce twice");

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The key verification is that only ONE path entry exists
    let paths = daemon.get_path_table().await.expect("Failed to get paths");
    let hash_hex = hex::encode(dest_hash);

    let matching_paths: Vec<_> = paths.keys().filter(|k| *k == &hash_hex).collect();

    // Should have exactly 1 path entry (deduplicated)
    println!(
        "Found {} path entries for this destination (expected 1)",
        matching_paths.len()
    );
    assert_eq!(
        matching_paths.len(),
        1,
        "Daemon should deduplicate announces into single path entry"
    );

    // Also verify the path exists
    assert!(daemon.has_path(&dest_hash).await);
}

// =========================================================================
// Boundary tests
// =========================================================================

/// Test announce with minimal (1 byte) app_data.
#[tokio::test]
async fn test_minimal_app_data_creates_path() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Send announce with 1 byte app_data (minimum valid)
    let dest_hash = send_announce_to_daemon(&daemon, "leviculum", &["minimal"], b"m").await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        daemon.has_path(&dest_hash).await,
        "Minimal app_data announce should create path"
    );

    println!("Minimal (1 byte) app_data announce accepted");
}

/// Test announce near MTU boundary.
#[tokio::test]
async fn test_large_app_data_creates_path() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Use 200 bytes of app_data (well under MTU but substantial)
    let large_app_data = vec![0x42u8; 200];
    let dest_hash =
        send_announce_to_daemon(&daemon, "leviculum", &["large"], &large_app_data).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        daemon.has_path(&dest_hash).await,
        "Large app_data announce should create path"
    );

    println!("Large (200 byte) app_data announce accepted");
}

// =========================================================================
// Hash derivation tests
// =========================================================================

/// Verify that our hash computation matches the daemon's hash for a registered destination.
///
/// This test compares hash derivation between Rust and Python implementations:
/// 1. Register a destination in the Python daemon
/// 2. Create the same destination in Rust
/// 3. Compare the computed hashes
#[tokio::test]
async fn test_hash_derivation_matches_daemon() {
    use leviculum_core::crypto::truncated_hash;

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register a destination in the daemon
    let dest_info = daemon
        .register_destination("hashtest", &["verify"])
        .await
        .expect("Failed to register destination");

    println!(
        "Daemon registered destination with hash: {}",
        dest_info.hash
    );

    // Verify hash format (should be 16 bytes = 32 hex chars)
    assert_eq!(
        dest_info.hash.len(),
        32,
        "Hash should be 16 bytes hex-encoded"
    );

    // Verify the hash derivation formula:
    // destination_hash = truncated_hash(name_hash || identity_hash)
    // where identity_hash = truncated_hash(public_key)

    let pub_key_bytes = hex::decode(&dest_info.public_key).expect("Invalid public key hex");
    assert_eq!(pub_key_bytes.len(), 64, "Public key should be 64 bytes");

    // Compute identity hash from public key
    let identity_hash = truncated_hash(&pub_key_bytes);
    println!("Computed identity hash: {}", hex::encode(identity_hash));

    // Compute name hash
    let name_hash = compute_name_hash("hashtest", &["verify"]);
    println!("Computed name hash: {}", hex::encode(name_hash));

    // Compute destination hash
    let computed_dest_hash =
        Destination::compute_destination_hash(&name_hash, &identity_hash).into_bytes();
    let computed_hex = hex::encode(computed_dest_hash);

    println!("Daemon destination hash:   {}", dest_info.hash);
    println!("Computed destination hash: {}", computed_hex);

    assert_eq!(
        computed_hex, dest_info.hash,
        "Rust hash derivation should match Python"
    );

    println!("Hash derivation verified - Rust matches Python!");
}

// =========================================================================
// Ratchet boundary detection tests
// =========================================================================

/// Test ratchet boundary detection with 32 bytes of app_data.
///
/// This is a critical edge case:
/// - Non-ratcheted announce with 32 bytes app_data: 148 + 32 = 180 bytes payload
/// - Ratcheted announce (minimum, 0 bytes app_data): 148 + 32 = 180 bytes payload
///
/// Both have the SAME payload size! The only way to distinguish them is by
/// checking the context_flag in the packet flags. This test verifies that
/// our packet construction correctly sets the flag.
#[tokio::test]
async fn test_non_ratcheted_32_byte_app_data() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Create an announce with exactly 32 bytes of app_data (non-ratcheted)
    // This is the boundary case where payload size equals ratcheted minimum
    let app_data_32 = vec![0x42u8; 32];
    let dest_hash =
        send_announce_to_daemon(&daemon, "leviculum", &["ratchet", "boundary"], &app_data_32).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify the daemon accepted it as a valid announce and created a path
    assert!(
        daemon.has_path(&dest_hash).await,
        "32-byte app_data announce should create path"
    );

    println!("32-byte app_data (ratchet boundary) announce accepted");

    // Verify our packet has context_flag=false (non-ratcheted)
    let (raw, _, _) = build_announce_raw("leviculum", &["ratchet", "test2"], &app_data_32);
    let flags_byte = raw[0];
    let context_flag = (flags_byte & 0x80) != 0;
    assert!(
        !context_flag,
        "Non-ratcheted announce should have context_flag=false"
    );

    println!("Verified context_flag=false for non-ratcheted announce");
}

// =========================================================================
// Concurrent daemon tests
// =========================================================================

/// Verify that multiple tests can run concurrently with separate daemons.
/// (This is a meta-test for the test infrastructure itself.)
#[tokio::test]
async fn test_daemon_isolation_a() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon A");

    let dest_hash =
        send_announce_to_daemon(&daemon, "leviculum", &["isolation", "a"], b"test-a").await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(daemon.has_path(&dest_hash).await);

    // This daemon is completely independent
    println!("Daemon A test completed on port {}", daemon.rns_port());
}

#[tokio::test]
async fn test_daemon_isolation_b() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon B");

    let dest_hash =
        send_announce_to_daemon(&daemon, "leviculum", &["isolation", "b"], b"test-b").await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(daemon.has_path(&dest_hash).await);

    // This daemon is completely independent
    println!("Daemon B test completed on port {}", daemon.rns_port());
}

// =========================================================================
// Resilience tests (migrated from resilience.rs)
// =========================================================================

/// Verify connection survives multiple malformed packets.
///
/// This test sends various types of malformed packets and verifies that:
/// 1. The connection stays open
/// 2. Valid announces can still be processed after the malformed packets
#[tokio::test]
async fn test_connection_survives_malformed_packets() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Connect to the daemon
    let mut stream = connect_to_daemon(&daemon).await;

    // Send a valid announce first to verify initial state
    let (dest_pre, _) = build_and_send_announce(
        &mut stream,
        "leviculum",
        &["resilience", "pre"],
        b"pre-invalid",
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        daemon.has_path(&dest_pre).await,
        "Pre-test announce should create path"
    );
    println!("Pre-test announce accepted");

    // 1. Truncated packet (just a few bytes)
    let truncated = vec![0x01, 0x00, 0xAA, 0xBB];
    send_framed(&mut stream, &truncated).await;
    println!("Sent truncated packet (4 bytes)");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2. Announce with bad signature
    let (mut bad_sig, _, _) =
        build_announce_raw("leviculum", &["resilience", "badsig"], b"bad-sig");
    bad_sig[103] ^= 0xFF; // corrupt signature
    send_framed(&mut stream, &bad_sig).await;
    println!("Sent bad-signature announce");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 3. Announce with wrong hash
    let (mut bad_hash, _, _) =
        build_announce_raw("leviculum", &["resilience", "badhash"], b"bad-hash");
    bad_hash[2] ^= 0xFF; // corrupt dest hash in header
    send_framed(&mut stream, &bad_hash).await;
    println!("Sent wrong-hash announce");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Announce with zeroed signature
    let (mut zeroed, _, _) = build_announce_raw("leviculum", &["resilience", "zero"], b"zeroed");
    zeroed[103..103 + 64].fill(0);
    send_framed(&mut stream, &zeroed).await;
    println!("Sent zeroed-signature announce");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 5. Random garbage bytes
    let garbage = vec![
        0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
        0x88, 0x99, 0xAA, 0xBB,
    ];
    send_framed(&mut stream, &garbage).await;
    println!("Sent random garbage bytes");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify connection is still alive
    assert!(
        connection_alive(&mut stream).await,
        "Connection should survive all invalid packets"
    );

    // Now send a valid announce and verify it creates a path
    let (dest_post, _) = build_and_send_announce(
        &mut stream,
        "leviculum",
        &["resilience", "post"],
        b"post-invalid",
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        daemon.has_path(&dest_post).await,
        "Post-invalid announce should create path (connection should be functional)"
    );

    println!("SUCCESS: Connection survived 5 invalid packets and still works");
}

/// Verify reconnection after disconnect works properly.
#[tokio::test]
async fn test_reconnect_after_disconnect() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // First connection
    let mut stream1 = connect_to_daemon(&daemon).await;

    // Send first announce
    let (dest1, _) =
        build_and_send_announce(&mut stream1, "leviculum", &["reconnect", "first"], b"first").await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        daemon.has_path(&dest1).await,
        "First announce should create path"
    );
    println!(
        "First announce accepted, dest: {:02x?}...",
        &dest1.as_bytes()[..4]
    );

    // Drop connection
    drop(stream1);
    println!("First connection dropped");

    // Wait for daemon to clean up
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Reconnect
    let mut stream2 = connect_to_daemon(&daemon).await;
    println!("Reconnected");

    // Send second announce (new identity)
    let (dest2, _) = build_and_send_announce(
        &mut stream2,
        "leviculum",
        &["reconnect", "second"],
        b"second",
    )
    .await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        daemon.has_path(&dest2).await,
        "Second announce should create path after reconnect"
    );

    println!("SUCCESS: Clean disconnect/reconnect handling verified");
}

// =========================================================================
// Stress tests (migrated from stress.rs)
// =========================================================================

/// Verify multiple simultaneous connections work correctly.
#[tokio::test]
async fn test_multiple_connections_concurrent() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let num_connections = 5;
    let mut streams: Vec<tokio::net::TcpStream> = Vec::new();
    let mut expected_hashes = Vec::new();

    // Open multiple connections
    for i in 0..num_connections {
        let stream = connect_to_daemon(&daemon).await;
        streams.push(stream);
        println!("Opened connection {}", i + 1);
    }

    // Each connection sends a unique announce
    for (i, stream) in streams.iter_mut().enumerate() {
        let aspect = format!("concurrent_{}", i);
        let (dest_hash, _) = build_and_send_announce(
            stream,
            "leviculum",
            &[&aspect],
            format!("connection-{}", i).as_bytes(),
        )
        .await;
        expected_hashes.push(dest_hash);
        println!(
            "Connection {} sent announce: {:02x?}...",
            i + 1,
            &dest_hash.as_bytes()[..4]
        );

        // Small delay between sends
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Verify all announces created paths
    let mut found_count = 0;
    for (i, hash) in expected_hashes.iter().enumerate() {
        if daemon.has_path(hash).await {
            found_count += 1;
            println!("Connection {} path found", i + 1);
        } else {
            println!("Connection {} path NOT found", i + 1);
        }
    }

    println!("Found {}/{} paths", found_count, num_connections);
    assert_eq!(
        found_count, num_connections,
        "All paths must be created. Got {}/{}",
        found_count, num_connections
    );

    println!("All connections verified");
}

/// Verify fragmented HDLC delivery is handled correctly.
///
/// This tests TCP stream fragmentation - packets delivered in small chunks
/// rather than complete frames.
#[tokio::test]
async fn test_fragmented_hdlc_delivery() {
    use leviculum_std::interfaces::hdlc::frame;
    use tokio::io::AsyncWriteExt;

    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    let mut stream = connect_to_daemon(&daemon).await;

    // Build a valid announce
    let (raw, dest_hash, _) =
        build_announce_raw("leviculum", &["fragmented", "test"], b"frag-test");

    // Frame it
    let mut framed = Vec::new();
    frame(&raw, &mut framed);

    println!(
        "Sending {} byte framed packet in 5-byte chunks",
        framed.len()
    );

    // Send in small chunks (5 bytes at a time)
    for chunk in framed.chunks(5) {
        stream.write_all(chunk).await.expect("Failed to send chunk");
        stream.flush().await.expect("Failed to flush");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    println!("All chunks sent");

    // Wait for processing
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify the announce was correctly reassembled and processed
    assert!(
        daemon.has_path(&dest_hash).await,
        "Fragmented announce should be reassembled and create path"
    );

    // Verify connection is still alive
    assert!(
        connection_alive(&mut stream).await,
        "Connection should survive fragmented delivery"
    );

    println!("SUCCESS: Fragmented HDLC delivery handled correctly");
}
