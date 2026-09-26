//! Request/response interoperability tests with Python Reticulum.
//!
//! These tests verify that Rust's request/response protocol is wire-compatible
//! with Python's `destination.register_request_handler()` / `Link.request()`.
//!
//! ## What These Tests Verify
//!
//! 1. **Msgpack encoding** - request payload format matches Python
//! 2. **request_id computation** - `truncated_packet_hash(raw_packet)` matches Python
//! 3. **Packet context bytes** - Request (0x09) and Response (0x0A) match
//! 4. **Encryption/decryption** - Both sides can decrypt each other's packets
//! 5. **Full round-trip** - Rust request → Python handler → Rust receives response
//!
//! ## Running These Tests
//!
//! ```sh
//! cargo test --package leviculum-std --test rnsd_interop request_tests -- --nocapture
//! ```

use std::time::Duration;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::link::{Link, LinkState};
use leviculum_core::packet::PacketContext;
use leviculum_std::interfaces::hdlc::{frame, DeframeResult, Deframer};

use crate::common::*;
use crate::harness::TestDaemon;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Encode a msgpack request payload: fixarray(3) [float64(timestamp), bin(path_hash), data]
fn encode_request_payload(path_hash: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();

    let mut buf = Vec::new();
    // fixarray(3) header
    buf.push(0x93);
    // float64 timestamp
    buf.push(0xcb);
    buf.extend_from_slice(&now_secs.to_be_bytes());
    // bin8(16) path_hash
    buf.push(0xc4);
    buf.push(16);
    buf.extend_from_slice(path_hash);
    // data (already msgpack-encoded)
    buf.extend_from_slice(data);
    buf
}

/// Encode a string as a msgpack fixstr.
fn msgpack_fixstr(s: &str) -> Vec<u8> {
    assert!(s.len() <= 31);
    let mut buf = Vec::new();
    buf.push(0xa0 | (s.len() as u8));
    buf.extend_from_slice(s.as_bytes());
    buf
}

/// Parse a msgpack response payload: fixarray(2) [bin(request_id), response_data]
fn parse_response_payload(plain: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut pos = 0;
    // fixarray(2)
    if pos >= plain.len() || plain[pos] != 0x92 {
        return None;
    }
    pos += 1;

    // bin8(16) request_id
    if pos >= plain.len() || plain[pos] != 0xc4 {
        return None;
    }
    pos += 1;
    if pos >= plain.len() {
        return None;
    }
    let bin_len = plain[pos] as usize;
    pos += 1;
    if pos + bin_len > plain.len() {
        return None;
    }
    let request_id = plain[pos..pos + bin_len].to_vec();
    pos += bin_len;

    // Remaining bytes are the response data (one msgpack value)
    let response_data = plain[pos..].to_vec();
    Some((request_id, response_data))
}

/// Test: Rust sends request to Python echo handler, receives response.
///
/// Topology: Rust (initiator) → Python daemon (responder, direct connection)
///
/// Flow:
/// 1. Python registers destination + echo request handler
/// 2. Rust establishes link to Python
/// 3. Rust builds and sends Request packet (context 0x09)
/// 4. Python's handler echoes back the data
/// 5. Rust receives Response packet (context 0x0A) with matching request_id
#[tokio::test]
async fn test_request_response_python_handler() {
    let daemon = TestDaemon::start().await.expect("Failed to start daemon");

    // Register a destination that accepts links
    let dest_info = daemon
        .register_destination("reqtest", &["echo"])
        .await
        .expect("Failed to register destination");

    eprintln!("Registered destination: {}", dest_info.hash);

    // Register echo request handler on the Python destination
    let result = daemon
        .register_echo_request_handler(&dest_info.hash, "/echo")
        .await
        .expect("Failed to register echo request handler");
    eprintln!("Registered echo handler: {:?}", result);

    let pub_key_bytes = hex::decode(&dest_info.public_key).expect("Invalid public key hex");
    let signing_key_bytes: [u8; 32] = pub_key_bytes[32..64]
        .try_into()
        .expect("Invalid signing key");
    let dest_hash: [u8; TRUNCATED_HASHBYTES] = hex::decode(&dest_info.hash)
        .expect("Invalid hash hex")
        .try_into()
        .expect("Invalid hash length");

    // Establish link: Rust initiator → Python responder
    let mut link = Link::new_outgoing(dest_hash.into(), &mut OsRng);
    link.set_destination_keys(&signing_key_bytes)
        .expect("Failed to set destination keys");

    let raw_packet = link.build_link_request_packet(None);
    let mut stream = connect_to_daemon(&daemon).await;
    let mut framed_buf = Vec::new();
    frame(&raw_packet, &mut framed_buf);
    stream.write_all(&framed_buf).await.unwrap();
    stream.flush().await.unwrap();

    let mut deframer = Deframer::new();
    let proof_packet = receive_proof_for_link(
        &mut stream,
        &mut deframer,
        link.id(),
        Duration::from_secs(10),
    )
    .await
    .expect("Should receive link proof");

    link.process_proof(proof_packet.data.as_slice())
        .expect("Proof should validate");
    assert_eq!(link.state(), LinkState::Active);

    // Send RTT to finalize link on Python side
    let rtt_packet = link.build_rtt_packet(0.05, &mut OsRng).unwrap();
    framed_buf.clear();
    frame(&rtt_packet, &mut framed_buf);
    stream.write_all(&framed_buf).await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Build request payload
    let path_hash = leviculum_core::crypto::truncated_hash(b"/echo");
    let request_data = msgpack_fixstr("hello");
    let packed = encode_request_payload(&path_hash, &request_data);

    // Build and send Request packet (context 0x09)
    let request_packet = link
        .build_data_packet_with_context(&packed, PacketContext::Request, &mut OsRng)
        .expect("Failed to build request packet");

    let request_id = leviculum_core::packet::truncated_packet_hash(&request_packet);
    eprintln!("Sending request, request_id: {}", hex::encode(request_id));

    framed_buf.clear();
    frame(&request_packet, &mut framed_buf);
    stream.write_all(&framed_buf).await.unwrap();
    stream.flush().await.unwrap();

    // Wait for response packet from Python
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut response_found = false;

    while tokio::time::Instant::now() < deadline {
        let mut buf = [0u8; 4096];
        let read_result =
            tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await;

        match read_result {
            Ok(Ok(n)) if n > 0 => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(packet_data) = result {
                        if packet_data.len() < 20 {
                            continue;
                        }
                        // Packet format: [flags(1)][hops(1)][link_id(16)][context(1)][encrypted]
                        let context_byte = packet_data[18];
                        if context_byte == 0x0A {
                            // Response context!
                            let encrypted = &packet_data[19..];
                            let mut plaintext = vec![0u8; encrypted.len()];
                            match link.decrypt(encrypted, &mut plaintext) {
                                Ok(len) => {
                                    let plain = &plaintext[..len];
                                    eprintln!("Decrypted response: {} bytes", len);

                                    let (resp_rid, resp_data) = parse_response_payload(plain)
                                        .expect("should parse response payload");

                                    assert_eq!(
                                        resp_rid.len(),
                                        TRUNCATED_HASHBYTES,
                                        "request_id should be 16 bytes"
                                    );
                                    assert_eq!(
                                        resp_rid.as_slice(),
                                        &request_id,
                                        "response request_id should match"
                                    );

                                    // Python echoes back the data, should be msgpack "hello"
                                    let expected_echo = msgpack_fixstr("hello");
                                    assert_eq!(
                                        resp_data, expected_echo,
                                        "response should echo back the request data"
                                    );

                                    response_found = true;
                                    eprintln!("SUCCESS: received correct echo response");
                                }
                                Err(e) => {
                                    eprintln!("Response decryption failed: {e:?}");
                                }
                            }
                        }
                    }
                }
                if response_found {
                    break;
                }
            }
            _ => {}
        }
    }

    assert!(
        response_found,
        "Should receive response from Python echo handler within 10s"
    );
}
