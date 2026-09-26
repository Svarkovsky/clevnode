//! AutoInterface, zero-configuration LAN discovery via IPv6 multicast
//!
//! Nodes on the same LAN discover each other via IPv6 multicast and
//! communicate over UDP. Matches Python Reticulum's `AutoInterface`.
//!
//! Linux only. All protocol logic is in `leviculum-std` (no core changes).

pub(crate) mod orchestrator;

use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::time::Instant;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket;

use leviculum_core::crypto::full_hash;

// Constants (match Python exactly)
pub(crate) const DEFAULT_GROUP_ID: &[u8] = b"reticulum";
pub(crate) const DEFAULT_DISCOVERY_PORT: u16 = 29716;
pub(crate) const DEFAULT_DATA_PORT: u16 = 42671;

/// Peer timeout, peer removed if not heard for this long
pub(crate) const PEERING_TIMEOUT_SECS: f64 = 22.0;
/// Multicast announce interval
pub(crate) const ANNOUNCE_INTERVAL_SECS: f64 = 1.6;
/// Peer maintenance job interval
pub(crate) const PEER_JOB_INTERVAL_SECS: f64 = 4.0;
/// Multicast echo timeout, carrier lost if no self-echo for this long
pub(crate) const MCAST_ECHO_TIMEOUT_SECS: f64 = 6.5;

/// Deduplication cache capacity (number of entries)
pub(crate) const DEDUP_CACHE_SIZE: usize = 48;
/// Deduplication cache TTL in seconds
pub(crate) const DEDUP_TTL_SECS: f64 = 0.75;

/// Hardware MTU for AutoInterface (matches Python `HW_MTU = 1196`)
pub(crate) const AUTO_HW_MTU: u32 = 1196;

/// Multicast address type: "1" = temporary
const MULTICAST_ADDRESS_TYPE: &str = "1";

// Scope mapping
/// IPv6 multicast scope values matching Python's AutoInterface
fn scope_to_byte(scope: &str) -> &'static str {
    match scope.to_lowercase().as_str() {
        "link" => "2",
        "admin" => "4",
        "site" => "5",
        "organisation" => "8",
        "global" => "e",
        _ => "2", // default to link-local
    }
}

// Configuration
/// Configuration for an AutoInterface instance
#[derive(Debug, Clone)]
pub struct AutoInterfaceConfig {
    pub group_id: Vec<u8>,
    pub discovery_port: u16,
    pub data_port: u16,
    pub discovery_scope: String,
    /// Comma-separated whitelist of NIC names (None = all)
    pub allowed_devices: Option<String>,
    /// Comma-separated blacklist of NIC names
    pub ignored_devices: Option<String>,
    /// Enable multicast loopback (`IPV6_MULTICAST_LOOP`). This is the carrier
    /// self-echo mechanism: the host receiving its own discovery multicast is
    /// how AutoInterface detects a live carrier. Defaults to `true`, matching
    /// Python-RNS (which never sets the sockopt and inherits the OS default ON
    /// on Linux) and giving reliable self-echo even on bridges that stop
    /// reflecting the group. Set to `false` to opt out.
    pub multicast_loopback: bool,
}

impl Default for AutoInterfaceConfig {
    fn default() -> Self {
        Self {
            group_id: DEFAULT_GROUP_ID.to_vec(),
            discovery_port: DEFAULT_DISCOVERY_PORT,
            data_port: DEFAULT_DATA_PORT,
            discovery_scope: "link".to_string(),
            allowed_devices: None,
            ignored_devices: None,
            multicast_loopback: true,
        }
    }
}

// NIC enumeration
/// A network interface adopted for AutoInterface use
#[derive(Debug, Clone)]
pub struct AdoptedNic {
    /// Interface name (e.g. "eth0", "wlan0")
    pub name: String,
    /// IPv6 link-local address on this interface
    pub link_local: Ipv6Addr,
    /// OS interface index (needed for multicast join and scope_id)
    pub index: u32,
}

/// Enumerate NICs suitable for AutoInterface.
///
/// Filters:
/// - Must have an IPv6 link-local address (fe80::)
/// - Not loopback
/// - Respects `allowed_devices` whitelist and `ignored_devices` blacklist
/// - Skips docker/veth/br- virtual interfaces on Linux
pub fn enumerate_nics(config: &AutoInterfaceConfig) -> Vec<AdoptedNic> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to enumerate network interfaces: {}", e);
            return Vec::new();
        }
    };

    let allowed: Option<Vec<&str>> = config
        .allowed_devices
        .as_ref()
        .map(|s| s.split(',').map(|d| d.trim()).collect());
    let ignored: Vec<&str> = config
        .ignored_devices
        .as_deref()
        .map(|s| s.split(',').map(|d| d.trim()).collect())
        .unwrap_or_default();

    // Default ignored prefixes on Linux (virtual interfaces)
    let default_ignored_prefixes = ["docker", "veth", "br-"];

    let mut adopted = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    tracing::debug!("AutoInterface: if_addrs returned {} entries", ifaces.len());

    for iface in &ifaces {
        tracing::trace!(
            "AutoInterface: evaluating iface '{}' addr={} loopback={}",
            iface.name,
            iface.addr.ip(),
            iface.is_loopback()
        );

        // Skip loopback
        if iface.is_loopback() {
            tracing::trace!("AutoInterface: '{}' skipped (loopback)", iface.name);
            continue;
        }

        // Only IPv6 link-local (fe80::)
        let addr = match iface.addr.ip() {
            std::net::IpAddr::V6(v6) if is_link_local_v6(&v6) => v6,
            other => {
                tracing::trace!(
                    "AutoInterface: '{}' skipped (not link-local v6: {})",
                    iface.name,
                    other
                );
                continue;
            }
        };

        let name = &iface.name;

        // Skip default-ignored virtual interfaces
        if default_ignored_prefixes.iter().any(|p| name.starts_with(p)) {
            tracing::trace!("AutoInterface: '{}' skipped (default-ignored prefix)", name);
            continue;
        }

        // Apply whitelist
        if let Some(ref allowed_list) = allowed {
            if !allowed_list.iter().any(|d| name == *d) {
                continue;
            }
        }

        // Apply blacklist
        if ignored.iter().any(|d| name == *d) {
            continue;
        }

        // Only take first link-local per interface name
        if !seen_names.insert(name.clone()) {
            continue;
        }

        // Get OS interface index
        let index = name_to_index(name);
        if index == 0 {
            tracing::warn!("Could not get interface index for {}, skipping", name);
            continue;
        }

        adopted.push(AdoptedNic {
            name: name.clone(),
            link_local: addr,
            index,
        });
    }

    adopted
}

/// Check if an IPv6 address is link-local (fe80::/10)
fn is_link_local_v6(addr: &Ipv6Addr) -> bool {
    let octets = addr.octets();
    octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80
}

/// Get the OS interface index for a NIC name.
/// Returns 0 on failure.
#[cfg(unix)]
fn name_to_index(name: &str) -> u32 {
    use std::ffi::CString;
    let Ok(c_name) = CString::new(name) else {
        return 0;
    };
    // SAFETY: if_nametoindex is a standard POSIX function that takes a
    // null-terminated string and returns the interface index (or 0 on error).
    unsafe { libc::if_nametoindex(c_name.as_ptr()) }
}

/// Get the OS interface index for a NIC name. Returns 0 on failure.
///
/// Windows exposes the same `if_nametoindex` C function from `iphlpapi` (same
/// signature and 0-on-error contract as the POSIX one used above).
#[cfg(windows)]
fn name_to_index(name: &str) -> u32 {
    use std::ffi::CString;
    let Ok(c_name) = CString::new(name) else {
        return 0;
    };
    #[link(name = "iphlpapi")]
    extern "system" {
        fn if_nametoindex(interface_name: *const std::os::raw::c_char) -> u32;
    }
    // SAFETY: same contract as POSIX `if_nametoindex` — null-terminated input,
    // returns the interface index or 0 on error.
    unsafe { if_nametoindex(c_name.as_ptr()) }
}

// Multicast address derivation
/// Derive the IPv6 multicast discovery address from a group ID and scope.
///
/// Matches Python's AutoInterface multicast address derivation:
/// - SHA-256 hash the group_id
/// - Take bytes [2..14] as big-endian 16-bit words
/// - Format as `ff{type}{scope}:0:{word1}:{word2}:{word3}:{word4}:{word5}:{word6}`
pub(crate) fn derive_multicast_address(group_id: &[u8], scope: &str) -> io::Result<Ipv6Addr> {
    let g = full_hash(group_id);
    let scope_byte = scope_to_byte(scope);

    // Build the address string matching Python's format exactly.
    // Python: gt = "0" then ":"+format(g[3]+(g[2]<<8)) for pairs at indices 2..14
    let addr_str = format!(
        "ff{}{}:0:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        MULTICAST_ADDRESS_TYPE,
        scope_byte,
        u16::from(g[3]) + (u16::from(g[2]) << 8),
        u16::from(g[5]) + (u16::from(g[4]) << 8),
        u16::from(g[7]) + (u16::from(g[6]) << 8),
        u16::from(g[9]) + (u16::from(g[8]) << 8),
        u16::from(g[11]) + (u16::from(g[10]) << 8),
        u16::from(g[13]) + (u16::from(g[12]) << 8),
    );

    addr_str.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("derive_multicast_address produced invalid IPv6: {}", e),
        )
    })
}

// Discovery tokens
/// Size of the per-instance nonce appended to discovery tokens.
///
/// Each orchestrator generates a random nonce at startup. This nonce is
/// appended to every outgoing discovery token so that self-echo detection
/// works correctly when multiple nodes share the same NIC addresses
/// (e.g. two nodes in the same process for testing).
const NONCE_SIZE: usize = 8;

/// Create a discovery token for multicast announcement.
///
/// `token = SHA-256(group_id + link_local_address_string)`
///
/// The address string is the text representation of the IPv6 link-local
/// address (e.g. "fe80::1"), matching Python's `str(addr).encode("utf-8")`.
pub(crate) fn make_discovery_token(group_id: &[u8], link_local_str: &str) -> [u8; 32] {
    let mut input = Vec::with_capacity(group_id.len() + link_local_str.len());
    input.extend_from_slice(group_id);
    input.extend_from_slice(link_local_str.as_bytes());
    full_hash(&input)
}

/// Discovery packet size: token(32) + nonce(8) + data_port(2) = 42 bytes.
/// Token first for Python compatibility (Python reads `data[:32]`).
pub(crate) const DISCOVERY_PACKET_SIZE: usize = 32 + NONCE_SIZE + 2;

/// Build a wire-format discovery packet: `[token(32)] + [nonce(8)] + [data_port(2)]`.
///
/// Token comes first so that Python Reticulum (which reads `data[:32]`)
/// gets the valid token directly. The nonce allows Rust receivers to
/// distinguish self-echoes from peer tokens when multiple nodes share
/// the same NIC addresses. The data_port tells Rust peers which port
/// to send unicast data to (distinct per node for same-machine operation).
pub(crate) fn build_discovery_packet(
    token: &[u8; 32],
    nonce: &[u8; NONCE_SIZE],
    data_port: u16,
) -> [u8; DISCOVERY_PACKET_SIZE] {
    let mut pkt = [0u8; DISCOVERY_PACKET_SIZE];
    pkt[..32].copy_from_slice(token);
    pkt[32..32 + NONCE_SIZE].copy_from_slice(nonce);
    pkt[32 + NONCE_SIZE..].copy_from_slice(&data_port.to_be_bytes());
    pkt
}

/// Parsed discovery packet fields.
///
/// - 32-byte (Python): token only, nonce and data_port are `None`
/// - 42-byte (Rust): token + nonce + data_port
pub(crate) struct DiscoveryFields {
    pub token: [u8; 32],
    pub nonce: Option<[u8; NONCE_SIZE]>,
    pub data_port: Option<u16>,
}

/// Parse a received discovery packet.
///
/// Accepts both 32-byte (Python: token only) and 42-byte (Rust: token + nonce + data_port)
/// packets. Returns `None` if the packet is too short.
pub(crate) fn parse_discovery_packet(data: &[u8]) -> Option<DiscoveryFields> {
    if data.len() < 32 {
        return None;
    }
    let mut token = [0u8; 32];
    token.copy_from_slice(&data[..32]);
    let nonce = if data.len() >= 32 + NONCE_SIZE {
        let mut n = [0u8; NONCE_SIZE];
        n.copy_from_slice(&data[32..32 + NONCE_SIZE]);
        Some(n)
    } else {
        None
    };
    let data_port = if data.len() >= 32 + NONCE_SIZE + 2 {
        Some(u16::from_be_bytes([
            data[32 + NONCE_SIZE],
            data[32 + NONCE_SIZE + 1],
        ]))
    } else {
        None
    };
    Some(DiscoveryFields {
        token,
        nonce,
        data_port,
    })
}

/// Verify a received discovery token against expected group_id and source address.
///
/// Uses constant-time comparison to prevent timing side channels.
pub(crate) fn verify_discovery_token(token: &[u8], group_id: &[u8], src_addr_str: &str) -> bool {
    if token.len() != 32 {
        return false;
    }
    let expected = make_discovery_token(group_id, src_addr_str);
    constant_time_eq(token, &expected)
}

/// Constant-time byte comparison
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// Deduplication cache
/// Cache for deduplicating packets received from multiple NICs.
///
/// Stores hashes of recently seen packets with timestamps.
/// A packet is a duplicate if its hash was seen within DEDUP_TTL.
///
/// Uses a VecDeque with fixed max capacity matching Python's `deque(maxlen=48)`.
pub(crate) struct DeduplicationCache {
    entries: std::collections::VecDeque<(Instant, [u8; 32])>,
    max_size: usize,
    ttl: std::time::Duration,
}

impl DeduplicationCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: std::collections::VecDeque::with_capacity(DEDUP_CACHE_SIZE),
            max_size: DEDUP_CACHE_SIZE,
            ttl: std::time::Duration::from_millis((DEDUP_TTL_SECS * 1000.0) as u64),
        }
    }

    /// Check if data is a duplicate. If not, adds it to the cache.
    /// Returns `true` if the data was already seen (duplicate).
    pub(crate) fn is_duplicate(&mut self, data: &[u8]) -> bool {
        let hash = full_hash(data);
        let now = Instant::now();

        // Check for existing entry within TTL
        for &(timestamp, ref h) in &self.entries {
            if *h == hash && now.duration_since(timestamp) < self.ttl {
                return true;
            }
        }

        // Not a duplicate, add to cache
        if self.entries.len() >= self.max_size {
            self.entries.pop_front();
        }
        self.entries.push_back((now, hash));
        false
    }
}

// Unicast discovery port
/// The unicast discovery port is discovery_port + 1 (matches Python)
pub(crate) fn unicast_discovery_port(discovery_port: u16) -> u16 {
    discovery_port + 1
}

/// Reverse peering interval = announce_interval * 3.25 (matches Python)
pub(crate) fn reverse_peering_interval_secs() -> f64 {
    ANNOUNCE_INTERVAL_SECS * 3.25
}

/// Short hex tag identifying a non-default discovery group.
///
/// Multiple `[[AutoInterface]]` sections run on the same LAN as isolated
/// discovery domains. A peer reachable in two groups shares one link-local
/// address, so without a per-group tag its interface name (`auto/<nic>/<ip>`)
/// would be identical in both groups and the two entries would be
/// indistinguishable in `rnstatus`. Appending this tag keeps the names unique
/// per group. Returns `None` for the default group so a single-section node's
/// interface names are unchanged.
pub(crate) fn group_name_tag(group_id: &[u8]) -> Option<String> {
    if group_id == DEFAULT_GROUP_ID {
        return None;
    }
    let h = full_hash(group_id);
    Some(format!("{:02x}{:02x}{:02x}{:02x}", h[0], h[1], h[2], h[3]))
}

// Socket binding helpers
/// Bind a multicast discovery socket on a specific NIC.
///
/// - Joins the multicast group on the NIC
/// - Sets `SO_REUSEADDR` + `SO_REUSEPORT`
/// - Sets `IPV6_MULTICAST_IF` to the NIC's index
/// - Binds to the multicast address (link-local scope) or `[::]` (other scopes)
/// - Returns a tokio `UdpSocket`
pub(crate) fn bind_multicast_socket(
    nic: &AdoptedNic,
    mcast_addr: &Ipv6Addr,
    port: u16,
    scope: &str,
    enable_loopback: bool,
) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    // Set multicast interface
    socket.set_multicast_if_v6(nic.index)?;

    // Enable/disable multicast loopback: the carrier self-echo mechanism
    // (default on, matching Python-RNS). Also required for same-machine tests.
    socket.set_multicast_loop_v6(enable_loopback)?;

    // Join multicast group on this NIC
    socket.join_multicast_v6(mcast_addr, nic.index)?;

    // Bind: link-local scope binds to the multicast addr with scope_id,
    // other scopes bind to the multicast addr without scope_id.
    let scope_id = if scope_to_byte(scope) == "2" {
        nic.index
    } else {
        0
    };
    let bind_addr = SocketAddrV6::new(*mcast_addr, port, 0, scope_id);
    socket.bind(&SockAddr::from(bind_addr))?;

    UdpSocket::from_std(socket.into())
}

/// Bind a unicast discovery socket on a specific NIC's link-local address.
///
/// Used for receiving reverse peering tokens.
pub(crate) fn bind_unicast_socket(nic: &AdoptedNic, port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;

    let bind_addr = SocketAddrV6::new(nic.link_local, port, 0, nic.index);
    socket.bind(&SockAddr::from(bind_addr))?;

    UdpSocket::from_std(socket.into())
}

/// Bind a data receive socket on a specific NIC's link-local address.
///
/// Used for receiving Reticulum packets from discovered peers.
/// Binds to the configured `port` with SO_REUSEPORT for multi-process coexistence.
pub(crate) fn bind_data_socket(nic: &AdoptedNic, port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;
    socket.set_reuse_address(true)?;
    #[cfg(target_os = "linux")]
    socket.set_reuse_port(true)?;

    let bind_addr = SocketAddrV6::new(nic.link_local, port, 0, nic.index);
    socket.bind(&SockAddr::from(bind_addr))?;

    UdpSocket::from_std(socket.into())
}

/// Bind a UDP socket on `[::]:0` (any available port).
///
/// Used in tests as a simple ephemeral socket for send/recv testing.
#[cfg(test)]
fn bind_outbound_socket() -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_nonblocking(true)?;

    let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
    socket.bind(&SockAddr::from(bind_addr))?;

    UdpSocket::from_std(socket.into())
}

/// Receive result from `recv_from_any`.
pub(crate) struct RecvResult {
    /// Number of bytes received
    pub bytes_read: usize,
    /// Source address (with scope_id for link-local)
    pub source: SocketAddrV6,
    /// Index into the socket slice identifying which socket received
    pub socket_index: usize,
}

/// Poll multiple UDP sockets and return data from the first one ready.
///
/// For small N (1-3 NICs), iterative polling is efficient.
/// Returns when any socket has data available.
///
/// `poll_start` provides round-robin fairness: iteration starts at
/// `*poll_start` and wraps. After a successful recv, `*poll_start` advances
/// past the served socket so the next call starts elsewhere.
///
/// Uses `readable()` + `try_recv_from()` pattern (edge-triggered).
/// On `WouldBlock`, loops back to `readable()` to re-register waker.
///
/// Generic over socket container: accepts `&[UdpSocket]` or `&[Arc<UdpSocket>]`.
pub(crate) async fn recv_from_any<S: std::borrow::Borrow<UdpSocket>>(
    sockets: &[S],
    buf: &mut [u8],
    poll_start: &mut usize,
) -> io::Result<RecvResult> {
    if sockets.is_empty() {
        std::future::pending::<()>().await;
        unreachable!()
    }

    let len = sockets.len();
    let start = *poll_start;

    loop {
        // Wait for any socket to become readable (round-robin from poll_start)
        let ready_idx = {
            let idx = std::future::poll_fn(|cx| {
                for offset in 0..len {
                    let idx = (start + offset) % len;
                    match std::borrow::Borrow::<UdpSocket>::borrow(&sockets[idx])
                        .poll_recv_ready(cx)
                    {
                        std::task::Poll::Ready(Ok(())) => {
                            return std::task::Poll::Ready(Ok(idx));
                        }
                        std::task::Poll::Ready(Err(e)) => {
                            return std::task::Poll::Ready(Err(e));
                        }
                        std::task::Poll::Pending => {}
                    }
                }
                std::task::Poll::Pending
            })
            .await?;
            idx
        };

        // Try non-blocking recv on the ready socket
        match std::borrow::Borrow::<UdpSocket>::borrow(&sockets[ready_idx]).try_recv_from(buf) {
            Ok((n, addr)) => {
                let v6 = match addr {
                    std::net::SocketAddr::V6(v6) => v6,
                    std::net::SocketAddr::V4(_) => continue,
                };
                *poll_start = (ready_idx + 1) % len;
                return Ok(RecvResult {
                    bytes_read: n,
                    source: v6,
                    socket_index: ready_idx,
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Spurious wakeup, re-register by looping back to poll_recv_ready
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_multicast_address_matches_python() {
        // Python test vector: group_id=b"reticulum", scope="link"
        // Expected: ff12:0:d70b:fb1c:16e4:5e39:485e:31e1
        let addr = derive_multicast_address(b"reticulum", "link").unwrap();
        let expected: Ipv6Addr = "ff12:0:d70b:fb1c:16e4:5e39:485e:31e1".parse().unwrap();
        assert_eq!(addr, expected, "multicast address must match Python output");
    }

    #[test]
    fn test_derive_multicast_address_site_scope() {
        let addr = derive_multicast_address(b"reticulum", "site").unwrap();
        let addr_str = addr.to_string();
        // Scope "site" = "5", type = "1" → prefix ff15:
        assert!(
            addr_str.starts_with("ff15:"),
            "site scope should produce ff15: prefix, got {}",
            addr_str
        );
    }

    #[test]
    fn test_derive_multicast_address_custom_group() {
        // Different group_id should produce a different address
        let default_addr = derive_multicast_address(b"reticulum", "link").unwrap();
        let custom_addr = derive_multicast_address(b"my_custom_network", "link").unwrap();
        assert_ne!(default_addr, custom_addr);
    }

    #[test]
    fn test_derive_multicast_address_group_isolation() {
        // Codeberg #7: multiple AutoInterface sections with distinct group_ids
        // form isolated discovery domains. Isolation at the multicast layer
        // means each group derives a DIFFERENT multicast address, so a section
        // only ever receives its own group's discovery traffic. Same group_id
        // is deterministic (peers must agree on the address).
        let groups: [&[u8]; 3] = [b"reticulum", b"groupA", b"groupB"];
        let addrs: Vec<Ipv6Addr> = groups
            .iter()
            .map(|g| derive_multicast_address(g, "link").unwrap())
            .collect();

        // All pairwise-distinct.
        for i in 0..addrs.len() {
            for j in (i + 1)..addrs.len() {
                assert_ne!(
                    addrs[i], addrs[j],
                    "groups {:?} and {:?} must derive different multicast addresses",
                    groups[i], groups[j]
                );
            }
        }

        // Deterministic: same group_id → same address.
        assert_eq!(
            derive_multicast_address(b"groupA", "link").unwrap(),
            derive_multicast_address(b"groupA", "link").unwrap(),
        );
    }

    #[test]
    fn test_group_name_tag() {
        // Default group is untagged so single-section naming is unchanged.
        assert_eq!(group_name_tag(DEFAULT_GROUP_ID), None);
        assert_eq!(group_name_tag(b"reticulum"), None);

        // Non-default groups get a stable 8-hex-char tag.
        let tag_a = group_name_tag(b"groupA").expect("non-default group tagged");
        let tag_b = group_name_tag(b"groupB").expect("non-default group tagged");
        assert_eq!(tag_a.len(), 8);
        assert_ne!(tag_a, tag_b, "distinct groups get distinct tags");
        // Deterministic.
        assert_eq!(group_name_tag(b"groupA").unwrap(), tag_a);
    }

    #[test]
    fn test_discovery_token_roundtrip() {
        let group_id = b"reticulum";
        let addr = "fe80::1";
        let token = make_discovery_token(group_id, addr);
        assert!(verify_discovery_token(&token, group_id, addr));
    }

    #[test]
    fn test_discovery_token_matches_python() {
        // Python test vector: group_id=b"reticulum", addr="fe80::1"
        // Expected: 97b25576749ea936b0d8a8536ffaf442d157cf47d460dcf13c48b7bd18b6c163
        let token = make_discovery_token(b"reticulum", "fe80::1");
        let expected =
            hex::decode("97b25576749ea936b0d8a8536ffaf442d157cf47d460dcf13c48b7bd18b6c163")
                .unwrap();
        assert_eq!(token.as_slice(), expected.as_slice());
    }

    #[test]
    fn test_discovery_token_wrong_addr_rejected() {
        let group_id = b"reticulum";
        let token = make_discovery_token(group_id, "fe80::1");
        assert!(!verify_discovery_token(&token, group_id, "fe80::2"));
    }

    #[test]
    fn test_discovery_token_wrong_group_rejected() {
        let token = make_discovery_token(b"reticulum", "fe80::1");
        assert!(!verify_discovery_token(&token, b"other_group", "fe80::1"));
    }

    #[test]
    fn test_discovery_token_wrong_length_rejected() {
        assert!(!verify_discovery_token(&[0u8; 16], b"reticulum", "fe80::1"));
        assert!(!verify_discovery_token(&[], b"reticulum", "fe80::1"));
    }

    #[test]
    fn test_dedup_cache_rejects_duplicate() {
        let mut cache = DeduplicationCache::new();
        let data = b"test packet data";
        assert!(!cache.is_duplicate(data), "first time should not be dup");
        assert!(cache.is_duplicate(data), "second time should be dup");
    }

    #[test]
    fn test_dedup_cache_different_data_not_duplicate() {
        let mut cache = DeduplicationCache::new();
        assert!(!cache.is_duplicate(b"packet A"));
        assert!(!cache.is_duplicate(b"packet B"));
    }

    #[test]
    fn test_dedup_cache_respects_max_size() {
        let mut cache = DeduplicationCache::new();
        // Fill past capacity
        for i in 0..DEDUP_CACHE_SIZE + 10 {
            let data = format!("packet_{}", i);
            cache.is_duplicate(data.as_bytes());
        }
        assert!(cache.entries.len() <= DEDUP_CACHE_SIZE);
    }

    #[test]
    fn test_dedup_cache_expires_after_ttl() {
        let mut cache = DeduplicationCache {
            entries: std::collections::VecDeque::with_capacity(DEDUP_CACHE_SIZE),
            max_size: DEDUP_CACHE_SIZE,
            // Use a very short TTL for testing
            ttl: std::time::Duration::from_millis(1),
        };
        let data = b"expiring packet";
        assert!(!cache.is_duplicate(data));
        // Sleep past TTL
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(
            !cache.is_duplicate(data),
            "should not be dup after TTL expires"
        );
    }

    #[test]
    fn test_scope_to_byte() {
        assert_eq!(scope_to_byte("link"), "2");
        assert_eq!(scope_to_byte("admin"), "4");
        assert_eq!(scope_to_byte("site"), "5");
        assert_eq!(scope_to_byte("organisation"), "8");
        assert_eq!(scope_to_byte("global"), "e");
        assert_eq!(scope_to_byte("Link"), "2"); // case insensitive
        assert_eq!(scope_to_byte("unknown"), "2"); // default
    }

    #[test]
    fn test_unicast_discovery_port() {
        assert_eq!(unicast_discovery_port(29716), 29717);
    }

    #[test]
    fn test_reverse_peering_interval() {
        let interval = reverse_peering_interval_secs();
        let expected = 1.6 * 3.25; // 5.2
        assert!((interval - expected).abs() < 0.001);
    }

    #[test]
    fn test_is_link_local_v6() {
        assert!(is_link_local_v6(&"fe80::1".parse().unwrap()));
        assert!(is_link_local_v6(
            &"fe80::abcd:1234:5678:9abc".parse().unwrap()
        ));
        assert!(!is_link_local_v6(&"::1".parse().unwrap()));
        assert!(!is_link_local_v6(&"2001:db8::1".parse().unwrap()));
        assert!(!is_link_local_v6(&"ff02::1".parse().unwrap()));
    }

    #[test]
    fn test_enumerate_nics_filters_loopback() {
        let config = AutoInterfaceConfig::default();
        let nics = enumerate_nics(&config);
        // Loopback (lo) should never appear
        for nic in &nics {
            assert_ne!(nic.name, "lo", "loopback must be filtered out");
        }
    }

    #[test]
    fn test_enumerate_nics_ignores_docker() {
        let config = AutoInterfaceConfig::default();
        let nics = enumerate_nics(&config);
        for nic in &nics {
            assert!(
                !nic.name.starts_with("docker"),
                "docker interfaces must be filtered: {}",
                nic.name
            );
            assert!(
                !nic.name.starts_with("veth"),
                "veth interfaces must be filtered: {}",
                nic.name
            );
        }
    }

    #[test]
    fn test_enumerate_nics_whitelist() {
        // Whitelist a non-existent device, should return empty
        let config = AutoInterfaceConfig {
            allowed_devices: Some("nonexistent_device_xyz".to_string()),
            ..Default::default()
        };
        let nics = enumerate_nics(&config);
        assert!(nics.is_empty());
    }

    #[test]
    fn test_constant_time_eq() {
        let a = [1u8, 2, 3, 4];
        let b = [1u8, 2, 3, 4];
        let c = [1u8, 2, 3, 5];
        assert!(constant_time_eq(&a, &b));
        assert!(!constant_time_eq(&a, &c));
        assert!(!constant_time_eq(&a, &[1, 2, 3])); // different length
    }

    #[tokio::test]
    async fn test_bind_outbound_socket() {
        let sock = bind_outbound_socket().expect("outbound socket should bind");
        let local = sock.local_addr().expect("should have local addr");
        assert_ne!(local.port(), 0, "OS should assign a port");
    }

    #[tokio::test]
    async fn test_recv_from_any_single_socket() {
        // Bind two sockets, send to the second, verify recv_from_any returns it.
        let s1 = bind_outbound_socket().unwrap();
        let s2 = bind_outbound_socket().unwrap();
        let s2_port = s2.local_addr().unwrap().port();

        // Sockets bind to the unspecified address (`::`), so `local_addr()`
        // reports `[::]:port` — which is not a valid unicast *destination*.
        // Linux delivers a packet sent to `::` locally anyway; macOS (more
        // correctly) does not, so addressing s2 by its raw local_addr made the
        // test Linux-only. Send to loopback `[::1]:port` instead — the
        // `::`-bound s2 still receives it — so the test is portable.
        let dest: std::net::SocketAddr =
            SocketAddrV6::new(Ipv6Addr::LOCALHOST, s2_port, 0, 0).into();

        let sender = bind_outbound_socket().unwrap();
        sender.send_to(b"hello", dest).await.unwrap();

        let sockets = [s1, s2];
        let mut buf = [0u8; 64];
        let mut poll_start = 0;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            recv_from_any(&sockets, &mut buf, &mut poll_start),
        )
        .await
        .expect("timeout")
        .expect("recv error");

        assert_eq!(result.socket_index, 1, "should recv on socket index 1");
        assert_eq!(result.bytes_read, 5);
        assert_eq!(&buf[..5], b"hello");
        assert_eq!(poll_start, 0, "round-robin should advance past index 1");
    }

    #[tokio::test]
    async fn test_unicast_socket_bind_loopback() {
        // Test binding a unicast socket on localhost (uses ::1 instead of link-local)
        // This just verifies the socket2 setup path works.
        let nic = AdoptedNic {
            name: "lo".to_string(),
            link_local: "::1".parse().unwrap(),
            index: 1,
        };
        let result = bind_unicast_socket(&nic, 0);
        // May fail on some systems where ::1 needs special handling,
        // but should succeed on standard Linux
        if let Ok(sock) = result {
            let addr = sock.local_addr().unwrap();
            assert_ne!(addr.port(), 0);
        }
    }

    #[tokio::test]
    async fn test_data_socket_bind_loopback() {
        let nic = AdoptedNic {
            name: "lo".to_string(),
            link_local: "::1".parse().unwrap(),
            index: 1,
        };
        // Use port 0 so the OS assigns an ephemeral port for the test
        let result = bind_data_socket(&nic, 0);
        if let Ok(sock) = result {
            let addr = sock.local_addr().unwrap();
            assert_ne!(addr.port(), 0);
        }
    }

    #[test]
    fn test_discovery_packet_42_byte_roundtrip() {
        let token = make_discovery_token(b"reticulum", "fe80::1");
        let nonce = [0x42u8; NONCE_SIZE];
        let data_port = 42671u16;
        let pkt = build_discovery_packet(&token, &nonce, data_port);
        assert_eq!(pkt.len(), DISCOVERY_PACKET_SIZE);
        assert_eq!(pkt.len(), 42);

        let parsed = parse_discovery_packet(&pkt).expect("should parse 42 bytes");
        assert_eq!(parsed.token, token);
        assert_eq!(parsed.nonce, Some(nonce));
        assert_eq!(parsed.data_port, Some(data_port));
    }

    #[test]
    fn test_discovery_packet_32_byte_python_compat() {
        // Python sends 32-byte packets: just the token
        let token = make_discovery_token(b"reticulum", "fe80::1");
        let parsed = parse_discovery_packet(&token).expect("should parse 32 bytes");
        assert_eq!(parsed.token, token);
        assert!(
            parsed.nonce.is_none(),
            "32-byte Python packet should have no nonce"
        );
        assert!(
            parsed.data_port.is_none(),
            "32-byte Python packet should have no data_port"
        );
    }

    #[test]
    fn test_discovery_packet_token_first() {
        // Verify token occupies bytes [0..32]. Python reads data[:32]
        let token = make_discovery_token(b"reticulum", "fe80::1");
        let nonce = [0xABu8; NONCE_SIZE];
        let data_port = 12345u16;
        let pkt = build_discovery_packet(&token, &nonce, data_port);
        assert_eq!(&pkt[..32], &token[..]);
        assert_eq!(&pkt[32..40], &nonce[..]);
        assert_eq!(&pkt[40..42], &data_port.to_be_bytes());
    }

    #[test]
    fn test_discovery_packet_too_short_rejected() {
        assert!(parse_discovery_packet(&[0u8; 31]).is_none());
        assert!(parse_discovery_packet(&[]).is_none());
    }
}
