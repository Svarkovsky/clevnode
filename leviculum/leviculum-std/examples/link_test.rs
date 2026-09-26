//! Link establishment test against Python rnsd
//!
//! Usage: cargo run -p leviculum-std --example link_test -- <dest_hash_hex> <signing_key_hex>
//!
//! Example:
//! cargo run -p leviculum-std --example link_test -- \
//!   a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4 \
//!   0102030405060708091011121314151617181920212223242526272829303132

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use leviculum_core::constants::TRUNCATED_HASHBYTES;
use leviculum_core::link::Link;
use leviculum_core::DestinationHash;
use leviculum_std::interfaces::hdlc::{frame, DeframeResult, Deframer};

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        std::process::exit(1);
    }

    // Parse destination hash
    let dest_hash_hex = &args[1];
    let dest_hash_bytes = hex_to_bytes(dest_hash_hex)?;
    if dest_hash_bytes.len() != TRUNCATED_HASHBYTES {
        return Err(format!(
            "Destination hash must be {} bytes, got {}",
            TRUNCATED_HASHBYTES,
            dest_hash_bytes.len()
        )
        .into());
    }
    let mut dest_hash = [0u8; TRUNCATED_HASHBYTES];
    dest_hash.copy_from_slice(&dest_hash_bytes);

    // Parse destination signing key
    let signing_key_hex = &args[2];
    let signing_key_bytes = hex_to_bytes(signing_key_hex)?;
    if signing_key_bytes.len() != 32 {
        return Err(format!(
            "Signing key must be 32 bytes, got {}",
            signing_key_bytes.len()
        )
        .into());
    }
    let mut signing_key = [0u8; 32];
    signing_key.copy_from_slice(&signing_key_bytes);

    // Parse host:port
    let host_port = args.get(3).map(String::as_str).unwrap_or("127.0.0.1:4242");

    println!("=== Link Establishment Test ===");
    println!("Target destination: {}", dest_hash_hex);
    println!("Signing key: {}", signing_key_hex);
    println!("Connecting to: {}", host_port);

    // Create link
    let mut link = Link::new_outgoing(DestinationHash::new(dest_hash), &mut rand_core::OsRng);
    link.set_destination_keys(&signing_key)
        .map_err(|e| format!("Failed to set destination keys: {:?}", e))?;

    // Build the link request packet
    let packet = link.build_link_request_packet(None);
    println!("\nLink request packet ({} bytes):", packet.len());
    println!("  Raw: {}", bytes_to_hex(&packet));
    println!("  Link ID: {}", bytes_to_hex(link.id().as_bytes()));

    // Frame with HDLC
    let mut framed = Vec::new();
    frame(&packet, &mut framed);
    println!("\nHDLC framed ({} bytes):", framed.len());
    println!("  Framed: {}", bytes_to_hex(&framed));

    // Connect to rnsd
    println!("\nConnecting to {}...", host_port);
    let mut stream = TcpStream::connect(host_port)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    println!("Connected!");

    // Send link request
    println!("\nSending link request...");
    stream.write_all(&framed)?;
    stream.flush()?;
    println!("Sent {} bytes", framed.len());

    // Wait for response
    println!("\nWaiting for link proof...");
    let mut deframer = Deframer::new();
    let mut recv_buffer = [0u8; 1024];

    loop {
        match stream.read(&mut recv_buffer) {
            Ok(0) => {
                println!("Connection closed by peer");
                break;
            }
            Ok(n) => {
                println!("Received {} bytes: {}", n, bytes_to_hex(&recv_buffer[..n]));

                let results = deframer.process(&recv_buffer[..n]);
                for result in results {
                    match result {
                        DeframeResult::Frame(frame_data) => {
                            println!("\n=== Received Frame ({} bytes) ===", frame_data.len());
                            println!("Raw: {}", bytes_to_hex(&frame_data));

                            // Parse the packet
                            if frame_data.len() >= 20 {
                                let flags = frame_data[0];
                                let hops = frame_data[1];
                                let dest_hash_recv = &frame_data[2..18];
                                let context = frame_data[18];
                                let payload = &frame_data[19..];

                                println!("Flags: 0x{:02x}", flags);
                                println!("Hops: {}", hops);
                                println!("Dest hash: {}", bytes_to_hex(dest_hash_recv));
                                println!("Context: 0x{:02x}", context);
                                println!(
                                    "Payload ({} bytes): {}",
                                    payload.len(),
                                    bytes_to_hex(payload)
                                );

                                // Check if this is a link proof (context = 0xFF)
                                if context == 0xFF {
                                    println!("\n=== Processing Link Proof ===");
                                    match link.process_proof(payload) {
                                        Ok(()) => {
                                            println!("Link established successfully!");
                                            println!("Link state: {:?}", link.state());
                                            println!(
                                                "Link ID: {}",
                                                bytes_to_hex(link.id().as_bytes())
                                            );
                                            if let Some(key) = link.link_key() {
                                                println!("Link key: {}", bytes_to_hex(key));
                                            }
                                            return Ok(());
                                        }
                                        Err(e) => {
                                            println!("Failed to process proof: {:?}", e);
                                        }
                                    }
                                }
                            }
                        }
                        DeframeResult::TooShort => {
                            println!("Received frame too short (empty)");
                        }
                        DeframeResult::Oversized => {
                            println!("Received frame exceeds HW_MTU, discarded");
                        }
                        DeframeResult::NeedMore => {
                            // Continue receiving
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                {
                    println!("Timeout waiting for response");
                } else {
                    println!("Error reading: {}", e);
                }
                break;
            }
        }
    }

    Ok(())
}
