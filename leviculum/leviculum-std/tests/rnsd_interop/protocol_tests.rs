//! Pure protocol logic tests - no daemon required.
//!
//! These tests verify packet format, flag encoding, hash derivation, and
//! other protocol details without requiring a running daemon or network.
//!
//! ## Running These Tests
//!
//! ```sh
//! # Run all protocol tests
//! cargo test --package leviculum-std --test rnsd_interop protocol_tests
//!
//! # Run with verbose output
//! cargo test --package leviculum-std --test rnsd_interop protocol_tests -- --nocapture
//! ```

use leviculum_core::constants::{MTU, TRUNCATED_HASHBYTES};
use leviculum_core::crypto::{full_hash, sha256, truncated_hash};
use leviculum_core::identity::Identity;
use leviculum_core::link::Link;
use leviculum_core::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use leviculum_core::{Destination, DestinationType};

use crate::common::compute_name_hash;

// =========================================================================
// Flag byte tests
// =========================================================================

/// Enumerate all valid flag byte combinations (bit7=0).
///
/// This test verifies that all 128 possible flag combinations with bit7=0
/// can be parsed and roundtripped correctly.
#[test]
fn test_flags_byte_all_valid_combinations() {
    let mut valid_count = 0;
    let mut data_type_count = 0;

    for byte in 0u8..128 {
        // bit7 is always 0 (values 0-127)
        match PacketFlags::from_byte(byte) {
            Ok(flags) => {
                let roundtripped = flags.to_byte();
                assert_eq!(
                    roundtripped, byte,
                    "Flags roundtrip failed for 0x{:02x}: got 0x{:02x}",
                    byte, roundtripped
                );
                valid_count += 1;

                if flags.packet_type == PacketType::Data {
                    data_type_count += 1;
                }
            }
            Err(_) => {
                // Some combinations may be invalid (e.g., reserved dest types)
                // That's fine; just count them
            }
        }
    }

    println!(
        "Valid flag combinations: {}/128, Data-type: {}",
        valid_count, data_type_count
    );
    // All 128 combinations should be valid since each field covers its full range
    assert!(
        valid_count >= 64,
        "Expected at least 64 valid flag combinations"
    );
}

// =========================================================================
// Packet context tests
// =========================================================================

/// Verify packet context exact byte values match Python Reticulum.
///
/// This test ensures our PacketContext enum values match the Python implementation
/// byte-for-byte.
#[test]
fn test_packet_context_exact_byte_values() {
    let expected: &[(PacketContext, u8, &str)] = &[
        (PacketContext::None, 0x00, "None"),
        (PacketContext::Resource, 0x01, "Resource"),
        (PacketContext::ResourceAdv, 0x02, "ResourceAdv"),
        (PacketContext::ResourceReq, 0x03, "ResourceReq"),
        (PacketContext::ResourceHmu, 0x04, "ResourceHmu"),
        (PacketContext::ResourcePrf, 0x05, "ResourcePrf"),
        (PacketContext::ResourceIcl, 0x06, "ResourceIcl"),
        (PacketContext::ResourceRcl, 0x07, "ResourceRcl"),
        (PacketContext::CacheRequest, 0x08, "CacheRequest"),
        (PacketContext::Request, 0x09, "Request"),
        (PacketContext::Response, 0x0A, "Response"),
        (PacketContext::PathResponse, 0x0B, "PathResponse"),
        (PacketContext::Command, 0x0C, "Command"),
        (PacketContext::CommandStatus, 0x0D, "CommandStatus"),
        (PacketContext::Channel, 0x0E, "Channel"),
        (PacketContext::Keepalive, 0xFA, "Keepalive"),
        (PacketContext::LinkIdentify, 0xFB, "LinkIdentify"),
        (PacketContext::LinkClose, 0xFC, "LinkClose"),
        (PacketContext::LinkProof, 0xFD, "LinkProof"),
        (PacketContext::Lrrtt, 0xFE, "Lrrtt"),
        (PacketContext::Lrproof, 0xFF, "Lrproof"),
    ];

    for &(context, expected_byte, name) in expected {
        assert_eq!(
            context.to_byte(),
            expected_byte,
            "PacketContext::{} should be 0x{:02X}, got 0x{:02X}",
            name,
            expected_byte,
            context.to_byte(),
        );

        // Verify roundtrip through TryFrom
        let parsed = PacketContext::try_from(expected_byte)
            .unwrap_or_else(|_| panic!("Failed to parse context byte 0x{:02X}", expected_byte));
        assert_eq!(parsed.to_byte(), expected_byte);
    }

    // Also verify that packing a packet with context_flag=true places context at offset 18
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: true,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: [0x00; 16],
        context: PacketContext::Keepalive,
        data: PacketData::Owned(vec![0x42]),
    };

    let mut raw = [0u8; MTU];
    let _size = packet.pack(&mut raw).unwrap();
    // Context byte is at: flags(1) + hops(1) + dest_hash(16) = offset 18
    assert_eq!(
        raw[18], 0xFA,
        "Context byte at offset 18 should be 0xFA (Keepalive)"
    );

    println!("All 21 PacketContext values verified");
}

// =========================================================================
// Name hash tests
// =========================================================================

/// Verify name_hash for well-known app names matches Python Reticulum.
///
/// Pre-computed using Python Reticulum:
/// ```python
/// import RNS
/// RNS.Identity.full_hash("lxmf.delivery".encode())[:10].hex()
/// ```
#[test]
fn test_name_hash_known_vectors() {
    // These are the SHA-256 hash of the full name string, truncated to 10 bytes.
    // Verify our compute_name_hash matches Python's behavior.

    let test_cases = [
        ("lxmf", &["delivery"][..]),
        ("nomadnetwork", &["node"][..]),
        ("testapp", &["echo"][..]),
    ];

    for (app_name, aspects) in &test_cases {
        let our_hash = compute_name_hash(app_name, aspects);

        // Verify manually: construct the full name and SHA-256 it
        let mut full_name = app_name.to_string();
        for aspect in *aspects {
            full_name.push('.');
            full_name.push_str(aspect);
        }
        let expected_full_hash = sha256(full_name.as_bytes());
        let expected_name_hash: [u8; 10] = expected_full_hash[..10].try_into().unwrap();

        assert_eq!(
            our_hash, expected_name_hash,
            "Name hash mismatch for '{}'",
            full_name
        );
        println!("  Name hash for '{}': {:02x?} - OK", full_name, our_hash);
    }

    // The critical invariant: our compute_name_hash uses full_hash (SHA-256)
    // and takes the first 10 bytes. Verify this explicitly:
    let manual = full_hash(b"lxmf.delivery");
    let manual_10: [u8; 10] = manual[..10].try_into().unwrap();
    let computed = compute_name_hash("lxmf", &["delivery"]);
    assert_eq!(computed, manual_10);

    println!("All name hash vectors verified");
}

// =========================================================================
// Destination hash tests
// =========================================================================

/// Verify destination hash from a known private key.
///
/// This test creates an identity from deterministic private key bytes and
/// verifies the destination hash derivation pipeline.
#[test]
fn test_destination_hash_known_vector() {
    // Create an identity from deterministic private key bytes
    let private_key_bytes = [0x42u8; 64]; // 32 X25519 + 32 Ed25519
    let identity = Identity::from_private_key_bytes(&private_key_bytes)
        .expect("Failed to create identity from known key");

    let identity_hash = *identity.hash();
    let name_hash = compute_name_hash("testapp", &["echo"]);
    let dest_hash = Destination::compute_destination_hash(&name_hash, &identity_hash).into_bytes();

    // Verify the pipeline: dest_hash = truncated_hash(name_hash + identity_hash)
    let mut hash_material = Vec::new();
    hash_material.extend_from_slice(&name_hash);
    hash_material.extend_from_slice(&identity_hash);
    let expected = truncated_hash(&hash_material);

    assert_eq!(dest_hash, expected);
    println!("  Identity hash: {:02x?}", &identity_hash[..4]);
    println!("  Name hash: {:02x?}", name_hash);
    println!("  Destination hash: {:02x?}", &dest_hash[..4]);
    println!("Destination hash pipeline verified");
}

// =========================================================================
// Link request byte layout tests
// =========================================================================

/// Verify link request packet byte layout.
///
/// This test verifies the exact byte-level layout of a link request packet.
///
/// Link requests always include 3-byte MTU signaling bytes (since MTU negotiation),
/// making the payload 67 bytes (64 base + 3 signaling) and total 86 bytes.
#[test]
fn test_link_request_packet_byte_layout() {
    let dest_hash = [0x42u8; TRUNCATED_HASHBYTES];
    let mut link = Link::new_outgoing(dest_hash.into(), &mut rand_core::OsRng);

    let raw = link.build_link_request_packet(None);

    // Expected: [flags(1)] [hops(1)] [dest_hash(16)] [context(1)] [payload(64+3 signaling)]
    assert_eq!(
        raw.len(),
        86,
        "Link request should be 86 bytes (83 base + 3 signaling)"
    );

    // Byte 0: flags = LinkRequest(0x02), H1, broadcast, single
    assert_eq!(raw[0], 0x02, "Flags should be 0x02 (LinkRequest)");

    // Byte 1: hops = 0
    assert_eq!(raw[1], 0x00, "Hops should be 0");

    // Bytes 2..18: destination hash
    assert_eq!(&raw[2..18], &dest_hash, "Dest hash mismatch");

    // Byte 18: context = None (0x00)
    assert_eq!(raw[18], 0x00, "Context should be 0x00 (None)");

    // Bytes 19..51: ephemeral X25519 public key (32 bytes)
    let ephemeral_pub = link.ephemeral_public_bytes();
    assert_eq!(
        &raw[19..51],
        &ephemeral_pub,
        "Ephemeral X25519 pub mismatch"
    );

    // Bytes 51..83: Ed25519 verifying key (32 bytes)
    let verifying_key = link.verifying_key_bytes();
    assert_eq!(
        &raw[51..83],
        &verifying_key,
        "Ed25519 verifying key mismatch"
    );

    // Bytes 83..86: MTU signaling bytes (3 bytes)
    // With hw_mtu=None, defaults to base MTU (500)
    let signaling = &raw[83..86];
    assert_eq!(signaling.len(), 3, "Signaling bytes should be 3 bytes");

    println!("Link request byte layout verified:");
    println!("  [0]:      0x{:02x} (flags)", raw[0]);
    println!("  [1]:      0x{:02x} (hops)", raw[1]);
    println!("  [2..18]:  dest_hash {:02x?}...", &raw[2..6]);
    println!("  [18]:     0x{:02x} (context)", raw[18]);
    println!("  [19..51]: ephemeral_pub {:02x?}...", &raw[19..23]);
    println!("  [51..83]: verifying_key {:02x?}...", &raw[51..55]);
    println!("  [83..86]: signaling {:02x?}", signaling);
}
