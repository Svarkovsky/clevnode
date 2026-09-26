//! Link integration test against Python rnsd
//!
//! This test establishes a link with a Python RNS destination and exits quickly.
//! It has strict timeouts and will never hang or run in an infinite loop.
//!
//! Usage: cargo run -p leviculum-std --example link_integration_test -- <dest_hash_hex> <signing_key_hex>
//!
//! Exit codes:
//!   0 - Success: Link established
//!   1 - Error: Invalid arguments or setup failure
//!   2 - Error: Connection failed
//!   3 - Error: Link proof verification failed
//!   4 - Error: Timeout waiting for proof

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::link::{Link, LinkState};
use leviculum_core::DestinationHash;
use leviculum_std::interfaces::hdlc::{frame, DeframeResult, Deframer};

const PROOF_TIMEOUT_SECS: u64 = 10;
const MAX_PACKETS_TO_CHECK: usize = 50;

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err("Hex string must have even length".to_string());
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[derive(Debug)]
enum TestResult {
    Success { link_id: String, link_key: String },
    ConnectionFailed(String),
    ProofFailed(String),
    Timeout,
    SetupError(String),
}

fn run_link_test(
    dest_hash: [u8; TRUNCATED_HASHBYTES],
    signing_key: [u8; 32],
    host_port: &str,
) -> TestResult {
    // Create link
    let mut link = Link::new_outgoing(DestinationHash::new(dest_hash), &mut rand_core::OsRng);
    if let Err(e) = link.set_destination_keys(&signing_key) {
        return TestResult::SetupError(format!("Failed to set destination keys: {:?}", e));
    }

    // Build the link request packet
    let packet = link.build_link_request_packet(None);

    // Frame with HDLC
    let mut framed = Vec::new();
    frame(&packet, &mut framed);

    // Connect to rnsd
    let mut stream = match TcpStream::connect(host_port) {
        Ok(s) => s,
        Err(e) => return TestResult::ConnectionFailed(format!("Connect failed: {}", e)),
    };

    // Set short timeouts
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_nodelay(true);

    // Send link request
    if let Err(e) = stream.write_all(&framed) {
        return TestResult::ConnectionFailed(format!("Write failed: {}", e));
    }
    if let Err(e) = stream.flush() {
        return TestResult::ConnectionFailed(format!("Flush failed: {}", e));
    }

    // Wait for response with timeout
    let start = Instant::now();
    let mut deframer = Deframer::new();
    let mut recv_buffer = [0u8; 1024];
    let mut packets_checked = 0;

    while start.elapsed() < Duration::from_secs(PROOF_TIMEOUT_SECS)
        && packets_checked < MAX_PACKETS_TO_CHECK
    {
        match stream.read(&mut recv_buffer) {
            Ok(0) => {
                return TestResult::ConnectionFailed("Connection closed by peer".to_string());
            }
            Ok(n) => {
                let results = deframer.process(&recv_buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(frame_data) = result {
                        packets_checked += 1;

                        // Parse the packet - need at least header
                        if frame_data.len() >= 20 {
                            let dest_hash_recv = &frame_data[2..18];
                            let context = frame_data[18];
                            let payload = &frame_data[19..];

                            // Check if this is addressed to our link_id and is a proof (context = 0xFF)
                            if dest_hash_recv == link.id().as_bytes() && context == 0xFF {
                                match link.process_proof(payload) {
                                    Ok(()) => {
                                        if link.state() == LinkState::Active {
                                            let link_key = link
                                                .link_key()
                                                .map(|k| bytes_to_hex(k))
                                                .unwrap_or_else(|| "none".to_string());
                                            return TestResult::Success {
                                                link_id: bytes_to_hex(link.id().as_bytes()),
                                                link_key,
                                            };
                                        }
                                    }
                                    Err(e) => {
                                        return TestResult::ProofFailed(format!("{:?}", e));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind() != std::io::ErrorKind::WouldBlock
                    && e.kind() != std::io::ErrorKind::TimedOut
                {
                    return TestResult::ConnectionFailed(format!("Read error: {}", e));
                }
                // Timeout on this read, continue loop
            }
        }
    }

    TestResult::Timeout
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 3 {
        eprintln!(
            "Usage: {} <dest_hash_hex> <signing_key_hex> [host:port]",
            args[0]
        );
        eprintln!();
        eprintln!("  dest_hash_hex:    16-byte destination hash (32 hex chars)");
        eprintln!("  signing_key_hex:  32-byte Ed25519 signing public key (64 hex chars)");
        eprintln!("  host:port:        Optional, default 127.0.0.1:4242");
        eprintln!();
        eprintln!("Exit codes: 0=success, 1=args, 2=connection, 3=proof failed, 4=timeout");
        std::process::exit(1);
    }

    // Parse destination hash
    let dest_hash_bytes = match hex_to_bytes(&args[1]) {
        Ok(b) if b.len() == TRUNCATED_HASHBYTES => b,
        Ok(b) => {
            eprintln!(
                "[FAIL] Destination hash must be {} bytes, got {}",
                TRUNCATED_HASHBYTES,
                b.len()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[FAIL] Invalid destination hash: {}", e);
            std::process::exit(1);
        }
    };
    let mut dest_hash = [0u8; TRUNCATED_HASHBYTES];
    dest_hash.copy_from_slice(&dest_hash_bytes);

    // Parse destination signing key
    let signing_key_bytes = match hex_to_bytes(&args[2]) {
        Ok(b) if b.len() == 32 => b,
        Ok(b) => {
            eprintln!("[FAIL] Signing key must be 32 bytes, got {}", b.len());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[FAIL] Invalid signing key: {}", e);
            std::process::exit(1);
        }
    };
    let mut signing_key = [0u8; 32];
    signing_key.copy_from_slice(&signing_key_bytes);

    let host_port = args.get(3).map(String::as_str).unwrap_or("127.0.0.1:4242");

    println!("=== Link Integration Test ===");
    println!("Destination: {}", args[1]);
    println!(
        "Timeout: {}s, Max packets: {}",
        PROOF_TIMEOUT_SECS, MAX_PACKETS_TO_CHECK
    );
    println!();

    let result = run_link_test(dest_hash, signing_key, host_port);

    match result {
        TestResult::Success { link_id, link_key } => {
            println!("[PASS] Link established successfully!");
            println!("  Link ID:  {}", link_id);
            println!("  Link Key: {}...", &link_key[..32]);
            std::process::exit(0);
        }
        TestResult::ConnectionFailed(msg) => {
            eprintln!("[FAIL] Connection error: {}", msg);
            std::process::exit(2);
        }
        TestResult::ProofFailed(msg) => {
            eprintln!("[FAIL] Proof verification failed: {}", msg);
            std::process::exit(3);
        }
        TestResult::Timeout => {
            eprintln!(
                "[FAIL] Timeout waiting for link proof ({}s)",
                PROOF_TIMEOUT_SECS
            );
            std::process::exit(4);
        }
        TestResult::SetupError(msg) => {
            eprintln!("[FAIL] Setup error: {}", msg);
            std::process::exit(1);
        }
    }
}
