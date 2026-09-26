//! Shared test infrastructure for rnsd interop tests

pub use rand_core::OsRng;
use std::collections::HashSet;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Initialize tracing for tests.  Safe to call multiple times
/// (idempotent).  Controlled via `RUST_LOG` env var
/// (e.g. `RUST_LOG=debug`).  Output goes through libtest's capture
/// mechanism.
///
/// Delegates to `leviculum_std::test_support::tracing_setup` so
/// future test-harness layers (Stage 6 / Codeberg #39 piece 1's
/// event-log layer) can compose into the same global subscriber.
pub fn init_tracing() {
    leviculum_std::test_support::tracing_setup::init_tracing_with_event_log();
}

use leviculum_core::constants::{MTU, TRUNCATED_HASHBYTES};
use leviculum_core::crypto::truncated_hash;
use leviculum_core::identity::Identity;
use leviculum_core::link::{Link, LinkId};
use leviculum_core::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use leviculum_core::traits::Clock;
use leviculum_core::{Destination, DestinationHash, DestinationType, Direction};
use leviculum_std::interfaces::hdlc::{frame, DeframeResult, Deframer};

// =========================================================================
// Shared test helpers
// =========================================================================

/// Real-time clock for tests that need actual time
pub struct TestClock;

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }
    /// Answer the wall clock like `leviculum_std::clock::SystemClock` does, so
    /// `Transport::emission_secs` — the one producer every cross-lifetime wire
    /// field draws from (Codeberg #155, #182) — resolves at source 1 rather
    /// than falling through to uptime seconds.
    fn wall_unix_secs(&self) -> Option<u64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| since.as_secs())
    }
    /// And the sub-second reading too (Codeberg #217), for the same reason:
    /// this clock stands in for `SystemClock` in the interop suite, and the
    /// LXMF message timestamp Python reads back is built from it.
    fn wall_unix_micros(&self) -> Option<u64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|since| u64::try_from(since.as_micros()).ok())
    }
}

/// Get current time in milliseconds (convenience for tests)
pub fn now_ms() -> u64 {
    TestClock.now_ms()
}

/// Create a unique temp storage directory for a test node.
///
/// Returns `TempDir` guard, directory is removed on Drop. The caller must
/// keep the guard alive for the test duration. Use `.path().to_path_buf()`
/// to get the `PathBuf` for `ReticulumNodeBuilder::storage_path()`.
pub fn temp_storage(test_name: &str, node: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("reticulum_test_{test_name}_{node}_"))
        .tempdir()
        .expect("failed to create temp storage dir")
}

/// Parsed announce data for verification
pub struct ParsedAnnounce {
    pub destination_hash: [u8; 16],
    pub public_key: Vec<u8>,
    pub name_hash: Vec<u8>,
    pub random_hash: Vec<u8>,
    pub ratchet: Vec<u8>,
    pub signature: Vec<u8>,
    pub app_data: Vec<u8>,
}

impl ParsedAnnounce {
    /// Parse an announce payload
    ///
    /// Announce format depends on context_flag:
    /// - context_flag=0: public_key(64) + name_hash(10) + random_hash(10) + signature(64) + app_data
    /// - context_flag=1: public_key(64) + name_hash(10) + random_hash(10) + ratchet(32) + signature(64) + app_data
    pub fn from_packet(packet: &Packet) -> Option<Self> {
        let payload = packet.data.as_slice();
        let has_ratchet = packet.flags.context_flag;

        if has_ratchet {
            if payload.len() < 180 {
                return None;
            }
            Some(Self {
                destination_hash: packet.destination_hash,
                public_key: payload[0..64].to_vec(),
                name_hash: payload[64..74].to_vec(),
                random_hash: payload[74..84].to_vec(),
                ratchet: payload[84..116].to_vec(),
                signature: payload[116..180].to_vec(),
                app_data: payload[180..].to_vec(),
            })
        } else {
            if payload.len() < 148 {
                return None;
            }
            Some(Self {
                destination_hash: packet.destination_hash,
                public_key: payload[0..64].to_vec(),
                name_hash: payload[64..74].to_vec(),
                random_hash: payload[74..84].to_vec(),
                ratchet: Vec::new(),
                signature: payload[84..148].to_vec(),
                app_data: payload[148..].to_vec(),
            })
        }
    }

    /// Compute the signed data for signature verification
    pub fn signed_data(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(100 + self.ratchet.len() + self.app_data.len());
        data.extend_from_slice(&self.destination_hash);
        data.extend_from_slice(&self.public_key);
        data.extend_from_slice(&self.name_hash);
        data.extend_from_slice(&self.random_hash);
        data.extend_from_slice(&self.ratchet);
        data.extend_from_slice(&self.app_data);
        data
    }

    /// Compute identity hash from public key
    pub fn computed_identity_hash(&self) -> [u8; 16] {
        truncated_hash(&self.public_key)
    }

    /// Compute destination hash from name_hash and identity_hash
    pub fn computed_destination_hash(&self) -> [u8; 16] {
        let identity_hash = self.computed_identity_hash();
        let mut hash_material = Vec::with_capacity(26);
        hash_material.extend_from_slice(&self.name_hash);
        hash_material.extend_from_slice(&identity_hash);
        truncated_hash(&hash_material)
    }
}

/// Helper to compute name_hash from app_name and aspects
/// Delegates to Destination::compute_name_hash
pub fn compute_name_hash(app_name: &str, aspects: &[&str]) -> [u8; 10] {
    Destination::compute_name_hash(app_name, aspects)
}

/// Build and send a valid announce on the given stream.
/// Returns (destination_hash, destination) for verification.
pub async fn build_and_send_announce(
    stream: &mut TcpStream,
    app_name: &str,
    aspects: &[&str],
    app_data: &[u8],
) -> (DestinationHash, Destination) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        app_name,
        aspects,
    )
    .expect("Failed to create destination");

    let packet = dest
        .announce(Some(app_data), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("Failed to create announce");

    let mut raw_packet = [0u8; MTU];
    let size = packet.pack(&mut raw_packet).expect("Failed to pack packet");

    let mut framed = Vec::new();
    frame(&raw_packet[..size], &mut framed);

    stream.write_all(&framed).await.expect("Failed to send");
    stream.flush().await.expect("Failed to flush");

    let dest_hash = *dest.hash();
    (dest_hash, dest)
}

/// Build a valid announce as raw bytes (not framed), returning (raw_bytes, dest_hash, destination).
pub fn build_announce_raw(
    app_name: &str,
    aspects: &[&str],
    app_data: &[u8],
) -> (Vec<u8>, DestinationHash, Destination) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        app_name,
        aspects,
    )
    .expect("Failed to create destination");

    let packet = dest
        .announce(Some(app_data), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("Failed to create announce");

    let mut raw_packet = [0u8; MTU];
    let size = packet.pack(&mut raw_packet).expect("Failed to pack packet");

    let dest_hash = *dest.hash();
    (raw_packet[..size].to_vec(), dest_hash, dest)
}

/// Build announce raw bytes with a specific hops value
pub fn build_announce_raw_with_hops(
    app_name: &str,
    aspects: &[&str],
    app_data: &[u8],
    hops: u8,
) -> (Vec<u8>, DestinationHash, Destination) {
    let (mut raw, dest_hash, dest) = build_announce_raw(app_name, aspects, app_data);
    // Hops is the second byte (offset 1) in the raw packet
    raw[1] = hops;
    (raw, dest_hash, dest)
}

/// Wait for an announce with a specific destination hash on the given stream.
/// Returns true if found within the timeout.
pub async fn wait_for_announce(
    stream: &mut TcpStream,
    dest_hash: &DestinationHash,
    timeout_duration: Duration,
) -> bool {
    let mut deframer = Deframer::new();
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(500), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Announce
                                && pkt.destination_hash == *dest_hash.as_bytes()
                            {
                                return true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Transport routing information extracted from an announce packet.
/// This contains the information needed to route link requests through intermediate nodes.
#[derive(Debug)]
pub struct AnnounceRouteInfo {
    /// The announce packet
    pub packet: Packet,
    /// Raw deframed bytes (for feeding to `NodeCore::handle_packet()`)
    pub raw_data: Vec<u8>,
    /// The transport_id from HEADER_2 announces, or None for direct announces
    pub transport_id: Option<[u8; 16]>,
    /// The hop count from the announce
    pub hops: u8,
}

impl AnnounceRouteInfo {
    /// Extract the destination's signing key from the announce payload.
    /// The signing key is bytes 32-64 of the public key in the announce data.
    pub fn signing_key(&self) -> Option<[u8; 32]> {
        let data = self.packet.data.as_slice();
        if data.len() >= 64 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&data[32..64]);
            Some(key)
        } else {
            None
        }
    }
}

/// Wait for any announce packet and return full routing information.
/// This includes the transport_id and hops needed for multi-hop link establishment.
pub async fn wait_for_any_announce_with_route_info(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    timeout_duration: Duration,
) -> Option<AnnounceRouteInfo> {
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(100), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Announce {
                                return Some(AnnounceRouteInfo {
                                    transport_id: pkt.transport_id,
                                    hops: pkt.hops,
                                    packet: pkt,
                                    raw_data: data,
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Send raw bytes (already packed, not framed) over a stream with HDLC framing
pub async fn send_framed(stream: &mut TcpStream, raw: &[u8]) {
    let mut framed = Vec::new();
    frame(raw, &mut framed);
    stream.write_all(&framed).await.expect("Failed to send");
    stream.flush().await.expect("Failed to flush");
}

/// Check that a connection is still open by waiting briefly for data or timeout
pub async fn connection_alive(stream: &mut TcpStream) -> bool {
    let mut buffer = [0u8; 1];
    match timeout(Duration::from_millis(200), stream.read(&mut buffer)).await {
        Ok(Ok(0)) => false,  // Connection closed
        Ok(Err(_)) => false, // Error
        _ => true,           // Timeout or got data = alive
    }
}

use crate::harness::TestDaemon;

/// Time to wait after connecting to daemon before sending data.
/// The daemon's TCPServerInterface needs time to initialize client connections.
pub const DAEMON_SETTLE_TIME: Duration = Duration::from_millis(500);

/// Time to wait for daemon to process a packet and update its state.
pub const DAEMON_PROCESS_TIME: Duration = Duration::from_millis(200);

/// Build and send a valid announce to a daemon, returning the destination hash.
/// This is a convenience wrapper that handles stream creation and timing.
pub async fn send_announce_to_daemon(
    daemon: &TestDaemon,
    app_name: &str,
    aspects: &[&str],
    app_data: &[u8],
) -> DestinationHash {
    let mut stream = TcpStream::connect(daemon.rns_addr())
        .await
        .expect("Failed to connect to daemon");

    // Wait for interface to settle
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    let (dest_hash, _dest) =
        build_and_send_announce(&mut stream, app_name, aspects, app_data).await;

    // Wait for daemon to process the announce
    tokio::time::sleep(DAEMON_PROCESS_TIME).await;

    dest_hash
}

/// Build and send raw announce bytes to a daemon.
pub async fn send_raw_to_daemon(daemon: &TestDaemon, raw: &[u8]) {
    let mut stream = TcpStream::connect(daemon.rns_addr())
        .await
        .expect("Failed to connect to daemon");

    // Wait for interface to settle
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    send_framed(&mut stream, raw).await;

    // Wait for daemon to process
    tokio::time::sleep(DAEMON_PROCESS_TIME).await;
}

/// Connect to daemon and wait for interface to settle.
pub async fn connect_to_daemon(daemon: &TestDaemon) -> TcpStream {
    let stream = TcpStream::connect(daemon.rns_addr())
        .await
        .expect("Failed to connect to daemon");

    // Wait for interface to settle
    tokio::time::sleep(DAEMON_SETTLE_TIME).await;

    stream
}

/// Wait for a proof packet for a specific link ID.
pub async fn receive_proof_for_link(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link_id: &LinkId,
    timeout_duration: Duration,
) -> Option<Packet> {
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(100), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Proof
                                && pkt.destination_hash == *link_id.as_bytes()
                            {
                                return Some(pkt);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Wait for a proof packet for a specific link ID, returning raw bytes.
///
/// Unlike `receive_proof_for_link` which returns a parsed `Packet`, this
/// returns the raw deframed bytes suitable for `NodeCore::handle_packet()`.
pub async fn receive_raw_proof_for_link(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link_id: &LinkId,
    timeout_duration: Duration,
) -> Option<Vec<u8>> {
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(100), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Proof
                                && pkt.destination_hash == *link_id.as_bytes()
                            {
                                return Some(data);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Receive a data packet on a link and decrypt it.
/// Returns the decrypted data if successful.
pub async fn receive_link_data(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link: &leviculum_core::link::Link,
    timeout_duration: Duration,
) -> Option<Vec<u8>> {
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(100), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Data
                                && pkt.destination_hash == *link.id().as_bytes()
                                && pkt.context == PacketContext::None
                            {
                                let mut decrypted = vec![0u8; pkt.data.len()];
                                if let Ok(len) = link.decrypt(pkt.data.as_slice(), &mut decrypted) {
                                    decrypted.truncate(len);
                                    return Some(decrypted);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

// =========================================================================
// Link establishment helpers
// =========================================================================

/// Wait for a LINK_REQUEST packet addressed to our destination.
///
/// Returns the raw packet bytes and calculated link_id if found.
pub async fn wait_for_link_request(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    dest_hash: &DestinationHash,
    timeout_duration: Duration,
) -> Option<(Vec<u8>, [u8; TRUNCATED_HASHBYTES])> {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::LinkRequest
                                && pkt.destination_hash == *dest_hash.as_bytes()
                            {
                                let link_id = Link::calculate_link_id(&data);
                                return Some((data, link_id.into_bytes()));
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }

    None
}

/// Wait for an RTT packet on a link.
pub async fn wait_for_rtt_packet(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link_id: &LinkId,
    timeout_duration: Duration,
) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Data
                                && pkt.destination_hash == *link_id.as_bytes()
                                && pkt.context == PacketContext::Lrrtt
                            {
                                return Some(pkt.data.as_slice().to_vec());
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }

    None
}

/// Wait for a DATA packet on a link (context = None).
pub async fn wait_for_data_packet(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link_id: &LinkId,
    timeout_duration: Duration,
) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Data
                                && pkt.destination_hash == *link_id.as_bytes()
                                && pkt.context == PacketContext::None
                            {
                                return Some(data);
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }

    None
}

/// Wait for any Data packet on a link (any context: None, Channel, etc.).
///
/// Unlike `wait_for_data_packet` which filters for `PacketContext::None`,
/// this accepts any Data packet addressed to the link_id. Use this for
/// Rust-to-Rust tests where `send_on_link()` goes through the Channel
/// (producing packets with `PacketContext::Channel`).
pub async fn wait_for_link_data_packet(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link_id: &LinkId,
    timeout_duration: Duration,
) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Data
                                && pkt.destination_hash == *link_id.as_bytes()
                            {
                                return Some(data);
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }

    None
}

/// Wait for a KEEPALIVE packet on a link.
/// Returns the raw packet data if received.
pub async fn wait_for_keepalive_packet(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link_id: &LinkId,
    timeout_duration: Duration,
) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Data
                                && pkt.destination_hash == *link_id.as_bytes()
                                && pkt.context == PacketContext::Keepalive
                            {
                                return Some(data);
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }

    None
}

/// Wait for a LINKCLOSE packet on a link.
/// Returns the raw packet data if received.
pub async fn wait_for_close_packet(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    link_id: &LinkId,
    timeout_duration: Duration,
) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    while tokio::time::Instant::now() < deadline {
        let remaining = deadline - tokio::time::Instant::now();

        match timeout(remaining, stream.read(&mut buf)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buf[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Data
                                && pkt.destination_hash == *link_id.as_bytes()
                                && pkt.context == PacketContext::LinkClose
                            {
                                return Some(data);
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) | Err(_) => continue,
        }
    }

    None
}

// =========================================================================
// Transport layer test helpers
// =========================================================================

/// Wait for a daemon to have a path entry for the given destination hash.
/// Polls the daemon's path table at regular intervals until the path appears
/// or the timeout expires.
pub async fn wait_for_path_on_daemon(
    daemon: &TestDaemon,
    dest_hash: &DestinationHash,
    timeout_duration: Duration,
) -> bool {
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(500);

    while start.elapsed() < timeout_duration {
        if daemon.has_path(dest_hash).await {
            return true;
        }
        tokio::time::sleep(poll_interval).await;
    }

    // Final check
    daemon.has_path(dest_hash).await
}

/// Build a raw PATH_REQUEST packet.
///
/// Path requests are Data packets sent to the well-known "rnstransport.path.request"
/// PLAIN destination. The payload format is:
///   dest_hash(16) + request_tag(16)
///
/// Returns the raw packed bytes ready for HDLC framing.
pub fn build_path_request_raw(requested_dest_hash: &[u8; TRUNCATED_HASHBYTES]) -> Vec<u8> {
    use rand_core::RngCore;

    // Compute path request destination hash (PLAIN destination: full_hash(name_hash)[:16])
    let name_hash = Destination::compute_name_hash("rnstransport", &["path", "request"]);
    let path_request_dest = truncated_hash(&name_hash);

    // Generate random request tag
    let mut tag = [0u8; TRUNCATED_HASHBYTES];
    OsRng.fill_bytes(&mut tag);

    // Build payload: requested_dest_hash(16) + tag(16)
    let mut payload = Vec::with_capacity(32);
    payload.extend_from_slice(requested_dest_hash);
    payload.extend_from_slice(&tag);

    // Build packet
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Plain,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: path_request_dest,
        context: PacketContext::None,
        data: PacketData::Owned(payload),
    };

    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).expect("Failed to pack path request");
    buf[..len].to_vec()
}

/// Build a raw PATH_REQUEST packet with a specific tag.
///
/// This variant allows specifying the request tag, which is useful for
/// testing deduplication behavior (same dest_hash + same tag = duplicate).
pub fn build_path_request_raw_with_tag(
    requested_dest_hash: &[u8; TRUNCATED_HASHBYTES],
    tag: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<u8> {
    let name_hash = Destination::compute_name_hash("rnstransport", &["path", "request"]);
    let path_request_dest = truncated_hash(&name_hash);

    let mut payload = Vec::with_capacity(32);
    payload.extend_from_slice(requested_dest_hash);
    payload.extend_from_slice(tag);

    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Plain,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: path_request_dest,
        context: PacketContext::None,
        data: PacketData::Owned(payload),
    };

    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).expect("Failed to pack path request");
    buf[..len].to_vec()
}

/// Wait for any announce with specific destination hash and return route info.
/// Similar to wait_for_any_announce_with_route_info but filters by dest_hash.
pub async fn wait_for_announce_for_dest(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    dest_hash: &DestinationHash,
    timeout_duration: Duration,
) -> Option<AnnounceRouteInfo> {
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(100), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Announce
                                && pkt.destination_hash == *dest_hash.as_bytes()
                            {
                                return Some(AnnounceRouteInfo {
                                    transport_id: pkt.transport_id,
                                    hops: pkt.hops,
                                    packet: pkt,
                                    raw_data: data,
                                });
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Set up a Rust destination on a daemon connection and announce it.
/// Returns `(destination, public_key_hex)`.
pub async fn setup_rust_destination(
    stream: &mut TcpStream,
    app_name: &str,
    aspects: &[&str],
    app_data: &[u8],
) -> (Destination, String) {
    let identity = Identity::generate(&mut OsRng);
    let public_key_hex = hex::encode(identity.public_key_bytes());

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        app_name,
        aspects,
    )
    .expect("Failed to create destination");

    let packet = dest
        .announce(Some(app_data), &mut OsRng, now_ms(), now_ms() / 1000)
        .expect("Failed to create announce");

    let mut raw_packet = [0u8; MTU];
    let size = packet.pack(&mut raw_packet).expect("Failed to pack packet");

    send_framed(stream, &raw_packet[..size]).await;

    (dest, public_key_hex)
}

// =========================================================================
// Node-level event helpers
// =========================================================================

use leviculum_core::node::NodeEvent;
use leviculum_std::EventReceiver;

/// Generic event-loop helper: drain events until `predicate` returns `Some(T)` or timeout.
pub async fn wait_for_event<T>(
    event_rx: &mut EventReceiver,
    timeout: Duration,
    mut predicate: impl FnMut(NodeEvent) -> Option<T>,
) -> Option<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline - tokio::time::Instant::now();
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(event)) => {
                if let Some(result) = predicate(event) {
                    return Some(result);
                }
            }
            Ok(None) | Err(_) => return None,
        }
    }
}

/// Wait for a `LinkDataReceived` or `MessageReceived` event for a specific link ID.
/// Drains other events while waiting.
pub async fn wait_for_data_event(
    event_rx: &mut EventReceiver,
    link_id: &LinkId,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let link_id = *link_id;
    wait_for_event(event_rx, timeout, move |event| match event {
        NodeEvent::LinkDataReceived { link_id: id, data } if id == link_id => Some(data),
        NodeEvent::MessageReceived {
            link_id: id, data, ..
        } if id == link_id => Some(data),
        _ => None,
    })
    .await
}

/// Wait for a `LinkClosed` event for a specific link ID.
/// Drains other events while waiting.
pub async fn wait_for_link_closed_event(
    event_rx: &mut EventReceiver,
    link_id: &LinkId,
    timeout: Duration,
) -> bool {
    let link_id = *link_id;
    wait_for_event(event_rx, timeout, move |event| match event {
        NodeEvent::LinkClosed { link_id: id, .. } if id == link_id => Some(()),
        _ => None,
    })
    .await
    .is_some()
}

/// Wait for a `LinkEstablished` event for a specific link ID.
/// Drains other events while waiting.
pub async fn wait_for_link_established(
    event_rx: &mut EventReceiver,
    link_id: &LinkId,
    timeout: Duration,
) -> bool {
    let link_id = *link_id;
    wait_for_event(event_rx, timeout, move |event| match event {
        NodeEvent::LinkEstablished { link_id: id, .. } if id == link_id => Some(()),
        _ => None,
    })
    .await
    .is_some()
}

/// Wait for a responder-side `LinkEstablished` event (`is_initiator == false`),
/// returning the established link_id. Drains other events while waiting.
///
/// Incoming links are accepted and proved automatically by the core (Python
/// parity), so a responder no longer sees a separate request event: it learns
/// its link_id from this establishment event and then mints a writable handle
/// with `node.link_handle(&link_id)`.
pub async fn wait_for_responder_established_link(
    event_rx: &mut EventReceiver,
    timeout: Duration,
) -> Option<LinkId> {
    wait_for_event(event_rx, timeout, |event| match event {
        NodeEvent::LinkEstablished {
            link_id,
            is_initiator: false,
            ..
        } => Some(link_id),
        _ => None,
    })
    .await
}

/// Wait for a `LinkIdentified` event for a specific link ID.
/// Returns the identity hash. Drains other events while waiting.
pub async fn wait_for_link_identified(
    event_rx: &mut EventReceiver,
    link_id: &LinkId,
    timeout: Duration,
) -> Option<[u8; leviculum_core::constants::TRUNCATED_HASHBYTES]> {
    let link_id = *link_id;
    wait_for_event(event_rx, timeout, move |event| match event {
        NodeEvent::LinkIdentified {
            link_id: id,
            identity_hash,
        } if id == link_id => Some(identity_hash),
        _ => None,
    })
    .await
}

/// Drain `event_rx` for `LinkDeliveryConfirmed` events until `expected_count` are
/// collected or `timeout` expires. Returns the count received.
pub async fn wait_for_delivery_confirmations(
    event_rx: &mut EventReceiver,
    expected_count: usize,
    timeout: Duration,
) -> usize {
    let mut count = 0;
    let deadline = tokio::time::Instant::now() + timeout;
    while count < expected_count {
        let remaining = deadline - tokio::time::Instant::now();
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(NodeEvent::LinkDeliveryConfirmed { .. })) => {
                count += 1;
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => break,
        }
    }
    count
}

/// Wait for a `ResourceCompleted` event on the receiver side.
/// Returns `(data, metadata)` on success, or `None` on timeout.
pub async fn wait_for_resource_completed(
    event_rx: &mut EventReceiver,
    link_id: &LinkId,
    timeout: Duration,
) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let link_id = *link_id;
    wait_for_event(event_rx, timeout, move |event| match event {
        NodeEvent::ResourceCompleted {
            link_id: id,
            data,
            metadata,
            is_sender,
            ..
        } if id == link_id && !is_sender => Some((data, metadata)),
        _ => None,
    })
    .await
}

/// Wait for a `ResourceCompleted` event on the sender side.
/// Returns `true` if the sender-side completion was received before timeout.
pub async fn wait_for_resource_sender_completed(
    event_rx: &mut EventReceiver,
    link_id: &LinkId,
    timeout: Duration,
) -> bool {
    let link_id = *link_id;
    wait_for_event(event_rx, timeout, move |event| match event {
        NodeEvent::ResourceCompleted {
            link_id: id,
            is_sender,
            ..
        } if id == link_id && is_sender => Some(()),
        _ => None,
    })
    .await
    .is_some()
}

// =========================================================================
// Non-event helpers consolidated from test files
// =========================================================================

/// Decode a hex destination hash string into a DestinationHash.
pub fn parse_dest_hash(hex_str: &str) -> DestinationHash {
    let bytes: [u8; TRUNCATED_HASHBYTES] = hex::decode(hex_str).unwrap().try_into().unwrap();
    DestinationHash::new(bytes)
}

/// Extract the Ed25519 signing key (last 32 bytes) from a 64-byte public key hex string.
pub fn extract_signing_key(public_key_hex: &str) -> [u8; 32] {
    let pub_key_bytes = hex::decode(public_key_hex).unwrap();
    pub_key_bytes[32..64].try_into().unwrap()
}

/// Poll `node.has_path()` every 500ms until it returns true or timeout expires.
pub async fn wait_for_path_on_node(
    node: &leviculum_std::driver::ReticulumNode,
    dest_hash: &DestinationHash,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if node.has_path(dest_hash) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Wait until `has_path` holds, re-driving the daemon announce on a fixed
/// cadence.
///
/// A Python `Destination.announce()` is a one-shot push to the interfaces
/// connected at that instant; it is not re-sent. Under a CPU-starved
/// parallel interop run the daemon can be scheduled through a relay or
/// fresh-connection setup window and drop that lone announce, after which
/// nothing re-sends it inside a fixed wait budget, so the learner never
/// learns the path and a timing-sensitive test times out (Codeberg #103).
/// The failure is bimodal: the path is learned in well under a second or
/// never arrives at all, so a longer passive wait does not help; the
/// announce has to be re-driven. Re-announcing mirrors how a live Python
/// peer periodically re-announces, so the path-learn tolerates load instead
/// of depending on a single announce landing in a fixed window.
///
/// `has_path` is any synchronous learner-side predicate (typically
/// `|| node.has_path(&dest_hash)`); `daemon` is the announcer whose
/// `announce_destination` RPC is re-driven with the same `app_data`.
pub async fn wait_for_path_reannounce<F: Fn() -> bool>(
    has_path: F,
    daemon: &TestDaemon,
    dest_hash_hex: &str,
    app_data: &[u8],
    budget: Duration,
) -> bool {
    const REANNOUNCE_EVERY: Duration = Duration::from_secs(3);
    let start = std::time::Instant::now();
    // The caller already fired the first announce; space the retries.
    let mut last_announce = std::time::Instant::now();
    while start.elapsed() < budget {
        if has_path() {
            return true;
        }
        if last_announce.elapsed() >= REANNOUNCE_EVERY {
            let _ = daemon.announce_destination(dest_hash_hex, app_data).await;
            last_announce = std::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    has_path()
}

/// Like [`wait_for_path_reannounce`] but for a daemon-side learner whose
/// `has_path` is an async RPC. Polls `learner.has_path(dest_hash)` while
/// re-driving `announcer.announce_destination` on the same fixed cadence,
/// hardening daemon-to-daemon path-learn against a lost one-shot announce
/// under load (Codeberg #103/#105).
pub async fn wait_for_path_reannounce_on_daemon(
    learner: &TestDaemon,
    dest_hash: &DestinationHash,
    announcer: &TestDaemon,
    dest_hash_hex: &str,
    app_data: &[u8],
    budget: Duration,
) -> bool {
    const REANNOUNCE_EVERY: Duration = Duration::from_secs(3);
    let start = std::time::Instant::now();
    // The caller already fired the first announce; space the retries.
    let mut last_announce = std::time::Instant::now();
    while start.elapsed() < budget {
        if learner.has_path(dest_hash).await {
            return true;
        }
        if last_announce.elapsed() >= REANNOUNCE_EVERY {
            let _ = announcer
                .announce_destination(dest_hash_hex, app_data)
                .await;
            last_announce = std::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    learner.has_path(dest_hash).await
}

/// Like [`wait_for_path_reannounce`] but the announcer is a Rust
/// [`ReticulumNode`](leviculum_std::driver::ReticulumNode) announcing its own
/// destination and the learner is a daemon (async `has_path`). A Rust node's
/// `announce_destination` is likewise a one-shot dispatch with no scheduled
/// retransmit for its own destination, so the same #103/#105 loss window
/// applies; re-driving it on the fixed cadence hardens the daemon-side
/// path-learn against a lost announce under load.
pub async fn wait_for_node_reannounce_on_daemon(
    learner: &TestDaemon,
    dest_hash: &DestinationHash,
    announcer: &leviculum_std::driver::ReticulumNode,
    app_data: &[u8],
    budget: Duration,
) -> bool {
    const REANNOUNCE_EVERY: Duration = Duration::from_secs(3);
    let start = std::time::Instant::now();
    // The caller already fired the first announce; space the retries.
    let mut last_announce = std::time::Instant::now();
    while start.elapsed() < budget {
        if learner.has_path(dest_hash).await {
            return true;
        }
        if last_announce.elapsed() >= REANNOUNCE_EVERY {
            let _ = announcer
                .announce_destination(dest_hash, Some(app_data))
                .await;
            last_announce = std::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    learner.has_path(dest_hash).await
}

/// Like [`wait_for_node_reannounce_on_daemon`] but for a node-side learner
/// whose `has_path` is synchronous (`|| node.has_path(&dest_hash)`), with a
/// Rust [`ReticulumNode`](leviculum_std::driver::ReticulumNode) announcer
/// re-driving its own destination on the fixed cadence (#103/#105).
pub async fn wait_for_node_reannounce<F: Fn() -> bool>(
    has_path: F,
    announcer: &leviculum_std::driver::ReticulumNode,
    dest_hash: &DestinationHash,
    app_data: &[u8],
    budget: Duration,
) -> bool {
    const REANNOUNCE_EVERY: Duration = Duration::from_secs(3);
    let start = std::time::Instant::now();
    // The caller already fired the first announce; space the retries.
    let mut last_announce = std::time::Instant::now();
    while start.elapsed() < budget {
        if has_path() {
            return true;
        }
        if last_announce.elapsed() >= REANNOUNCE_EVERY {
            let _ = announcer
                .announce_destination(dest_hash, Some(app_data))
                .await;
            last_announce = std::time::Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    has_path()
}

/// Poll a daemon for messages matching a prefix, collecting unique messages.
/// Returns when `expected_count` unique messages are found or the deadline expires.
pub async fn collect_messages(
    daemon: &TestDaemon,
    prefix: &str,
    expected_count: usize,
    timeout: Duration,
) -> HashSet<String> {
    let mut received = HashSet::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while received.len() < expected_count && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let packets = daemon.get_received_packets().await.unwrap_or_default();
        for p in &packets {
            let s = String::from_utf8_lossy(&p.data);
            if s.starts_with(prefix) {
                received.insert(s.to_string());
            }
        }
    }
    received
}

/// Receive an announce packet from the daemon stream.
/// Returns the parsed Packet and raw bytes on success.
pub async fn receive_announce_from_daemon(
    stream: &mut TcpStream,
    deframer: &mut Deframer,
    timeout_duration: Duration,
) -> Option<(Packet, Vec<u8>)> {
    let mut buffer = [0u8; 2048];
    let start = std::time::Instant::now();

    while start.elapsed() < timeout_duration {
        match timeout(Duration::from_millis(100), stream.read(&mut buffer)).await {
            Ok(Ok(0)) => return None,
            Ok(Ok(n)) => {
                let results = deframer.process(&buffer[..n]);
                for result in results {
                    if let DeframeResult::Frame(data) = result {
                        if let Ok(pkt) = Packet::unpack(&data) {
                            if pkt.flags.packet_type == PacketType::Announce {
                                return Some((pkt, data));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Wait for the Python daemon to show a link in its link table.
/// Polls `get_links()` every 500ms.
pub async fn wait_for_link_on_daemon(
    daemon: &TestDaemon,
    link_hash: &str,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Ok(links) = daemon.get_links().await {
            if links.contains_key(link_hash) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

// =========================================================================
// MTU test constants and helpers
// =========================================================================

/// TCP interface hardware MTU. Not `TCPInterface.HW_MTU = 262144`: that class
/// value never reaches the wire, because `interface_post_init` runs
/// `optimise_mtu()` over the interface bitrate before the interface carries
/// traffic (Reticulum.py:948). With `TCPServerInterface.BITRATE_GUESS` that
/// derivation gives 16384 on Reticulum >= 1.5.2 and 8192 on <= 1.5.0, and
/// lnsd signals the same derived value since Codeberg #355.
/// Used when configuring Rust node interfaces via `set_interface_hw_mtu()`.
pub const TCP_HW_MTU: u32 = 16384;

/// UDP interface hardware MTU (matches Python UDPInterface.HW_MTU)
pub const UDP_HW_MTU: u32 = 1064;

/// Negotiated MTU when connecting through a Python daemon over TCP.
///
/// Python's `optimise_mtu()` sets HW_MTU based on measured bitrate. With the
/// default `BITRATE_GUESS = 10_000_000` (10 Mbps), the check `> 10_000_000`
/// is strictly-greater, so it falls through to `> 5_000_000 → HW_MTU = 8192`.
/// The daemon's Reticulum.__init__ calls `optimise_mtu()` on the server interface,
/// and spawned client interfaces inherit the server's HW_MTU.
pub const DAEMON_TCP_NEGOTIATED_MTU: u32 = 8192;

/// Encrypted link MDU for daemon TCP: floor((8192 - 1 - 19 - 48) / 16) * 16 - 1 = 8111
pub const DAEMON_TCP_LINK_MDU: usize = 8111;

/// Channel overhead (message header): 6 bytes
pub const CHANNEL_OVERHEAD: usize = 6;

/// Maximum channel payload over a daemon TCP link: 8111 - 6 = 8105
pub const DAEMON_TCP_MAX_CHANNEL_PAYLOAD: usize = DAEMON_TCP_LINK_MDU - CHANNEL_OVERHEAD;

/// Encrypted link MDU for UDP: floor((1064 - 1 - 19 - 48) / 16) * 16 - 1 = 991
pub const UDP_LINK_MDU: usize = 991;

/// Maximum channel payload over UDP link: 991 - 6 = 985
pub const UDP_MAX_CHANNEL_PAYLOAD: usize = UDP_LINK_MDU - CHANNEL_OVERHEAD;

/// Negotiated MTU when connecting through a Python daemon over UDP.
///
/// Python's UDPInterface has `AUTOCONFIGURE_MTU=False` and `FIXED_MTU=False`,
/// so `Transport.next_hop_interface_hw_mtu()` returns None and
/// `Transport.inbound()` clamps the link MTU to `RNS.Reticulum.MTU = 500`.
/// Even though UDPInterface.HW_MTU = 1064, the link-level negotiation
/// always settles on the base protocol MTU for UDP interop with Python.
pub const DAEMON_UDP_NEGOTIATED_MTU: u32 = 500;

/// Encrypted link MDU for daemon UDP: floor((500 - 1 - 19 - 48) / 16) * 16 - 1 = 431
pub const DAEMON_UDP_LINK_MDU: usize = 431;

/// Maximum channel payload over a daemon UDP link: 431 - 6 = 425
pub const DAEMON_UDP_MAX_CHANNEL_PAYLOAD: usize = DAEMON_UDP_LINK_MDU - CHANNEL_OVERHEAD;

/// Direct Rust-to-Rust TCP link MDU (no daemon clamping):
/// floor((16384 - 1 - 19 - 48) / 16) * 16 - 1 = 16303
pub const DIRECT_TCP_LINK_MDU: usize = 16303;

/// Maximum channel payload over direct TCP link: 16303 - 6 = 16297.
///
/// The channel envelope's u16 length field (max 65535) no longer binds here:
/// since the TCP HW_MTU is derived from the bitrate (#355), the link MDU is
/// the smaller of the two limits, so the payload is min(link_mdu - overhead,
/// u16::MAX) = 16297.
pub const DIRECT_TCP_MAX_CHANNEL_PAYLOAD: usize = DIRECT_TCP_LINK_MDU - CHANNEL_OVERHEAD;

/// Generate a deterministic test payload of the given size.
///
/// Uses a SHA-256 chain: block[0] = sha256(seed), block[n] = sha256(block[n-1]).
/// The first 32 bytes are the first hash; subsequent 32-byte blocks are chained.
/// The payload is truncated to the exact requested size.
pub fn generate_test_payload(size: usize) -> Vec<u8> {
    use leviculum_core::crypto::sha256;

    let mut result = Vec::with_capacity(size);
    let mut block = sha256(b"mtu_test_payload_seed");

    while result.len() < size {
        let remaining = size - result.len();
        let to_copy = remaining.min(32);
        result.extend_from_slice(&block[..to_copy]);
        if result.len() < size {
            block = sha256(&block);
        }
    }

    result
}

/// Verify a test payload matches the deterministic SHA-256 chain.
///
/// Returns true if the data matches `generate_test_payload(data.len())`.
pub fn verify_test_payload(data: &[u8]) -> bool {
    let expected = generate_test_payload(data.len());
    data == expected.as_slice()
}

/// Create a Transport for testing (sans-I/O, no interfaces registered).
/// Returns `(transport, interface_index)`.
pub fn create_test_transport() -> (
    leviculum_core::transport::Transport<TestClock, leviculum_core::MemoryStorage>,
    usize,
) {
    let clock = TestClock;
    let identity = Identity::generate(&mut OsRng);
    let config = leviculum_core::transport::TransportConfig::default();
    let transport = leviculum_core::transport::Transport::new(
        config,
        clock,
        leviculum_core::MemoryStorage::with_defaults(),
        identity,
    );
    (transport, 0)
}

/// Build a Rust node connected to a daemon via TCP, ready for single-packet
/// operations. Returns `(node, event_rx, storage)`; the node is started and
/// the storage guard must outlive the test.
pub async fn build_rust_node(
    daemon: &crate::harness::TestDaemon,
) -> (
    leviculum_std::driver::ReticulumNode,
    EventReceiver,
    tempfile::TempDir,
) {
    let storage = temp_storage("build_rust_node", "node");
    let mut node = leviculum_std::driver::ReticulumNodeBuilder::new()
        .add_tcp_client(daemon.rns_addr())
        .storage_path(storage.path().to_path_buf())
        .build()
        .await
        .expect("Failed to build node");

    let event_rx = node.take_event_receiver().unwrap();
    node.start().await.expect("Failed to start node");

    tokio::time::sleep(Duration::from_secs(1)).await;

    (node, event_rx, storage)
}

/// Recursively remove a config directory, ignoring errors.
pub fn cleanup_config_dir(path: &std::path::Path) {
    let _ = std::fs::remove_dir_all(path);
}

/// Send a `create_link` JSON-RPC command to a daemon using a raw TCP
/// connection. Callable from a spawned task without borrowing `TestDaemon`.
/// Returns the hex link hash on success.
pub async fn create_link_raw(
    cmd_addr: std::net::SocketAddr,
    dest_hash: &str,
    dest_key: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    let cmd = serde_json::json!({
        "method": "create_link",
        "params": {
            "dest_hash": dest_hash,
            "dest_key": dest_key,
            "timeout": timeout_secs,
        }
    });

    let mut stream = TcpStream::connect(cmd_addr)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;

    stream
        .write_all(cmd.to_string().as_bytes())
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    stream
        .shutdown()
        .await
        .map_err(|e| format!("shutdown failed: {e}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .map_err(|e| format!("read failed: {e}"))?;

    let resp: serde_json::Value =
        serde_json::from_slice(&response).map_err(|e| format!("parse failed: {e}"))?;

    if let Some(error) = resp.get("error") {
        return Err(format!("create_link error: {error}"));
    }

    resp.get("result")
        .and_then(|r| r.get("link_hash"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "missing link_hash in response".to_string())
}
