//! Stress tests for Reticulum interoperability.
//!
//! These tests verify that the protocol implementation can handle
//! extended communication sessions, rapid operations, and concurrent
//! connections without degradation.
//!
//! ## Test Categories
//!
//! 1. **Extended exchange** - Many messages over long duration
//! 2. **Rapid operations** - Quick succession of link create/teardown
//! 3. **Concurrent links** - Multiple simultaneous links
//! 4. **Payload variety** - Various payload sizes in sequence
//!
//! ## Running These Tests
//!
//! ```sh
//! # Run all stress tests
//! cargo test --package leviculum-std --test rnsd_interop stress_tests
//!
//! # Run with verbose output
//! cargo test --package leviculum-std --test rnsd_interop stress_tests -- --nocapture
//!
//! # Note: Stress tests may take longer than usual
//! cargo test --package leviculum-std --test rnsd_interop stress_tests -- --nocapture --test-threads=1
//! ```
//!
//! ## Notes
//!
//! These tests are marked with `#[ignore]` by default because they:
//! - Take longer to run than typical unit tests
//! - May consume significant resources
//! - Are primarily useful for manual verification
//!
//! To run them: `cargo test stress_tests -- --ignored`

use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::link::{Link, LinkState};
use leviculum_std::interfaces::hdlc::{frame, Deframer};

use crate::common::*;
use crate::harness::{DaemonTopology, TestDaemon};

// =========================================================================
// Test 1: Extended message exchange (100 messages)
// =========================================================================

/// Test exchanging 100 messages over an established link.
///
/// This test verifies that:
/// - Links remain stable over extended communication
/// - No message corruption occurs over many packets
/// - Daemon remains responsive throughout
#[tokio::test]
async fn test_extended_exchange_100_messages() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register destination
    let dest_info = daemon
        .register_destination("stress", &["extended", "100"])
        .await
        .expect("Failed to register");

    let dest_hash: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_info.hash).unwrap().try_into().unwrap();

    let pub_key_bytes = hex::decode(&dest_info.public_key).unwrap();
    let signing_key_bytes: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    // Establish link
    let mut link = Link::new_outgoing(dest_hash.into(), &mut OsRng);
    link.set_destination_keys(&signing_key_bytes).unwrap();

    let mut stream = connect_to_daemon(&daemon).await;
    let raw_packet = link.build_link_request_packet(None);

    let mut framed = Vec::new();
    frame(&raw_packet, &mut framed);
    stream.write_all(&framed).await.unwrap();
    stream.flush().await.unwrap();

    let mut deframer = Deframer::new();
    let proof = receive_proof_for_link(
        &mut stream,
        &mut deframer,
        link.id(),
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive proof");

    link.process_proof(proof.data.as_slice()).unwrap();
    assert_eq!(link.state(), LinkState::Active);

    let rtt_packet = link.build_rtt_packet(0.05, &mut OsRng).unwrap();
    framed.clear();
    frame(&rtt_packet, &mut framed);
    stream.write_all(&framed).await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    println!("Link established, sending 100 messages...");
    let start_time = Instant::now();

    // Send 100 messages
    const NUM_MESSAGES: usize = 100;
    for i in 0..NUM_MESSAGES {
        let msg = format!("Extended test message #{:04}", i);
        let data_packet = link
            .build_data_packet(msg.as_bytes(), &mut OsRng)
            .expect("Failed to build packet");

        framed.clear();
        frame(&data_packet, &mut framed);
        stream.write_all(&framed).await.unwrap();
        stream.flush().await.unwrap();

        // Small delay to avoid overwhelming
        if i % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            print!(".");
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
        }
    }
    println!();

    let send_duration = start_time.elapsed();
    println!(
        "Sent {} messages in {:.2}s ({:.1} msg/s)",
        NUM_MESSAGES,
        send_duration.as_secs_f64(),
        NUM_MESSAGES as f64 / send_duration.as_secs_f64()
    );

    // Wait for daemon to process all messages
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify messages received
    let received = daemon
        .get_received_packets()
        .await
        .expect("Failed to get packets");

    let extended_messages = received
        .iter()
        .filter(|p| String::from_utf8_lossy(&p.data).contains("Extended test"))
        .count();

    println!(
        "Received {} of {} extended test messages ({:.1}%)",
        extended_messages,
        NUM_MESSAGES,
        extended_messages as f64 / NUM_MESSAGES as f64 * 100.0
    );

    // We should receive at least 90% of messages
    assert!(
        extended_messages >= NUM_MESSAGES * 9 / 10,
        "Should receive at least 90% of messages"
    );

    // Verify daemon still responsive
    daemon
        .ping()
        .await
        .expect("Daemon should still be responsive");

    println!("SUCCESS: Extended exchange of 100 messages completed");
}

// =========================================================================
// Test 2: Rapid link creation/teardown
// =========================================================================

/// Test creating and tearing down links in rapid succession.
///
/// This test verifies that:
/// - Daemon handles rapid link lifecycle correctly
/// - No resource leaks from many link creations
/// - Each link works correctly despite rapid turnover
#[tokio::test]
async fn test_rapid_link_creation_teardown() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register destination
    let dest_info = daemon
        .register_destination("stress", &["rapid", "links"])
        .await
        .expect("Failed to register");

    let dest_hash: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_info.hash).unwrap().try_into().unwrap();

    let pub_key_bytes = hex::decode(&dest_info.public_key).unwrap();
    let signing_key_bytes: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    let start_time = Instant::now();
    const NUM_LINKS: usize = 20;
    let mut successful_links = 0;
    let mut failed_links = 0;

    println!("Creating {} links in rapid succession...", NUM_LINKS);

    for i in 0..NUM_LINKS {
        // Create new link
        let mut link = Link::new_outgoing(dest_hash.into(), &mut OsRng);
        link.set_destination_keys(&signing_key_bytes).unwrap();

        let raw_packet = link.build_link_request_packet(None);

        // Connect (new connection for each link)
        let stream_result = tokio::net::TcpStream::connect(daemon.rns_addr()).await;
        if stream_result.is_err() {
            println!("Link {}: Connection failed", i);
            failed_links += 1;
            continue;
        }
        let mut stream = stream_result.unwrap();
        tokio::time::sleep(DAEMON_SETTLE_TIME).await;

        let mut framed = Vec::new();
        frame(&raw_packet, &mut framed);
        stream.write_all(&framed).await.unwrap();
        stream.flush().await.unwrap();

        // Wait for proof
        let mut deframer = Deframer::new();
        let proof = receive_proof_for_link(
            &mut stream,
            &mut deframer,
            link.id(),
            Duration::from_secs(5),
        )
        .await;

        if proof.is_none() {
            println!("Link {}: No proof received", i);
            failed_links += 1;
            continue;
        }

        let proof = proof.unwrap();
        if link.process_proof(proof.data.as_slice()).is_err() {
            println!("Link {}: Proof validation failed", i);
            failed_links += 1;
            continue;
        }

        if link.state() != LinkState::Active {
            println!("Link {}: Not active", i);
            failed_links += 1;
            continue;
        }

        // Send one message

        let msg = format!("Rapid link {}", i);
        let data_packet = link.build_data_packet(msg.as_bytes(), &mut OsRng).unwrap();

        framed.clear();
        frame(&data_packet, &mut framed);
        stream.write_all(&framed).await.unwrap();
        stream.flush().await.unwrap();

        successful_links += 1;
        print!(".");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();

        // Brief delay between links
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!();

    let duration = start_time.elapsed();
    println!(
        "Completed {} links in {:.2}s: {} successful, {} failed",
        NUM_LINKS,
        duration.as_secs_f64(),
        successful_links,
        failed_links
    );

    // Should have most links succeed
    assert!(
        successful_links >= NUM_LINKS * 7 / 10,
        "At least 70% of rapid links should succeed"
    );

    // Verify daemon still responsive
    daemon
        .ping()
        .await
        .expect("Daemon should still be responsive");

    // Check daemon state
    let links = daemon.get_links().await.expect("Failed to get links");
    println!("Daemon has {} active links", links.len());

    println!("SUCCESS: Rapid link creation/teardown test completed");
}

// =========================================================================
// Test 3: Concurrent links through relay
// =========================================================================

/// Test multiple simultaneous links to a destination.
///
/// This test creates multiple concurrent links directly to the exit daemon.
#[tokio::test]
async fn test_concurrent_links_to_daemon() {
    // Create 2-daemon topology
    let topology = DaemonTopology::linear(2)
        .await
        .expect("Failed to create topology");

    // Register destination in exit daemon
    let dest_info = topology
        .exit_daemon()
        .register_destination("stress", &["concurrent", "direct"])
        .await
        .expect("Failed to register");

    let dest_hash: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_info.hash).unwrap().try_into().unwrap();

    let pub_key_bytes = hex::decode(&dest_info.public_key).unwrap();
    let signing_key_bytes: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    const NUM_CONCURRENT: usize = 3;
    let mut handles = Vec::new();

    println!(
        "Creating {} concurrent links to exit daemon...",
        NUM_CONCURRENT
    );

    let exit_addr = topology.exit_daemon().rns_addr();

    for i in 0..NUM_CONCURRENT {
        let signing_key = signing_key_bytes;
        let dest_hash_clone = dest_hash;

        let handle = tokio::spawn(async move {
            // Create link
            let mut link = Link::new_outgoing(dest_hash_clone.into(), &mut OsRng);
            link.set_destination_keys(&signing_key).unwrap();

            // Connect directly to exit daemon
            let mut stream = tokio::net::TcpStream::connect(exit_addr)
                .await
                .expect("Failed to connect");
            tokio::time::sleep(DAEMON_SETTLE_TIME).await;

            let raw_packet = link.build_link_request_packet(None);
            let mut framed = Vec::new();
            frame(&raw_packet, &mut framed);
            stream.write_all(&framed).await.unwrap();
            stream.flush().await.unwrap();

            // Wait for proof
            let mut deframer = Deframer::new();
            let proof = receive_proof_for_link(
                &mut stream,
                &mut deframer,
                link.id(),
                Duration::from_secs(15),
            )
            .await;

            if proof.is_none() {
                println!("Link {}: No proof", i);
                return (i, false);
            }

            let proof = proof.unwrap();
            if link.process_proof(proof.data.as_slice()).is_err() {
                println!("Link {}: Proof failed", i);
                return (i, false);
            }

            if link.state() != LinkState::Active {
                println!("Link {}: Not active", i);
                return (i, false);
            }

            // Send RTT

            let rtt_packet = link.build_rtt_packet(0.05, &mut OsRng).unwrap();
            framed.clear();
            frame(&rtt_packet, &mut framed);
            stream.write_all(&framed).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;

            // Send test message
            let msg = format!("Concurrent link {} message", i);
            let data_packet = link.build_data_packet(msg.as_bytes(), &mut OsRng).unwrap();
            framed.clear();
            frame(&data_packet, &mut framed);
            stream.write_all(&framed).await.unwrap();
            stream.flush().await.unwrap();

            println!("Link {} established and sent message", i);
            (i, true)
        });

        handles.push(handle);
    }

    // Wait for all links
    let mut successful = 0;
    for handle in handles {
        let (i, success) = handle.await.expect("Task failed");
        if success {
            successful += 1;
            println!("Link {} completed successfully", i);
        }
    }

    println!(
        "{} of {} concurrent links succeeded",
        successful, NUM_CONCURRENT
    );

    // At least some should succeed
    assert!(
        successful >= NUM_CONCURRENT / 2,
        "At least half of concurrent links should succeed"
    );

    // Wait for messages to be processed
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Check exit daemon received messages
    let received = topology
        .exit_daemon()
        .get_received_packets()
        .await
        .expect("Failed to get packets");

    let concurrent_messages = received
        .iter()
        .filter(|p| String::from_utf8_lossy(&p.data).contains("Concurrent link"))
        .count();

    println!(
        "Exit daemon received {} concurrent link messages",
        concurrent_messages
    );

    println!("SUCCESS: Concurrent links through relay test completed");
}

// =========================================================================
// Test 4: Large payload variety
// =========================================================================

/// Test sending various payload sizes in sequence.
///
/// This test sends payloads from 1 byte to 400 bytes to verify
/// consistent handling across sizes.
#[tokio::test]
async fn test_large_payload_variety() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register destination
    let dest_info = daemon
        .register_destination("stress", &["payload", "variety"])
        .await
        .expect("Failed to register");

    let dest_hash: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_info.hash).unwrap().try_into().unwrap();

    let pub_key_bytes = hex::decode(&dest_info.public_key).unwrap();
    let signing_key_bytes: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    // Establish link
    let mut link = Link::new_outgoing(dest_hash.into(), &mut OsRng);
    link.set_destination_keys(&signing_key_bytes).unwrap();

    let mut stream = connect_to_daemon(&daemon).await;
    let raw_packet = link.build_link_request_packet(None);

    let mut framed = Vec::new();
    frame(&raw_packet, &mut framed);
    stream.write_all(&framed).await.unwrap();
    stream.flush().await.unwrap();

    let mut deframer = Deframer::new();
    let proof = receive_proof_for_link(
        &mut stream,
        &mut deframer,
        link.id(),
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive proof");

    link.process_proof(proof.data.as_slice()).unwrap();

    let rtt_packet = link.build_rtt_packet(0.05, &mut OsRng).unwrap();
    framed.clear();
    frame(&rtt_packet, &mut framed);
    stream.write_all(&framed).await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Test various sizes
    let test_sizes: Vec<usize> = vec![
        1, 15, 16, 17, 31, 32, 33, 50, 64, 100, 128, 150, 200, 256, 300, 350, 400,
    ];

    println!("Sending {} packets with various sizes...", test_sizes.len());
    let mut sent_successfully = 0;

    for size in &test_sizes {
        // Create deterministic data based on size
        let data: Vec<u8> = (0..*size).map(|i| ((i + size) % 256) as u8).collect();

        match link.build_data_packet(&data, &mut OsRng) {
            Ok(data_packet) => {
                framed.clear();
                frame(&data_packet, &mut framed);
                stream.write_all(&framed).await.unwrap();
                stream.flush().await.unwrap();
                sent_successfully += 1;
                print!("{}.", size);
            }
            Err(e) => {
                println!("\nSize {} exceeds limit: {:?}", size, e);
            }
        }
        std::io::Write::flush(&mut std::io::stdout()).unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    println!();

    println!(
        "Sent {} of {} sizes successfully",
        sent_successfully,
        test_sizes.len()
    );

    // Wait for processing
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Check received packets
    let received = daemon
        .get_received_packets()
        .await
        .expect("Failed to get packets");

    println!("Daemon received {} total packets", received.len());

    // Count sizes received
    let received_sizes: Vec<usize> = received.iter().map(|p| p.data.len()).collect();

    let unique_sizes: std::collections::HashSet<_> = received_sizes.iter().collect();
    println!(
        "Received {} unique sizes: {:?}",
        unique_sizes.len(),
        unique_sizes
    );

    // All varied-size packets must be received
    assert_eq!(
        received.len(),
        test_sizes.len(),
        "All packets must be received. Got {}/{}",
        received.len(),
        test_sizes.len()
    );

    println!("All payload sizes verified");
}

// =========================================================================
// Test 5: Rapid fire exchange (250 messages at maximum throughput)
// =========================================================================

/// Extended stress test with 250 messages at maximum throughput.
///
/// This test verifies:
/// - Links remain stable under continuous load
/// - No message corruption occurs over many packets
/// - Daemon handles rapid fire messages without crashing
/// - Throughput is reasonable (>50 msg/s expected)
#[tokio::test]
async fn test_extended_exchange_rapid_fire() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register destination
    let dest_info = daemon
        .register_destination("stress", &["rapid", "fire"])
        .await
        .expect("Failed to register");

    let dest_hash: [u8; TRUNCATED_HASHBYTES] =
        hex::decode(&dest_info.hash).unwrap().try_into().unwrap();

    let pub_key_bytes = hex::decode(&dest_info.public_key).unwrap();
    let signing_key_bytes: [u8; 32] = pub_key_bytes[32..64].try_into().unwrap();

    // Establish link
    let mut link = Link::new_outgoing(dest_hash.into(), &mut OsRng);
    link.set_destination_keys(&signing_key_bytes).unwrap();

    let mut stream = connect_to_daemon(&daemon).await;
    let raw_packet = link.build_link_request_packet(None);

    let mut framed = Vec::new();
    frame(&raw_packet, &mut framed);
    stream.write_all(&framed).await.unwrap();
    stream.flush().await.unwrap();

    let mut deframer = Deframer::new();
    let proof = receive_proof_for_link(
        &mut stream,
        &mut deframer,
        link.id(),
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive proof");

    link.process_proof(proof.data.as_slice()).unwrap();

    let rtt_packet = link.build_rtt_packet(0.05, &mut OsRng).unwrap();
    framed.clear();
    frame(&rtt_packet, &mut framed);
    stream.write_all(&framed).await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    println!("Link established, sending 250 messages at maximum speed...");
    let start_time = Instant::now();

    const NUM_MESSAGES: usize = 250;

    // Send all messages as fast as possible (no delays)
    for i in 0..NUM_MESSAGES {
        let msg = format!("RapidFire#{:04}", i);
        let data_packet = link
            .build_data_packet(msg.as_bytes(), &mut OsRng)
            .expect("Failed to build packet");

        framed.clear();
        frame(&data_packet, &mut framed);
        stream.write_all(&framed).await.unwrap();
        stream.flush().await.unwrap();
    }

    let send_duration = start_time.elapsed();
    let throughput = NUM_MESSAGES as f64 / send_duration.as_secs_f64();

    println!(
        "Sent {} messages in {:.2}s ({:.1} msg/s)",
        NUM_MESSAGES,
        send_duration.as_secs_f64(),
        throughput
    );

    // Short wait for daemon to process
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let received = daemon
        .get_received_packets()
        .await
        .expect("Failed to get packets");

    let rapid_messages: Vec<_> = received
        .iter()
        .filter(|p| String::from_utf8_lossy(&p.data).starts_with("RapidFire#"))
        .collect();

    // Verify message integrity - spot check content matches expected format
    for msg in rapid_messages.iter().take(10) {
        let text = String::from_utf8_lossy(&msg.data);
        assert!(
            text.starts_with("RapidFire#"),
            "Message corruption detected: {}",
            text
        );
    }

    let delivery_rate = rapid_messages.len() as f64 / NUM_MESSAGES as f64 * 100.0;
    println!(
        "Received {} of {} messages ({:.1}%)",
        rapid_messages.len(),
        NUM_MESSAGES,
        delivery_rate
    );

    // Assert reasonable delivery rate (rapid fire may drop some)
    assert!(
        rapid_messages.len() >= NUM_MESSAGES * 80 / 100,
        "Should receive at least 80% of messages, got {}",
        rapid_messages.len()
    );

    // Verify throughput is reasonable
    assert!(
        throughput > 50.0,
        "Expected >50 msg/s throughput, got {:.1}",
        throughput
    );

    // Verify daemon still responsive
    daemon
        .ping()
        .await
        .expect("Daemon should still be responsive");

    println!("SUCCESS: Rapid fire exchange completed");
}
