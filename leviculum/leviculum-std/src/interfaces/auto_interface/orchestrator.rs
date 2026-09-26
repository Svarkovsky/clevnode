//! AutoInterface orchestrator: peer discovery and lifecycle management
//!
//! Single tokio task that handles multicast discovery, peer management,
//! and data socket demultiplexing for AutoInterface.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use super::{
    bind_data_socket, bind_multicast_socket, bind_unicast_socket, build_discovery_packet,
    derive_multicast_address, enumerate_nics, group_name_tag, make_discovery_token,
    parse_discovery_packet, recv_from_any, unicast_discovery_port, verify_discovery_token,
    AdoptedNic, AutoInterfaceConfig, DeduplicationCache, ANNOUNCE_INTERVAL_SECS, AUTO_HW_MTU,
    DISCOVERY_PACKET_SIZE, MCAST_ECHO_TIMEOUT_SECS, NONCE_SIZE, PEERING_TIMEOUT_SECS,
    PEER_JOB_INTERVAL_SECS,
};
use crate::interfaces::{
    IncomingPacket, InterfaceCounters, InterfaceHandle, InterfaceInfo, OutgoingPacket,
};
use leviculum_core::transport::InterfaceId;

/// Maximum datagram size for AutoInterface (matches Python HW_MTU = 1196).
const MAX_DATAGRAM_SIZE: usize = 1196;

/// Per-peer channel buffer size
const PEER_CHANNEL_BUFFER: usize = 64;

/// Information about a discovered peer
struct PeerInfo {
    /// NIC name where this peer was discovered
    nic_name: String,
    /// NIC interface index (scope_id for SocketAddrV6)
    scope_id: u32,
    /// Last time we heard from this peer (discovery or data)
    last_heard: Instant,
    /// Last time we sent a reverse peering token to this peer
    last_reverse_peering: Instant,
    /// Channel to push incoming data to the event loop.
    /// Dropping this triggers handle_interface_down cascade.
    incoming_tx: mpsc::Sender<IncomingPacket>,
    /// Shared I/O counters (same Arc is in the InterfaceHandle)
    counters: Arc<InterfaceCounters>,
}

/// Per-NIC multicast echo tracking for carrier detection
struct NicState {
    /// Last time a multicast echo was received on this NIC
    last_echo: Option<Instant>,
    /// Whether the NIC is currently timed out (no carrier)
    timed_out: bool,
}

/// Given all enumerated NICs and per-NIC bind results, return only the NICs
/// that succeeded and pre-compute their discovery tokens.
///
/// Socket vecs (`mcast_sockets`, `unicast_sockets`, `data_sockets`) only contain
/// entries for NICs where all three binds succeeded. This function produces
/// parallel `active_nics` vecs so that `socket_index` maps to the correct NIC.
///
/// Removal path: entries are static for the lifetime of the orchestrator.
pub(crate) fn build_active_nics_and_tokens(
    nics: &[AdoptedNic],
    bind_results: &[bool],
    group_id: &[u8],
) -> (Vec<AdoptedNic>, Vec<([u8; 32], String)>) {
    let active_indices: Vec<usize> = bind_results
        .iter()
        .enumerate()
        .filter(|(_, ok)| **ok)
        .map(|(i, _)| i)
        .collect();

    let active: Vec<AdoptedNic> = active_indices.iter().map(|&i| nics[i].clone()).collect();

    let tokens: Vec<([u8; 32], String)> = active
        .iter()
        .map(|n| {
            let addr_str = n.link_local.to_string();
            let token = make_discovery_token(group_id, &addr_str);
            (token, addr_str)
        })
        .collect();

    (active, tokens)
}

/// Compute the discovery token for a reverse peering announcement.
///
/// The token must be verifiable by the peer using the sender's source IP.
/// Looks up the peer's NIC in `active_nics` to find OUR link-local address
/// on that NIC, the peer will verify `hash(group_id + our_source_ip)`.
///
/// Returns `None` if the NIC is not found in `active_nics`.
pub(crate) fn compute_reverse_peering_token(
    group_id: &[u8],
    peer_nic_name: &str,
    active_nics: &[AdoptedNic],
) -> Option<[u8; 32]> {
    let our_nic = active_nics.iter().find(|n| n.name == peer_nic_name)?;
    let our_addr_str = our_nic.link_local.to_string();
    Some(make_discovery_token(group_id, &our_addr_str))
}

/// Spawn the AutoInterface orchestrator as a background tokio task.
///
/// The orchestrator enumerates NICs, binds sockets, and runs the discovery
/// and data forwarding loop. Discovered peers are registered as individual
/// interfaces via `new_iface_tx`.
///
/// Returns a `watch::Receiver<usize>` that broadcasts the current peer count.
pub(crate) fn spawn_auto_interface(
    next_id: Arc<AtomicUsize>,
    new_iface_tx: mpsc::Sender<InterfaceHandle>,
    config: AutoInterfaceConfig,
) -> watch::Receiver<usize> {
    let (peer_count_tx, peer_count_rx) = watch::channel(0usize);
    tokio::spawn(async move {
        if let Err(e) = run_auto_interface(config, next_id, new_iface_tx, peer_count_tx).await {
            tracing::error!("AutoInterface orchestrator exited with error: {}", e);
        }
    });
    peer_count_rx
}

/// Main orchestrator loop.
///
/// Enumerates NICs, binds sockets, and runs the discovery + data loop.
/// Each discovered peer becomes a separate InterfaceHandle registered
/// via `new_iface_tx` into the main event loop.
async fn run_auto_interface(
    config: AutoInterfaceConfig,
    next_id: Arc<AtomicUsize>,
    new_iface_tx: mpsc::Sender<InterfaceHandle>,
    peer_count_tx: watch::Sender<usize>,
) -> io::Result<()> {
    // Enumerate suitable NICs
    let nics = enumerate_nics(&config);
    if nics.is_empty() {
        tracing::warn!("AutoInterface: no suitable network interfaces found");
        return Ok(());
    }

    let mcast_addr = derive_multicast_address(&config.group_id, &config.discovery_scope)?;
    let unicast_port = unicast_discovery_port(config.discovery_port);
    // Per-group tag appended to peer interface names so peers reachable in
    // multiple groups do not collide in the registry / rnstatus. `None` for
    // the default group keeps single-section naming unchanged.
    let group_tag = group_name_tag(&config.group_id);

    tracing::info!(
        "AutoInterface: {} NIC(s), multicast={}, discovery_port={}, data_port={}",
        nics.len(),
        mcast_addr,
        config.discovery_port,
        config.data_port
    );

    // Bind sockets per NIC, tracking which succeeded
    let mut mcast_sockets = Vec::new();
    let mut unicast_sockets = Vec::new();
    let mut data_sockets = Vec::new();
    let mut bind_results = Vec::with_capacity(nics.len());
    for nic in &nics {
        match bind_multicast_socket(
            nic,
            &mcast_addr,
            config.discovery_port,
            &config.discovery_scope,
            config.multicast_loopback,
        ) {
            Ok(s) => {
                tracing::info!(
                    "AutoInterface: multicast socket on {} ({})",
                    nic.name,
                    nic.link_local
                );
                mcast_sockets.push(s);
            }
            Err(e) => {
                tracing::warn!(
                    "AutoInterface: failed to bind multicast on {}: {}",
                    nic.name,
                    e
                );
                bind_results.push(false);
                continue;
            }
        }

        match bind_unicast_socket(nic, unicast_port) {
            Ok(s) => unicast_sockets.push(s),
            Err(e) => {
                tracing::warn!(
                    "AutoInterface: failed to bind unicast on {}: {}",
                    nic.name,
                    e
                );
                // Remove the multicast socket we just pushed, can't function without unicast
                mcast_sockets.pop();
                bind_results.push(false);
                continue;
            }
        }

        match bind_data_socket(nic, config.data_port) {
            Ok(s) => {
                data_sockets.push(Arc::new(s));
            }
            Err(e) => {
                tracing::warn!("AutoInterface: failed to bind data on {}: {}", nic.name, e);
                mcast_sockets.pop();
                unicast_sockets.pop();
                bind_results.push(false);
                continue;
            }
        }
        bind_results.push(true);
    }

    if mcast_sockets.is_empty() {
        tracing::warn!("AutoInterface: no sockets could be bound, exiting");
        return Ok(());
    }

    // Per-peer state: keyed by peer's (IPv6 link-local, data_port).
    // Data receive does a two-tier lookup: try exact (ip, port) first,
    // then fall back to ip-only. This handles both:
    // - Same-machine Rust peers (send from data_port → exact match)
    // - Cross-machine Python peers (send from ephemeral port → ip-only)
    // Removal path: peers are removed on timeout in the peer_job_timer branch.
    let mut peers: HashMap<(Ipv6Addr, u16), PeerInfo> = HashMap::new();

    // Per-NIC state for carrier detection
    let mut nic_states: HashMap<String, NicState> = nics
        .iter()
        .map(|n| {
            (
                n.name.clone(),
                NicState {
                    last_echo: None,
                    timed_out: false,
                },
            )
        })
        .collect();

    // Deduplication cache for data packets received on multiple NICs
    let mut dedup = DeduplicationCache::new();

    // Timers
    let announce_interval = Duration::from_secs_f64(ANNOUNCE_INTERVAL_SECS);
    let peer_job_interval = Duration::from_secs_f64(PEER_JOB_INTERVAL_SECS);
    let peering_timeout = Duration::from_secs_f64(PEERING_TIMEOUT_SECS);
    let echo_timeout = Duration::from_secs_f64(MCAST_ECHO_TIMEOUT_SECS);
    let reverse_peering_interval = Duration::from_secs_f64(super::reverse_peering_interval_secs());

    let mut announce_timer = tokio::time::interval(announce_interval);
    let mut peer_job_timer = tokio::time::interval(peer_job_interval);

    // Build active_nics (only NICs where all 3 sockets bound) and their tokens
    let (active_nics, our_tokens) =
        build_active_nics_and_tokens(&nics, &bind_results, &config.group_id);

    // Generate per-instance nonce for self-echo detection.
    // Two nodes on the same machine share NIC addresses, so IP-based
    // self-echo detection fails. The nonce distinguishes our own packets.
    let instance_nonce: [u8; NONCE_SIZE] = {
        use rand_core::RngCore;
        let mut buf = [0u8; NONCE_SIZE];
        rand_core::OsRng.fill_bytes(&mut buf);
        buf
    };

    // Pre-build discovery packets: [token(32)] + [nonce(8)] + [data_port(2)] per NIC
    let discovery_packets: Vec<[u8; DISCOVERY_PACKET_SIZE]> = our_tokens
        .iter()
        .map(|(token, _)| build_discovery_packet(token, &instance_nonce, config.data_port))
        .collect();

    let mut mcast_buf = [0u8; 64];
    let mut unicast_buf = [0u8; 64];
    let mut data_buf = [0u8; MAX_DATAGRAM_SIZE];
    let mut mcast_poll = 0usize;
    let mut unicast_poll = 0usize;
    let mut data_poll = 0usize;

    loop {
        tokio::select! {
            // Multicast discovery recv
            result = recv_from_any(&mcast_sockets, &mut mcast_buf, &mut mcast_poll) => {
                let recv = match result {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("AutoInterface: multicast recv error: {}", e);
                        continue;
                    }
                };
                let data = &mcast_buf[..recv.bytes_read];
                let src_addr = *recv.source.ip();
                let nic_idx = recv.socket_index;
                let nic = &active_nics[nic_idx];
                handle_discovery_packet(
                    data,
                    src_addr,
                    nic,
                    &config,
                    &group_tag,
                    &instance_nonce,
                    &mut nic_states,
                    &mut peers,
                    &peer_count_tx,
                    &next_id,
                    &new_iface_tx,
                    &data_sockets[nic_idx],
                );
            }

            // Unicast discovery recv
            result = recv_from_any(&unicast_sockets, &mut unicast_buf, &mut unicast_poll) => {
                let recv = match result {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("AutoInterface: unicast recv error: {}", e);
                        continue;
                    }
                };
                let data = &unicast_buf[..recv.bytes_read];
                let src_addr = *recv.source.ip();
                let nic_idx = recv.socket_index;
                let nic = &active_nics[nic_idx];

                handle_discovery_packet(
                    data,
                    src_addr,
                    nic,
                    &config,
                    &group_tag,
                    &instance_nonce,
                    &mut nic_states,
                    &mut peers,
                    &peer_count_tx,
                    &next_id,
                    &new_iface_tx,
                    &data_sockets[nic_idx],
                );
            }

            // Data recv + demux
            result = recv_from_any(&data_sockets, &mut data_buf, &mut data_poll) => {
                let recv = match result {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("AutoInterface: data recv error: {}", e);
                        continue;
                    }
                };

                let data = &data_buf[..recv.bytes_read];
                if data.is_empty() {
                    continue;
                }

                // Dedup (same packet from multiple NICs)
                if dedup.is_duplicate(data) {
                    continue;
                }

                // Two-tier peer lookup:
                // 1. Try exact (ip, src_port), works for same-machine Rust peers
                //    that send from their data_port via the NIC data socket.
                // 2. Fall back to ip-only, works for cross-machine Python peers
                //    that send from ephemeral ports.
                let src_ip = *recv.source.ip();
                let src_port = recv.source.port();

                // Resolve the lookup key: exact match first, then ip-only fallback
                let lookup_key = if peers.contains_key(&(src_ip, src_port)) {
                    Some((src_ip, src_port))
                } else {
                    peers.keys().find(|(ip, _)| *ip == src_ip).copied()
                };

                if let Some(peer) = lookup_key.and_then(|k| peers.get_mut(&k)) {
                    peer.last_heard = Instant::now();
                    peer.counters.rx_bytes.fetch_add(data.len() as usize, std::sync::atomic::Ordering::Relaxed);
                    match peer.incoming_tx.try_send(IncomingPacket {
                        data: data.to_vec(),
                    }) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::debug!(
                                "AutoInterface: incoming channel full for {}, dropping packet",
                                src_ip
                            );
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            tracing::warn!(
                                "AutoInterface: incoming channel closed for {}",
                                src_ip
                            );
                        }
                    }
                } else {
                    tracing::debug!(
                        "AutoInterface: data from unknown peer {}, dropping (no discovery yet)",
                        src_ip
                    );
                }
            }

            // Announce timer (send multicast token)
            _ = announce_timer.tick() => {
                for (nic_idx, sock) in mcast_sockets.iter().enumerate() {
                    let nic = &active_nics[nic_idx];
                    let dest = SocketAddrV6::new(mcast_addr, config.discovery_port, 0, nic.index);
                    if let Err(e) = sock.send_to(&discovery_packets[nic_idx], dest).await {
                        tracing::debug!(
                            "AutoInterface: multicast send on {} failed: {}",
                            nic.name,
                            e
                        );
                    }
                }
            }

            // Peer job timer (timeout + reverse peering + carrier)
            _ = peer_job_timer.tick() => {
                let now = Instant::now();

                // Check peer timeouts
                let timed_out: Vec<(Ipv6Addr, u16)> = peers
                    .iter()
                    .filter(|(_, p)| now.duration_since(p.last_heard) > peering_timeout)
                    .map(|(key, _)| *key)
                    .collect();

                for key in &timed_out {
                    if let Some(peer) = peers.remove(key) {
                        tracing::info!(
                            "AutoInterface: peer {}:{} on {} timed out",
                            key.0,
                            key.1,
                            peer.nic_name,
                        );
                        // Dropping incoming_tx triggers the cascade:
                        // event loop detects Disconnected → handle_interface_down → cleanup
                    }
                }
                if !timed_out.is_empty() {
                    let _ = peer_count_tx.send(peers.len());
                }

                // Send reverse peering tokens to peers that haven't been poked recently
                for ((peer_ip, _peer_data_port), peer) in &mut peers {
                    if now.duration_since(peer.last_reverse_peering) > reverse_peering_interval {
                        peer.last_reverse_peering = now;
                        // Token must be verifiable by the peer using OUR source IP
                        let token = match compute_reverse_peering_token(
                            &config.group_id,
                            &peer.nic_name,
                            &active_nics,
                        ) {
                            Some(t) => t,
                            None => continue,
                        };
                        let pkt = build_discovery_packet(&token, &instance_nonce, config.data_port);
                        let dest = SocketAddrV6::new(
                            *peer_ip,
                            unicast_port,
                            0,
                            peer.scope_id,
                        );
                        // Find the NIC index to use the correct multicast socket for sending
                        // (it's already bound to the right interface)
                        if let Some(nic_idx) = active_nics.iter().position(|n| n.name == peer.nic_name) {
                            if nic_idx < mcast_sockets.len() {
                                if let Err(e) = mcast_sockets[nic_idx].send_to(&pkt, dest).await {
                                    tracing::debug!(
                                        "AutoInterface: reverse peering send to {} failed: {}",
                                        peer_ip,
                                        e
                                    );
                                }
                            }
                        }
                    }
                }

                // Check multicast echo timeouts (carrier detection)
                for (nic_name, state) in &mut nic_states {
                    let echo_timed_out = match state.last_echo {
                        Some(last) => now.duration_since(last) > echo_timeout,
                        None => continue, // No echo yet — normal at startup
                    };

                    if echo_timed_out && !state.timed_out {
                        state.timed_out = true;
                        tracing::warn!(
                            "AutoInterface: multicast echo timeout on {}. Carrier lost.",
                            nic_name
                        );
                    } else if !echo_timed_out && state.timed_out {
                        state.timed_out = false;
                        tracing::warn!(
                            "AutoInterface: carrier recovered on {}",
                            nic_name
                        );
                    }
                }
            }

            // Event loop shut down
            _ = new_iface_tx.closed() => {
                tracing::info!("AutoInterface: event loop shut down, exiting");
                break;
            }
        }
    }

    Ok(())
}

/// Process a discovery packet received via multicast or unicast.
///
/// Accepts both 32-byte (Python: token only) and 40-byte (Rust: token + nonce)
/// packets. Token is always at bytes [0..32].
///
/// Self-echo detection:
/// - 40-byte packet with matching nonce: self-echo → carrier detection, discard
/// - 40-byte packet with different nonce: peer Rust node → add peer
/// - 32-byte packet: NEVER self-echo (we only send 40-byte), always from Python → add peer
#[allow(clippy::too_many_arguments)]
fn handle_discovery_packet(
    data: &[u8],
    src_addr: Ipv6Addr,
    nic: &AdoptedNic,
    config: &AutoInterfaceConfig,
    group_tag: &Option<String>,
    instance_nonce: &[u8; NONCE_SIZE],
    nic_states: &mut HashMap<String, NicState>,
    peers: &mut HashMap<(Ipv6Addr, u16), PeerInfo>,
    peer_count_tx: &watch::Sender<usize>,
    next_id: &Arc<AtomicUsize>,
    new_iface_tx: &mpsc::Sender<InterfaceHandle>,
    data_socket: &Arc<UdpSocket>,
) {
    // Parse token (+ optional nonce)
    let parsed = match parse_discovery_packet(data) {
        Some(p) => p,
        None => {
            tracing::debug!(
                "AutoInterface: malformed discovery packet ({} bytes) from {} on {}: {:02x?}",
                data.len(),
                src_addr,
                nic.name,
                &data[..data.len().min(64)]
            );
            return;
        }
    };

    // Verify token
    let src_str = src_addr.to_string();
    if !verify_discovery_token(&parsed.token, &config.group_id, &src_str) {
        let expected = make_discovery_token(&config.group_id, &src_str);
        tracing::debug!(
            "AutoInterface: invalid discovery token from {} on {} (len={}, token={:02x?}, expected={:02x?}, group={:?}, src_str={:?})",
            src_addr,
            nic.name,
            data.len(),
            &parsed.token[..8],
            &expected[..8],
            String::from_utf8_lossy(&config.group_id),
            src_str
        );
        return;
    }

    // Self-echo detection: only possible for 40-byte packets (which have a nonce).
    // 32-byte packets (Python) are NEVER self-echo because we only send 40-byte.
    if let Some(nonce) = parsed.nonce {
        if nonce == *instance_nonce {
            // Our own 40-byte packet echoed back, carrier detection
            if let Some(state) = nic_states.get_mut(&nic.name) {
                state.last_echo = Some(Instant::now());
            }
            return;
        }
    }

    // Peer data_port: from discovery packet if present, else config default.
    // Used as destination port when sending TO this peer, and as part of the
    // peer map key for same-machine disambiguation.
    let peer_data_port = parsed.data_port.unwrap_or(config.data_port);
    let peer_key = (src_addr, peer_data_port);

    // Known peer, refresh
    if let Some(peer) = peers.get_mut(&peer_key) {
        peer.last_heard = Instant::now();
        return;
    }

    // New peer, register
    let id = InterfaceId(next_id.fetch_add(1, Ordering::Relaxed));
    let (incoming_tx, incoming_rx) = mpsc::channel(PEER_CHANNEL_BUFFER);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(PEER_CHANNEL_BUFFER);

    let counters = Arc::new(InterfaceCounters::new());

    // Spawn per-peer send task, sends via NIC data socket to peer's
    // (IP, data_port). Using the NIC data socket means our source port =
    // our data_port, which the peer matches against its peer map key.
    let peer_dest = SocketAddrV6::new(src_addr, peer_data_port, 0, nic.index);
    let send_socket = data_socket.clone();
    let send_counters = Arc::clone(&counters);
    tokio::spawn(async move {
        peer_send_task(outgoing_rx, send_socket, peer_dest, send_counters).await;
    });

    // Format interface name: IP suffix + port (to distinguish same-IP peers),
    // plus a per-group tag so the same peer discovered in two groups does not
    // collide (empty for the default group).
    let o = src_addr.octets();
    let addr_short = format!("{:02x}{:02x}{:02x}{:02x}", o[12], o[13], o[14], o[15]);
    let base_name = if peer_data_port == config.data_port {
        format!("auto/{}/{}", nic.name, addr_short)
    } else {
        format!("auto/{}/{}:{}", nic.name, addr_short, peer_data_port)
    };
    let iface_name = match group_tag {
        Some(tag) => format!("{}#{}", base_name, tag),
        None => base_name,
    };

    let handle = InterfaceHandle {
        info: InterfaceInfo {
            id,
            name: iface_name.clone(),
            hw_mtu: Some(AUTO_HW_MTU),
            is_local_client: false,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::Auto,
            ingress_control: None,
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters: Arc::clone(&counters),
        credit: None,
        // AutoInterface peers materialise per discovery; the UDP
        // socket pair already exists by the time this handle is
        // constructed, so the interface is ready immediately.
        ready: crate::interfaces::ReadySignal::ready_immediate(),
    };

    let now = Instant::now();
    peers.insert(
        peer_key,
        PeerInfo {
            nic_name: nic.name.clone(),
            scope_id: nic.index,
            last_heard: now,
            last_reverse_peering: now,
            incoming_tx,
            counters,
        },
    );
    let _ = peer_count_tx.send(peers.len());

    match new_iface_tx.try_send(handle) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::warn!(
                "AutoInterface: new-interface channel full, cannot register peer {}:{}",
                src_addr,
                peer_data_port,
            );
            peers.remove(&peer_key);
            let _ = peer_count_tx.send(peers.len());
            return;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::warn!("AutoInterface: event loop gone, cannot register peer");
            peers.remove(&peer_key);
            let _ = peer_count_tx.send(peers.len());
            return;
        }
    }

    tracing::info!(
        "AutoInterface: new peer {}:{} on {} (id={}, name={})",
        src_addr,
        peer_data_port,
        nic.name,
        id,
        iface_name
    );
}

/// Per-peer send task: reads outgoing packets from the event loop and
/// sends them to the peer's address via the shared outbound socket.
///
/// Exits when `outgoing_rx` is closed (event loop dropped the sender,
/// which happens when the InterfaceHandle is removed from the registry).
async fn peer_send_task(
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    socket: Arc<UdpSocket>,
    peer_addr: SocketAddrV6,
    counters: Arc<InterfaceCounters>,
) {
    while let Some(pkt) = outgoing_rx.recv().await {
        match socket.send_to(&pkt.data, peer_addr).await {
            Ok(n) => {
                counters
                    .tx_bytes
                    .fetch_add(n as usize, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                tracing::debug!("AutoInterface: send to {} failed: {}", peer_addr.ip(), e);
                // Don't break, send errors are transient for UDP
            }
        }
    }
    tracing::debug!("AutoInterface: send task for {} exiting", peer_addr.ip());
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Protocol, SockAddr, Type};

    #[test]
    fn test_active_nics_skip_failed_binds() {
        let nics = vec![
            AdoptedNic {
                name: "eth0".into(),
                link_local: "fe80::1".parse().unwrap(),
                index: 1,
            },
            AdoptedNic {
                name: "eth1".into(),
                link_local: "fe80::2".parse().unwrap(),
                index: 2,
            },
            AdoptedNic {
                name: "eth2".into(),
                link_local: "fe80::3".parse().unwrap(),
                index: 3,
            },
        ];
        // eth0 failed to bind, eth1 and eth2 succeeded
        let bind_results = vec![false, true, true];

        let (active, tokens) = build_active_nics_and_tokens(&nics, &bind_results, b"reticulum");

        // active_nics[0] must be eth1, NOT eth0
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].name, "eth1");
        assert_eq!(active[1].name, "eth2");

        // Token at index 0 must be for eth1's address (fe80::2)
        let expected = make_discovery_token(b"reticulum", "fe80::2");
        assert_eq!(tokens[0].0, expected, "token[0] must match eth1, not eth0");
    }

    #[test]
    fn test_active_nics_all_succeed() {
        let nics = vec![
            AdoptedNic {
                name: "eth0".into(),
                link_local: "fe80::1".parse().unwrap(),
                index: 1,
            },
            AdoptedNic {
                name: "eth1".into(),
                link_local: "fe80::2".parse().unwrap(),
                index: 2,
            },
        ];
        let bind_results = vec![true, true];

        let (active, tokens) = build_active_nics_and_tokens(&nics, &bind_results, b"reticulum");

        assert_eq!(active.len(), 2);
        assert_eq!(active[0].name, "eth0");
        assert_eq!(active[1].name, "eth1");
        assert_eq!(tokens.len(), 2);
    }

    #[test]
    fn test_reverse_peering_token_verifiable_by_peer() {
        let group_id = b"reticulum";
        let our_nic = AdoptedNic {
            name: "eth0".into(),
            link_local: "fe80::1".parse().unwrap(),
            index: 1,
        };
        let active_nics = vec![our_nic];
        // Peer's address is different from ours
        let _peer_addr: Ipv6Addr = "fe80::99".parse().unwrap();

        let token =
            compute_reverse_peering_token(group_id, "eth0", &active_nics).expect("should find NIC");

        // Peer receives this token and verifies against our source IP (fe80::1)
        assert!(
            verify_discovery_token(&token, group_id, "fe80::1"),
            "token must verify against sender's source IP, not peer's address"
        );
        // Must NOT verify against the peer's address
        assert!(
            !verify_discovery_token(&token, group_id, "fe80::99"),
            "token must NOT verify against peer's own address"
        );
    }

    #[test]
    fn test_reverse_peering_token_unknown_nic() {
        let active_nics = vec![AdoptedNic {
            name: "eth0".into(),
            link_local: "fe80::1".parse().unwrap(),
            index: 1,
        }];

        let result = compute_reverse_peering_token(b"reticulum", "wlan0", &active_nics);
        assert!(result.is_none(), "unknown NIC should return None");
    }

    #[test]
    fn test_active_nics_all_fail() {
        let nics = vec![AdoptedNic {
            name: "eth0".into(),
            link_local: "fe80::1".parse().unwrap(),
            index: 1,
        }];
        let bind_results = vec![false];

        let (active, tokens) = build_active_nics_and_tokens(&nics, &bind_results, b"reticulum");

        assert!(active.is_empty());
        assert!(tokens.is_empty());
    }

    #[test]
    fn test_peer_info_fields() {
        // Basic construction test, verifies the struct layout compiles
        let (tx, _rx) = mpsc::channel(1);
        let _peer = PeerInfo {
            nic_name: "eth0".to_string(),
            scope_id: 2,
            last_heard: Instant::now(),
            last_reverse_peering: Instant::now(),
            incoming_tx: tx,
            counters: Arc::new(InterfaceCounters::new()),
        };
    }

    /// Bind a UDP socket on [::]:0 for test purposes
    fn bind_test_socket() -> UdpSocket {
        let socket = socket2::Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        socket.set_nonblocking(true).unwrap();
        let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
        socket.bind(&SockAddr::from(bind_addr)).unwrap();
        UdpSocket::from_std(socket.into()).unwrap()
    }

    #[tokio::test]
    async fn test_peer_send_task_exits_on_channel_close() {
        let (outgoing_tx, outgoing_rx) = mpsc::channel(8);
        let socket = Arc::new(bind_test_socket());
        // Use a dummy address, we won't actually receive
        let peer_addr = SocketAddrV6::new("::1".parse().unwrap(), 9999, 0, 0);

        let counters = Arc::new(InterfaceCounters::new());
        let handle = tokio::spawn(async move {
            peer_send_task(outgoing_rx, socket, peer_addr, counters).await;
        });

        // Drop the sender, task should exit
        drop(outgoing_tx);
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("peer_send_task should exit within 2s")
            .expect("task should not panic");
    }

    #[tokio::test]
    async fn test_peer_send_task_forwards_data() {
        let (outgoing_tx, outgoing_rx) = mpsc::channel(8);
        let socket = Arc::new(bind_test_socket());

        // Bind a receiver socket to verify the send arrives
        let recv_socket = bind_test_socket();
        let recv_addr = recv_socket.local_addr().unwrap();
        let recv_v6 = match recv_addr {
            std::net::SocketAddr::V6(v6) => v6,
            _ => panic!("expected v6"),
        };

        // Rewrite to use loopback with correct port
        let peer_addr = SocketAddrV6::new("::1".parse().unwrap(), recv_v6.port(), 0, 0);
        let counters = Arc::new(InterfaceCounters::new());

        tokio::spawn(async move {
            peer_send_task(outgoing_rx, socket, peer_addr, counters).await;
        });

        // Send a packet
        outgoing_tx
            .send(OutgoingPacket {
                peer: None,
                data: b"test data".to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        let mut buf = [0u8; 64];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), recv_socket.recv_from(&mut buf))
            .await
            .expect("timeout")
            .expect("recv error");
        assert_eq!(&buf[..n], b"test data");
    }

    /// Helper to set up a standard test context for handle_discovery_packet tests
    struct DiscoveryTestCtx {
        config: AutoInterfaceConfig,
        nic: AdoptedNic,
        nic_states: HashMap<String, NicState>,
        peers: HashMap<(Ipv6Addr, u16), PeerInfo>,
        peer_count_tx: watch::Sender<usize>,
        _peer_count_rx: watch::Receiver<usize>,
        next_id: Arc<AtomicUsize>,
        new_iface_tx: mpsc::Sender<InterfaceHandle>,
        new_iface_rx: mpsc::Receiver<InterfaceHandle>,
        data_socket: Arc<UdpSocket>,
        our_nonce: [u8; NONCE_SIZE],
        group_tag: Option<String>,
    }

    impl DiscoveryTestCtx {
        fn new() -> Self {
            let (peer_count_tx, _peer_count_rx) = watch::channel(0usize);
            let (new_iface_tx, new_iface_rx) = mpsc::channel(8);
            let mut nic_states = HashMap::new();
            nic_states.insert(
                "eth0".to_string(),
                NicState {
                    last_echo: None,
                    timed_out: false,
                },
            );
            // Bind data socket on port 0 (ephemeral) for tests
            let data_socket = {
                let socket =
                    socket2::Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
                socket.set_nonblocking(true).unwrap();
                let bind_addr = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
                socket.bind(&SockAddr::from(bind_addr)).unwrap();
                Arc::new(UdpSocket::from_std(socket.into()).unwrap())
            };
            let config = AutoInterfaceConfig::default();
            let group_tag = group_name_tag(&config.group_id);
            Self {
                config,
                nic: AdoptedNic {
                    name: "eth0".to_string(),
                    link_local: "fe80::1".parse().unwrap(),
                    index: 1,
                },
                nic_states,
                peers: HashMap::new(),
                peer_count_tx,
                _peer_count_rx,
                next_id: Arc::new(AtomicUsize::new(100)),
                new_iface_tx,
                new_iface_rx,
                data_socket,
                our_nonce: [0x42u8; NONCE_SIZE],
                group_tag,
            }
        }

        fn call(&mut self, data: &[u8], src_addr: Ipv6Addr) {
            handle_discovery_packet(
                data,
                src_addr,
                &self.nic,
                &self.config,
                &self.group_tag,
                &self.our_nonce,
                &mut self.nic_states,
                &mut self.peers,
                &self.peer_count_tx,
                &self.next_id,
                &self.new_iface_tx,
                &self.data_socket,
            );
        }
    }

    #[tokio::test]
    async fn test_self_echo_not_added_as_peer() {
        let mut ctx = DiscoveryTestCtx::new();
        ctx.nic_states.get_mut("eth0").unwrap().timed_out = true;

        // Create a valid 42-byte discovery packet with OUR nonce
        let token = make_discovery_token(&ctx.config.group_id, "fe80::1");
        let pkt = build_discovery_packet(&token, &ctx.our_nonce, ctx.config.data_port);

        ctx.call(&pkt, "fe80::1".parse().unwrap());

        // Should NOT create a peer (self-echo: nonce matches)
        assert!(ctx.peers.is_empty(), "self-echo should not create a peer");
        // Should update echo timestamp
        assert!(
            ctx.nic_states["eth0"].last_echo.is_some(),
            "self-echo should update echo timestamp"
        );
    }

    #[tokio::test]
    async fn test_new_peer_registered() {
        let mut ctx = DiscoveryTestCtx::new();

        // Create a discovery packet from a different instance (different nonce)
        let peer_nonce = [0x99u8; NONCE_SIZE];
        let peer_addr: Ipv6Addr = "fe80::2".parse().unwrap();
        let token = make_discovery_token(&ctx.config.group_id, &peer_addr.to_string());
        let pkt = build_discovery_packet(&token, &peer_nonce, ctx.config.data_port);

        ctx.call(&pkt, peer_addr);

        assert_eq!(ctx.peers.len(), 1, "should have one peer");
        let peer_key = (peer_addr, ctx.config.data_port);
        assert!(ctx.peers.contains_key(&peer_key));

        // Should have sent an InterfaceHandle to the event loop
        let handle = ctx.new_iface_rx.try_recv().expect("should receive handle");
        assert_eq!(handle.info.id, InterfaceId(100));
        assert!(handle.info.name.starts_with("auto/eth0/"));
        assert_eq!(handle.info.hw_mtu, Some(AUTO_HW_MTU));
    }

    #[tokio::test]
    async fn test_known_peer_refreshed() {
        let mut ctx = DiscoveryTestCtx::new();

        let peer_nonce = [0x99u8; NONCE_SIZE];
        let peer_addr: Ipv6Addr = "fe80::2".parse().unwrap();
        let token = make_discovery_token(&ctx.config.group_id, &peer_addr.to_string());
        let pkt = build_discovery_packet(&token, &peer_nonce, ctx.config.data_port);
        let peer_key = (peer_addr, ctx.config.data_port);

        // First discovery
        ctx.call(&pkt, peer_addr);
        let first_heard = ctx.peers[&peer_key].last_heard;

        // Brief delay
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Second discovery, should refresh, not add new
        ctx.call(&pkt, peer_addr);

        assert_eq!(ctx.peers.len(), 1, "should still have one peer");
        assert!(
            ctx.peers[&peer_key].last_heard > first_heard,
            "last_heard should be refreshed"
        );
    }

    #[tokio::test]
    async fn test_invalid_token_rejected() {
        let mut ctx = DiscoveryTestCtx::new();

        // Send a bogus 40-byte packet (correct size but wrong token)
        let bogus = [0u8; DISCOVERY_PACKET_SIZE];
        ctx.call(&bogus, "fe80::2".parse().unwrap());

        assert!(
            ctx.peers.is_empty(),
            "invalid token should not create a peer"
        );
    }

    #[tokio::test]
    async fn test_same_ip_different_nonce_not_self_echo() {
        // Two nodes on the same machine: same IP, different nonces
        let mut ctx = DiscoveryTestCtx::new();
        ctx.next_id = Arc::new(AtomicUsize::new(0));

        let peer_nonce = [0x99u8; NONCE_SIZE];

        // Source IP is our own (fe80::1), but nonce differs → NOT self-echo
        let token = make_discovery_token(&ctx.config.group_id, "fe80::1");
        let pkt = build_discovery_packet(&token, &peer_nonce, ctx.config.data_port);

        ctx.call(&pkt, "fe80::1".parse().unwrap());

        assert_eq!(
            ctx.peers.len(),
            1,
            "same IP but different nonce should create a peer"
        );
        let _handle = ctx
            .new_iface_rx
            .try_recv()
            .expect("should register interface");
    }

    #[tokio::test]
    async fn test_32_byte_python_packet_creates_peer() {
        // 32-byte Python packet (token only, no nonce) should always create a peer
        // and never be treated as self-echo
        let mut ctx = DiscoveryTestCtx::new();
        ctx.next_id = Arc::new(AtomicUsize::new(0));

        let peer_addr: Ipv6Addr = "fe80::3".parse().unwrap();
        let token = make_discovery_token(&ctx.config.group_id, &peer_addr.to_string());

        // Send just the 32-byte token (Python format)
        ctx.call(&token, peer_addr);

        assert_eq!(
            ctx.peers.len(),
            1,
            "32-byte Python packet should create a peer"
        );
        let _handle = ctx
            .new_iface_rx
            .try_recv()
            .expect("should register interface");
    }

    #[tokio::test]
    async fn test_peer_name_tagged_in_non_default_group() {
        // Codeberg #7: a peer discovered in a non-default group carries the
        // group tag in its interface name so it does not collide with the same
        // peer discovered in another group.
        let mut ctx = DiscoveryTestCtx::new();
        ctx.next_id = Arc::new(AtomicUsize::new(0));
        ctx.config.group_id = b"groupA".to_vec();
        ctx.group_tag = group_name_tag(&ctx.config.group_id);
        let expected_tag = ctx.group_tag.clone().expect("non-default group is tagged");

        let peer_nonce = [0x99u8; NONCE_SIZE];
        let peer_addr: Ipv6Addr = "fe80::2".parse().unwrap();
        let token = make_discovery_token(&ctx.config.group_id, &peer_addr.to_string());
        let pkt = build_discovery_packet(&token, &peer_nonce, ctx.config.data_port);

        ctx.call(&pkt, peer_addr);

        let handle = ctx.new_iface_rx.try_recv().expect("should register handle");
        assert!(
            handle.info.name.ends_with(&format!("#{}", expected_tag)),
            "non-default-group peer name should carry the group tag, got {}",
            handle.info.name
        );
    }

    #[tokio::test]
    async fn test_peer_name_untagged_in_default_group() {
        // Default group keeps the historical untagged name.
        let mut ctx = DiscoveryTestCtx::new();
        ctx.next_id = Arc::new(AtomicUsize::new(0));

        let peer_nonce = [0x99u8; NONCE_SIZE];
        let peer_addr: Ipv6Addr = "fe80::2".parse().unwrap();
        let token = make_discovery_token(&ctx.config.group_id, &peer_addr.to_string());
        let pkt = build_discovery_packet(&token, &peer_nonce, ctx.config.data_port);

        ctx.call(&pkt, peer_addr);

        let handle = ctx.new_iface_rx.try_recv().expect("should register handle");
        assert!(
            !handle.info.name.contains('#'),
            "default-group peer name must not carry a group tag, got {}",
            handle.info.name
        );
        assert!(handle.info.name.starts_with("auto/eth0/"));
    }

    /// Two AutoInterface sections with distinct group_ids AND distinct ports
    /// must spawn without an `AddrInUse` panic. On a CI host without a suitable
    /// link-local NIC each orchestrator exits cleanly after enumeration; on a
    /// host with one they bind real (distinct) sockets. Either way the spawn
    /// path must not panic and both peer-count channels start at 0.
    ///
    /// Real cross-group multicast isolation (a peer announced in group A must
    /// not appear in group B) needs an actual multi-NIC LAN and is not
    /// reproducible in CI; it is validated at the address layer by
    /// `test_derive_multicast_address_group_isolation`.
    #[tokio::test]
    async fn test_two_sections_distinct_ports_spawn_without_conflict() {
        let next_id = Arc::new(AtomicUsize::new(0));
        let (new_iface_tx, _new_iface_rx) = mpsc::channel(16);

        let cfg_a = AutoInterfaceConfig {
            group_id: b"groupA".to_vec(),
            discovery_port: 39716,
            data_port: 42671,
            ..AutoInterfaceConfig::default()
        };
        let cfg_b = AutoInterfaceConfig {
            group_id: b"groupB".to_vec(),
            discovery_port: 39816,
            data_port: 42771,
            ..AutoInterfaceConfig::default()
        };

        let rx_a = spawn_auto_interface(next_id.clone(), new_iface_tx.clone(), cfg_a);
        let rx_b = spawn_auto_interface(next_id.clone(), new_iface_tx.clone(), cfg_b);

        // Give the orchestrators a moment to bind or exit cleanly.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(*rx_a.borrow(), 0);
        assert_eq!(*rx_b.borrow(), 0);
    }

    #[tokio::test]
    async fn test_32_byte_packet_from_own_ip_not_self_echo() {
        // Even if the 32-byte packet comes from our own IP, it cannot be
        // self-echo because we only send 40-byte packets
        let mut ctx = DiscoveryTestCtx::new();
        ctx.next_id = Arc::new(AtomicUsize::new(0));

        let token = make_discovery_token(&ctx.config.group_id, "fe80::1");

        // Send 32-byte token from our own link-local address
        ctx.call(&token, "fe80::1".parse().unwrap());

        assert_eq!(
            ctx.peers.len(),
            1,
            "32-byte packet from own IP should still create peer (never self-echo)"
        );
    }
}
