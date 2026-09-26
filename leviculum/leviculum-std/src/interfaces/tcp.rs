//! TCP interfaces (client and server)
//!
//! Client: connects to a Reticulum TCP server (e.g. rnsd TCPServerInterface).
//! Server: listens for incoming connections and spawns an interface per client.
//!
//! Both use HDLC framing to delimit packets on the TCP stream,
//! matching Python Reticulum's `TCPClientInterface` / `TCPServerInterface`.

use std::io;
use std::net::SocketAddr;
#[cfg(test)]
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::inventory::{self as inventory_names, InterfaceIdentity, ListenerRow, SharedInventory};
use super::{IncomingPacket, InterfaceCounters, InterfaceInfo, OutgoingPacket, ReadySignal};
use crate::sync_ext::MutexRecover;
use leviculum_core::constants::MTU;
use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};
use leviculum_core::transport::InterfaceId;
use rand_core::RngCore;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use super::InterfaceHandle;

/// Python `TCPServerInterface.BITRATE_GUESS` (TCPInterface.py:453/564), the
/// bitrate a TCP interface reports when the config sets none.
pub(crate) const TCP_BITRATE_GUESS: i64 = 10_000_000;

/// The hardware MTU a TCP interface signals, and the ceiling handed to the
/// deframer so one peer cannot grow our buffer without limit (Codeberg #271).
///
/// Derived, not chosen: `TCPInterface.HW_MTU` (TCPInterface.py:42) is only what
/// a Python TCP interface carries into `interface_post_init`
/// (Reticulum.py:879), which immediately runs `optimise_mtu` (Interface.py:198)
/// over the interface bitrate — with `TCPServerInterface.BITRATE_GUESS`
/// (TCPInterface.py:453) that lands on 16384 from Reticulum 1.5.2 on and on
/// 8192 before it, and the class value never reaches the wire. We signalled the class value until Codeberg #355, which
/// made a Python client on our shared instance negotiate a 262144-byte link
/// MTU across a TCP hop where an all-Python mesh negotiates 16384 — a 32x
/// larger frame than a Reticulum 1.5.x peer accepts on that hop, whose receive
/// path rejects any frame longer than its own HW_MTU.
///
/// Bounding our own read path is still a deviation from the reference, which
/// bounds only its KISS branch (TCPInterface.py:362) — it is the same value we
/// signal, so no frame a Python peer legitimately sends is affected.
pub(crate) const TCP_HW_MTU: u32 = match super::hw_mtu_for_bitrate(TCP_BITRATE_GUESS) {
    Some(mtu) => mtu,
    // Unreachable for the 10 Mbps guess; the base protocol MTU is the
    // "no link MTU upgrade" answer Python gives for HW_MTU = None.
    None => MTU as u32,
};

/// Default channel buffer size for TCP interfaces.
/// Used for both incoming and outgoing channels.
/// Must be large enough to absorb short bursts during reconnection.
pub(crate) const TCP_DEFAULT_BUFFER_SIZE: usize = 256;

/// TCP liveness parity with Python Reticulum (Codeberg #63).
///
/// Values mirror TCPInterface.py:84-87 / set_timeouts_linux():
/// TCP_USER_TIMEOUT 24 s, SO_KEEPALIVE with idle 5 s / interval 2 s /
/// 12 probes. A silently-dead link (e.g. an iptables-dropped path with
/// no FIN/RST) then surfaces as a read/write error within ~24 s, the
/// driver marks the interface offline, `handle_interface_down` culls
/// its path entries, and the reconnect loop takes over — without these
/// options the kernel defaults let such a connection linger for many
/// minutes. No config surface yet, by design (reference parity).
#[cfg(any(target_os = "linux", target_os = "android"))]
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(24);
const TCP_PROBE_AFTER: Duration = Duration::from_secs(5);
#[cfg(any(target_os = "linux", target_os = "android"))]
const TCP_PROBE_INTERVAL: Duration = Duration::from_secs(2);
#[cfg(any(target_os = "linux", target_os = "android"))]
const TCP_PROBES: u32 = 12;

/// Apply the liveness options above to a TCP socket (std or tokio).
/// Best-effort by contract at the call sites: a socket that cannot take
/// the options still works, it just falls back to kernel default
/// dead-peer detection.
#[cfg(unix)]
fn apply_liveness_options<S: std::os::fd::AsFd>(stream: &S) -> io::Result<()> {
    apply_liveness_sockref(socket2::SockRef::from(stream))
}

#[cfg(windows)]
fn apply_liveness_options<S: std::os::windows::io::AsSocket>(stream: &S) -> io::Result<()> {
    apply_liveness_sockref(socket2::SockRef::from(stream))
}

/// Shared body. The keepalive interval/retry tuning and `TCP_USER_TIMEOUT` are
/// Linux/Android-only (socket2 gates the setters; `TCP_USER_TIMEOUT` is a Linux
/// socket option). Other platforms get the universal idle-keepalive and fall
/// back to kernel-default dead-peer detection (best-effort by contract).
fn apply_liveness_sockref(sock: socket2::SockRef<'_>) -> io::Result<()> {
    use socket2::TcpKeepalive;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let keepalive = TcpKeepalive::new()
            .with_time(TCP_PROBE_AFTER)
            .with_interval(TCP_PROBE_INTERVAL)
            .with_retries(TCP_PROBES);
        sock.set_tcp_keepalive(&keepalive)?;
        sock.set_tcp_user_timeout(Some(TCP_USER_TIMEOUT))?;
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        sock.set_tcp_keepalive(&TcpKeepalive::new().with_time(TCP_PROBE_AFTER))?;
    }
    Ok(())
}

/// Process-global gate for fault injection (`--corrupt-every`).
/// Default `true` so existing invocations are unaffected.
static CORRUPT_ACTIVE: AtomicBool = AtomicBool::new(true);

/// Re-enable byte-flip fault injection on TCP writes.
/// Counterpart of [`disable_fault_injection`].
pub fn enable_fault_injection() {
    CORRUPT_ACTIVE.store(true, Ordering::Relaxed);
}

/// Disable byte-flip fault injection on TCP writes without
/// changing per-iface `corrupt_every` configuration. Used by
/// `lnstest selftest` during Phase-2 mutual discovery so announces
/// cross a clean stream (otherwise a corrupted announce yields
/// a deterministic 60 s Phase-2 timeout — Bug #31).
pub fn disable_fault_injection() {
    CORRUPT_ACTIVE.store(false, Ordering::Relaxed);
}

/// Configuration for a reconnecting TCP client interface.
pub(crate) struct TcpClientConfig {
    pub id: InterfaceId,
    pub name: String,
    pub target_host: String,
    pub target_port: u16,
    pub buffer_size: usize,
    pub corrupt_every: Option<u64>,
    pub reconnect_interval: Duration,
    pub max_reconnect_tries: Option<u64>,
    /// Upper bound on the backoff delay between reconnect attempts. The base
    /// `reconnect_interval` is doubled on each attempt past the third and
    /// clamped here, so a permanently-dead peer is retried at most once per
    /// this interval instead of every `reconnect_interval`. Default 60 s.
    pub reconnect_max_interval: Duration,
    /// Upper bound on a single connect attempt. A connect that does not
    /// complete within this window is abandoned and counted as a failed
    /// attempt, so reconnect accounting (and give-up) stays responsive even
    /// when the OS does not refuse promptly. Platforms differ here: a refused
    /// loopback connect returns instantly on Linux but stalls on SYN-retransmit
    /// for ~1s+ on Windows, and a black-holed peer never refuses at all. The
    /// interface owns this carrier-medium quirk so the driver need not.
    pub connect_timeout: Duration,
    pub reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
    /// Tunnel-synthesize signal (Codeberg #64 initiator side). When present, the
    /// interface fires its `InterfaceId` here on every successful connect (the
    /// initial one AND every reconnect), so the driver initiates the tunnel
    /// synthesize handshake. `None` for KISS-framed or non-tunnel clients, which
    /// mirrors Python's `if not self.kiss_framing: wants_tunnel = True`. The
    /// presence of the channel is the `wants_tunnel` flag; interface isolation
    /// keeps the medium-specific "when to want a tunnel" decision here.
    pub tunnel_notify: Option<mpsc::Sender<InterfaceId>>,
    /// When set, the freshly-connected stream reaches a SOCKS5 proxy rather than
    /// the peer; the interface performs a SOCKS5 CONNECT to this `(host, port)`
    /// before framing starts. The host is sent as a domain name (ATYP=domain),
    /// so the proxy resolves it — an onion or any hostname works without local
    /// DNS. `None` is a direct connection.
    pub socks_target: Option<(String, u16)>,
    /// Detach signal. When it resolves (a value is sent, or the sender is
    /// dropped) the reconnect loop stops and the interface is removed. `None`
    /// for file-config interfaces, which live for the node's lifetime.
    pub shutdown: Option<oneshot::Receiver<()>>,
    /// Run against each freshly created connect socket before it dials. `None`
    /// skips the hook. See [`crate::socket_hook::OutboundSocketHook`].
    pub outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
}

/// Default per-attempt connect timeout for reconnecting TCP clients.
pub(crate) const DEFAULT_TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default cap on the exponential reconnect backoff. See [`backoff_delay`].
pub(crate) const DEFAULT_RECONNECT_MAX_INTERVAL: Duration = Duration::from_secs(60);

/// Fast non-cryptographic PRNG (xorshift64). Seeded from OsRng once per task.
struct Xorshift64(u64);

impl Xorshift64 {
    fn from_entropy() -> Self {
        Self(rand_core::OsRng.next_u64() | 1) // ensure non-zero seed
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Corrupt bytes in `buf` with probability 1/N per byte.
///
/// Uses a fast inline PRNG (not OsRng) to avoid syscalls per byte.
/// XORs with a random non-zero value to guarantee the byte actually changes.
/// Returns the number of bytes corrupted.
fn maybe_corrupt(buf: &mut [u8], every_n: u64, rng: &mut Xorshift64) -> usize {
    if every_n == 0 {
        return 0;
    }
    let mut corrupted = 0;
    for byte in buf.iter_mut() {
        if rng.next().is_multiple_of(every_n) {
            let flip = loop {
                let v = (rng.next() & 0xFF) as u8;
                if v != 0 {
                    break v;
                }
            };
            *byte ^= flip;
            corrupted += 1;
        }
    }
    corrupted
}

/// Frame buffer multiplier (accounts for HDLC escaping overhead)
const FRAME_BUFFER_MULTIPLIER: usize = 2;

/// Read buffer multiplier (handles multiple packets per read)
const READ_BUFFER_MULTIPLIER: usize = 4;

/// Create channels, spawn the I/O task for an already-connected TCP stream,
/// and return the resulting `InterfaceHandle`.
///
/// Used by the TCP server accept loop for each incoming connection.
pub(crate) fn spawn_tcp_interface_from_stream(
    id: InterfaceId,
    name: String,
    stream: tokio::net::TcpStream,
    buffer_size: usize,
    corrupt_every: Option<u64>,
) -> InterfaceHandle {
    let (incoming_tx, incoming_rx) = mpsc::channel(buffer_size);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(buffer_size);
    let counters = Arc::new(InterfaceCounters::new());

    let task_name = name.clone();
    let task_counters = Arc::clone(&counters);

    tokio::spawn(async move {
        let _rx = tcp_interface_task(
            task_name,
            stream,
            incoming_tx,
            outgoing_rx,
            corrupt_every,
            task_counters,
        )
        .await;
    });

    InterfaceHandle {
        info: InterfaceInfo {
            id,
            name,
            hw_mtu: Some(TCP_HW_MTU),
            is_local_client: false,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::Tcp,
            ingress_control: None,
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        // Server-spawned children are pre-signaled — by the time we
        // hand the handle to the registry, the underlying TCP stream
        // already exists and is bidirectionally usable.
        ready: ReadySignal::ready_immediate(),
    }
}

/// Spawn a TCP client interface task (synchronous connect, no reconnect).
///
/// Connects to the given address synchronously (with timeout), then spawns
/// a tokio task that handles all I/O through channels. Returns an
/// `InterfaceHandle` for the event loop to use.
///
/// Production code uses `spawn_tcp_client_with_reconnect` instead. This
/// function is retained for tests that need a one-shot, synchronous connect.
#[cfg(test)]
pub(crate) fn spawn_tcp_interface<A: ToSocketAddrs>(
    id: InterfaceId,
    name: String,
    addr: A,
    connect_timeout: Duration,
    buffer_size: usize,
    corrupt_every: Option<u64>,
) -> Result<InterfaceHandle, io::Error> {
    let addr = addr
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses found"))?;

    let std_stream = std::net::TcpStream::connect_timeout(&addr, connect_timeout)?;
    std_stream.set_nonblocking(true)?;
    std_stream.set_nodelay(true)?;
    apply_liveness_options(&std_stream).ok();
    let stream = tokio::net::TcpStream::from_std(std_stream)?;

    Ok(spawn_tcp_interface_from_stream(
        id,
        name,
        stream,
        buffer_size,
        corrupt_every,
    ))
}

/// Start a TCP server that listens for incoming connections.
///
/// Binds synchronously (so errors propagate to the caller), then spawns
/// an async accept loop. Each accepted connection becomes an
/// `InterfaceHandle` sent to the event loop via `new_interface_tx`.
///
/// The listener itself carries no packets, so it never becomes a routable
/// interface; it is announced to the reporting inventory instead (Codeberg
/// #177), which is what `interface_stats` reports it out of, and every
/// accepted connection registers its reference display identity there before
/// the handle is sent on.
///
/// The accept loop exits when the event loop drops `new_interface_rx`
/// (detected via `Sender::closed()`).
pub(crate) struct TcpServerConfig {
    pub bind_addr: SocketAddr,
    /// Config section name — Python `interface.name`, which appears in both
    /// the listener's and every spawned connection's reported name.
    pub section: String,
    pub next_id: Arc<AtomicUsize>,
    pub new_interface_tx: mpsc::Sender<InterfaceHandle>,
    pub buffer_size: usize,
    pub corrupt_every: Option<u64>,
    pub ifac: Option<leviculum_core::ifac::IfacConfig>,
    pub mode: leviculum_core::traits::InterfaceMode,
    /// Resolved `ingress_control` of the listener, inherited by every accepted
    /// connection (Codeberg #189).
    pub ingress_control: bool,
    /// Inventory id reserved for this listener (its config index) plus the
    /// shared inventory the accept loop registers spawned children in.
    pub listener_id: usize,
    pub inventory: SharedInventory,
    /// Resolved `announce_rate_target/penalty/grace` of the listener.
    pub announce_rate: (Option<u32>, Option<u32>, Option<u32>),
}

pub(crate) fn spawn_tcp_server(config: TcpServerConfig) -> Result<(), io::Error> {
    let TcpServerConfig {
        bind_addr,
        section,
        next_id,
        new_interface_tx,
        buffer_size,
        corrupt_every,
        ifac,
        mode,
        ingress_control,
        listener_id,
        inventory,
        announce_rate,
    } = config;

    // Bind synchronously so errors propagate to the caller immediately
    let std_listener = std::net::TcpListener::bind(bind_addr)?;
    std_listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(std_listener)?;

    // The bound address is what Python reports (`self.bind_ip`/`bind_port`),
    // which matters for a wildcard or port-0 bind.
    let bound = listener.local_addr().unwrap_or(bind_addr);
    tracing::info!("TCP server listening on {}", bound);

    inventory.lock_recover().add_listener(
        listener_id,
        ListenerRow {
            identity: InterfaceIdentity {
                name: inventory_names::tcp_listener_name(&section, bound),
                short_name: section.clone(),
                type_name: "TCPServerInterface",
                parent: None,
            },
            bitrate: TCP_BITRATE_GUESS,
            hw_mtu: TCP_HW_MTU as i64,
            mode,
            announce_rate,
            ifac_size_bits: ifac.as_ref().map(|c| (c.ifac_size() * 8) as i64),
            departed_rxb: 0,
            departed_txb: 0,
            bound_addr: Some(bound),
        },
    );

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer_addr)) => {
                            let id = InterfaceId(next_id.fetch_add(1, Ordering::Relaxed));
                            let name = format!("tcp_server/{}", peer_addr);
                            inventory.lock_recover().add_spawned(
                                id.0,
                                InterfaceIdentity {
                                    name: inventory_names::tcp_spawned_name(&section, peer_addr),
                                    short_name: inventory_names::tcp_spawned_short_name(&section),
                                    // Python spawns a TCPClientInterface for an
                                    // accepted connection (TCPInterface.py:578).
                                    type_name: "TCPClientInterface",
                                    parent: Some(listener_id),
                                },
                            );
                            stream.set_nodelay(true).ok();
                            apply_liveness_options(&stream).ok();
                            let mut handle = spawn_tcp_interface_from_stream(
                                id, name.clone(), stream, buffer_size, corrupt_every,
                            );
                            // Inherit IFAC config from parent TCP server listener.
                            handle.info.ifac = ifac.clone();
                            // Codeberg #104: the accepted (spawned) child inherits
                            // the listener's propagation mode so inbound-side mode
                            // rules (AP/roaming/etc.) apply to peers connecting to
                            // this server, mirroring Python
                            // `spawned_interface.mode = self.mode` (TCPInterface.py:625).
                            handle.info.mode = mode;
                            // Codeberg #189: and its ingress control, mirroring
                            // `spawned_interface.ingress_control =
                            // self.ingress_control` (TCPInterface.py:582). The
                            // operator configures the limiter on the listener,
                            // which is the only entry a config file has for an
                            // accepted connection.
                            handle.info.ingress_control = Some(ingress_control);
                            tracing::info!("Accepted connection: {} ({})", name, id);
                            if new_interface_tx.send(handle).await.is_err() {
                                break; // event loop shut down
                            }
                        }
                        Err(e) => {
                            tracing::warn!("TCP accept error: {}", e);
                        }
                    }
                }
                _ = new_interface_tx.closed() => {
                    tracing::debug!("TCP server shutting down (event loop exited)");
                    break;
                }
            }
        }
    });

    Ok(())
}

/// Spawn a TCP client interface with automatic reconnection.
///
/// Creates the channel pair once and spawns a reconnect task that owns them.
/// The `InterfaceHandle` is returned immediately, the initial connect happens
/// asynchronously in the background, so `start()` returns without blocking.
///
/// During disconnect, the `incoming_tx` stays alive so the driver never sees
/// `Disconnected`. Outgoing packets buffer in the channel (up to `buffer_size`);
/// excess packets are dropped with `BufferFull`. On reconnect, buffered packets
/// are sent on the new stream.
pub(crate) fn spawn_tcp_client_with_reconnect(config: TcpClientConfig) -> InterfaceHandle {
    let (incoming_tx, incoming_rx) = mpsc::channel(config.buffer_size);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(config.buffer_size);
    let counters = Arc::new(InterfaceCounters::new());
    // Offline until the reconnect loop's first successful connect (L-0020);
    // covers the window between registration and the task's first attempt.
    counters.set_online(false);
    let ready = ReadySignal::new();

    let id = config.id;
    let task_name = config.name.clone();
    let task_counters = Arc::clone(&counters);
    let task_ready = Arc::clone(&ready);

    tokio::spawn(async move {
        tcp_client_reconnect_task(
            id,
            config.target_host,
            config.target_port,
            task_name,
            incoming_tx,
            outgoing_rx,
            config.corrupt_every,
            config.reconnect_interval,
            config.max_reconnect_tries,
            config.reconnect_max_interval,
            config.connect_timeout,
            task_counters,
            config.reconnect_notify,
            config.tunnel_notify,
            task_ready,
            config.socks_target,
            config.shutdown,
            config.outbound_socket_hook,
        )
        .await;
    });

    InterfaceHandle {
        info: InterfaceInfo {
            id,
            name: config.name,
            hw_mtu: Some(TCP_HW_MTU),
            is_local_client: false,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::Tcp,
            ingress_control: None,
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        ready,
    }
}

/// Control handle for a TCP client interface added at runtime via
/// [`ReticulumNode::spawn_tcp_client`](crate::driver::ReticulumNode::spawn_tcp_client).
///
/// Hold it to keep the interface attached; drop it (or call [`detach`]) to
/// detach — the reconnect loop stops, its channel closes, and the event loop
/// removes the interface from routing, cleanly, without rebuilding the node.
///
/// [`detach`]: TcpClientHandle::detach
pub struct TcpClientHandle {
    id: InterfaceId,
    // Dropping this sender resolves the task's shutdown receiver, which stops
    // the reconnect loop -> closes the incoming channel -> event loop detaches.
    _shutdown: oneshot::Sender<()>,
}

impl TcpClientHandle {
    pub(crate) fn new(id: InterfaceId, shutdown: oneshot::Sender<()>) -> Self {
        Self {
            id,
            _shutdown: shutdown,
        }
    }

    /// The id the node assigned to this interface.
    pub fn id(&self) -> InterfaceId {
        self.id
    }

    /// Detach the interface now. Equivalent to dropping the handle; provided as
    /// an explicit, self-documenting call for host bindings.
    pub fn detach(self) {}
}

/// Bounded exponential backoff between reconnect attempts (no jitter).
///
/// Attempts `1..=3` wait exactly `base`, so a transient blip heals as fast as a
/// Python peer would (see the deviation note on [`tcp_client_reconnect_task`]).
/// From attempt 4 on the delay doubles each attempt, clamped at `max`. The
/// result is monotonically non-decreasing in `attempt` and never exceeds `max`.
fn backoff_delay(attempt: u64, base: Duration, max: Duration) -> Duration {
    if attempt <= 3 {
        return base.min(max);
    }
    // 1 doubling at attempt 4, 2 at attempt 5, ... All arithmetic saturates so
    // a large attempt count can never overflow; it simply pins to `max`.
    let doublings = attempt - 3;
    let base_nanos = base.as_nanos();
    let scaled = if doublings >= 128 {
        u128::MAX
    } else {
        base_nanos.saturating_mul(1u128 << doublings)
    };
    let capped = scaled.min(max.as_nanos());
    Duration::from_nanos(capped.min(u64::MAX as u128) as u64)
}

/// Deterministic +/-20 % jitter on a backoff delay, keyed on interface name and
/// attempt. No RNG (so it is unit-testable), and two differently-named
/// interfaces retrying the same dead peer draw different offsets, so a fleet of
/// nodes does not reconnect in lockstep.
fn backoff_jitter(name: &str, attempt: u64, delay: Duration) -> Duration {
    // FNV-1a over the name bytes followed by the attempt number.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for b in attempt.to_le_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Offset in parts-per-100_000, uniform over [-20_000, +20_000] => +/-20 %.
    let offset = (hash % 40_001) as i64 - 20_000;
    let nanos = delay.as_nanos() as i128;
    let adjusted = nanos + nanos * offset as i128 / 100_000;
    Duration::from_nanos(adjusted.max(0) as u64)
}

/// Whether a reconnect failure at this attempt should emit a `warn!` line.
///
/// Attempts `1..=3` always log (they mirror the base-rate retries). After that,
/// log only on attempt-count doublings (4, 8, 16, ...): each doubling of the
/// backoff delay gets at most one line, and once the delay caps the spacing
/// keeps widening, so a permanently-dead peer logs at most once per capped
/// interval instead of twice every `reconnect_interval`.
fn should_log_failure(attempt: u64) -> bool {
    attempt <= 3 || attempt.is_power_of_two()
}

/// The single info line emitted on a successful connect. A connect that
/// followed at least one failed attempt reports the recovery (attempt count and
/// how long the peer was gone); a clean first connect just states the endpoint.
/// Exactly one line per successful connect, by construction.
fn connect_log_line(
    name: &str,
    addr_str: &str,
    failed_attempts: u64,
    outage: Option<Duration>,
) -> String {
    if failed_attempts > 0 {
        let elapsed = outage.unwrap_or_default();
        format!("{name}: reconnected to {addr_str} after {failed_attempts} attempt(s), {elapsed:.1?}")
    } else {
        format!("{name}: connected to {addr_str}")
    }
}

/// Reconnect wrapper for TCP client connections.
///
/// Owns the channel endpoints and keeps them alive across reconnection cycles.
/// The driver never sees `RecvEvent::Disconnected`, only a gap in incoming
/// packets during downtime. On reconnection (not the first connect), sends a
/// notification on `reconnect_notify` so the driver can call
/// `handle_interface_up` to re-announce destinations (Block D).
#[allow(clippy::too_many_arguments)]
async fn tcp_client_reconnect_task(
    id: InterfaceId,
    target_host: String,
    target_port: u16,
    name: String,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    corrupt_every: Option<u64>,
    reconnect_interval: Duration,
    max_reconnect_tries: Option<u64>,
    reconnect_max_interval: Duration,
    connect_timeout: Duration,
    counters: Arc<InterfaceCounters>,
    reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
    tunnel_notify: Option<mpsc::Sender<InterfaceId>>,
    ready: Arc<ReadySignal>,
    socks_target: Option<(String, u16)>,
    shutdown: Option<oneshot::Receiver<()>>,
    outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
) {
    // A runtime-added interface carries a detach signal; racing the reconnect
    // loop against it stops the loop at whatever await it is parked on (connect,
    // serve, or backoff). Dropping the handle resolves the receiver, so detach
    // needs no explicit message. File-config interfaces pass `None` and just run
    // the loop directly.
    let loop_fut = tcp_client_reconnect_loop(
        id,
        target_host,
        target_port,
        name.clone(),
        incoming_tx,
        outgoing_rx,
        corrupt_every,
        reconnect_interval,
        max_reconnect_tries,
        reconnect_max_interval,
        connect_timeout,
        counters,
        reconnect_notify,
        tunnel_notify,
        ready,
        socks_target,
        outbound_socket_hook,
    );
    match shutdown {
        Some(sd) => {
            tokio::select! {
                _ = loop_fut => {}
                _ = sd => tracing::info!("{}: detached", name),
            }
        }
        None => loop_fut.await,
    }
}

/// Create an outbound TCP socket, run the hook against it, then dial — so the
/// hook sees the fd before connect.
async fn connect_hooked(
    addr: SocketAddr,
    hook: &Option<crate::socket_hook::OutboundSocketHook>,
) -> io::Result<tokio::net::TcpStream> {
    crate::socket_hook::connect_hooked(addr, hook.as_ref()).await
}

#[allow(clippy::too_many_arguments)]
async fn tcp_client_reconnect_loop(
    id: InterfaceId,
    target_host: String,
    target_port: u16,
    name: String,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    corrupt_every: Option<u64>,
    reconnect_interval: Duration,
    max_reconnect_tries: Option<u64>,
    reconnect_max_interval: Duration,
    connect_timeout: Duration,
    counters: Arc<InterfaceCounters>,
    reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
    tunnel_notify: Option<mpsc::Sender<InterfaceId>>,
    ready: Arc<ReadySignal>,
    socks_target: Option<(String, u16)>,
    outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
) {
    // Backoff DELIBERATELY DEVIATES from Python `RNS/Interfaces/TCPInterface.py`,
    // which uses `RECONNECT_WAIT = 5` and `RECONNECT_MAX_TRIES = None`: a fixed
    // 5 s retry, forever, logging each attempt. A dead peer there costs ~17,280
    // attempts and ~34,560 journal lines per day. This deviation is permitted by
    // the project deviation rule: a client's reconnect cadence and its local
    // logging are wire-invisible and semantics-invisible (no peer observes them),
    // and the change measurably improves Priority-1 operation on constrained
    // nodes (far less wasted work, a readable journal). The trade is that a
    // returning peer's reconnect latency rises from <=`reconnect_interval` to
    // <=`reconnect_max_interval`. We still NEVER give up by default
    // (`max_reconnect_tries = None`): backoff replaces abandonment, which would
    // cost delivery. Attempts 1..=3 stay at the base interval so a transient
    // blip heals exactly as fast as a Python peer would.
    let mut attempt = 0u64;
    let mut has_connected_before = false;
    // Set on the first failed cycle of an outage, cleared on the next success,
    // so the success line can report how long the peer was gone.
    let mut outage_start: Option<Instant> = None;
    loop {
        // Between here and a successful connect the carrier is down; say so
        // (L-0020, Python `self.online = False` across teardown/reconnect).
        counters.set_online(false);
        // Bound each attempt: a connect that does not resolve within
        // `connect_timeout` (Windows SYN-retransmit to a closed loopback port,
        // a black-holed peer that never sends RST) is abandoned and counted,
        // keeping give-up deterministic across platforms.
        let addr_str = format!("{}:{}", target_host, target_port);
        let resolve_result = tokio::net::lookup_host(&addr_str).await;
        
        let connect_result = match resolve_result {
            Ok(mut addrs) => {
                if let Some(addr) = addrs.next() {
                    match tokio::time::timeout(
                        connect_timeout,
                        connect_hooked(addr, &outbound_socket_hook),
                    )
                    .await
                    {
                        Ok(res) => res,
                        Err(_elapsed) => Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "connect attempt timed out",
                        )),
                    }
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "no addresses for host",
                    ))
                }
            }
            Err(e) => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("cannot resolve {}: {}", addr_str, e),
            )),
        };
        // A SOCKS proxy target folds into the connect result: `addr` reached the
        // proxy, and the CONNECT handshake must succeed before the stream is
        // usable. A handshake failure is just a failed attempt, so backoff and
        // give-up accounting stay identical to a direct dial.
        let connect_result = match connect_result {
            Ok(mut stream) => {
                stream.set_nodelay(true).ok();
                apply_liveness_options(&stream).ok();
                match &socks_target {
                    Some((host, port)) => match tokio::time::timeout(
                        connect_timeout,
                        socks5_connect(&mut stream, host, *port),
                    )
                    .await
                    {
                        Ok(Ok(())) => Ok(stream),
                        Ok(Err(e)) => Err(e),
                        Err(_elapsed) => Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "SOCKS5 handshake timed out",
                        )),
                    },
                    None => Ok(stream),
                }
            }
            Err(e) => Err(e),
        };
        match connect_result {
            Ok(stream) => {
                let is_reconnect = has_connected_before;
                let failed_attempts = attempt;
                let outage = outage_start.take();
                has_connected_before = true;
                // RESET the backoff after any successful connect.
                attempt = 0;
                // Per the wait_for_interface_ready contract (Option α):
                // ready fires when the kernel-level TCP three-way
                // handshake has succeeded.  This is idempotent, so
                // reconnects after a drop are safe — the signal stays
                // ready for the lifetime of the interface.
                ready.signal_ready();
                counters.set_online(true);

                // Exactly ONE info line per successful connect.
                tracing::info!(
                    "{}",
                    connect_log_line(&name, &addr_str, failed_attempts, outage.map(|t| t.elapsed()))
                );

                // Notify the driver about reconnection so it can re-announce
                // destinations on the recovered interface (Block D).
                if is_reconnect {
                    if let Some(ref notify) = reconnect_notify {
                        let _ = notify.try_send(id);
                    }
                }

                // Initiate the tunnel synthesize handshake on every successful
                // connect (initial AND reconnect), matching Python's
                // synthesize_tunnel on connect (:179) and reconnect (:297-298).
                // Codeberg #64 initiator side.
                if let Some(ref notify) = tunnel_notify {
                    let _ = notify.try_send(id);
                }

                // Packets queued in outgoing_rx during disconnect will be sent on
                // the new stream. If the channel overflowed (capacity limited),
                // excess packets were dropped by the event loop (BufferFull).
                outgoing_rx = tcp_interface_task(
                    name.clone(),
                    stream,
                    incoming_tx.clone(),
                    outgoing_rx,
                    corrupt_every,
                    Arc::clone(&counters),
                )
                .await;
                tracing::warn!("{}: connection lost, will reconnect", name);
            }
            Err(e) => {
                // The failure warn! is throttled: attempts 1,2,3 and each
                // backoff doubling, then at most once per capped interval.
                // `attempt + 1` is the value the tail below increments to.
                if should_log_failure(attempt + 1) {
                    tracing::warn!(
                        "{}: connect to {} failed: {} (attempt {})",
                        name,
                        addr_str,

                        e,
                        attempt + 1
                    );
                }
            }
        }
        if outage_start.is_none() {
            outage_start = Some(Instant::now());
        }
        attempt += 1;
        if let Some(max) = max_reconnect_tries {
            if attempt >= max {
                tracing::error!("{}: max reconnect attempts ({}) reached", name, max);
                return; // drops incoming_tx → driver sees Disconnected
            }
        }
        // Check if event loop shut down (incoming receiver dropped)
        if incoming_tx.is_closed() {
            tracing::debug!("{}: event loop shut down, stopping reconnect", name);
            return;
        }
        let delay = backoff_jitter(
            &name,
            attempt,
            backoff_delay(attempt, reconnect_interval, reconnect_max_interval),
        );
        tracing::debug!(
            "{}: reconnecting in {:.1?} (attempt {})",
            name,
            delay,
            attempt
        );
        tokio::time::sleep(delay).await;
    }
}

/// Perform a SOCKS5 CONNECT handshake to `host:port` over an established stream.
///
/// No authentication, address type domain — the proxy resolves the host, so an
/// onion or any name works without local DNS. Returns once the proxy confirms
/// the tunnel; framing then proceeds on the same stream transparently.
async fn socks5_connect(
    stream: &mut tokio::net::TcpStream,
    host: &str,
    port: u16,
) -> io::Result<()> {
    use tokio::io::AsyncReadExt;

    let host_bytes = host.as_bytes();
    let host_len = u8::try_from(host_bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "SOCKS5 host too long"))?;

    // Greeting: VER=5, one method offered, NO-AUTH (0x00).
    stream.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut method = [0u8; 2];
    stream.read_exact(&mut method).await?;
    if method != [0x05, 0x00] {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SOCKS5 proxy rejected no-auth",
        ));
    }

    // Request: VER=5, CMD=CONNECT, RSV=0, ATYP=domain, len, host, port.
    let mut req = Vec::with_capacity(7 + host_bytes.len());
    req.extend_from_slice(&[0x05, 0x01, 0x00, 0x03, host_len]);
    req.extend_from_slice(host_bytes);
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await?;

    // Reply: VER, REP, RSV, ATYP, then a bound address we discard.
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("SOCKS5 CONNECT failed (reply {:#04x})", head[1]),
        ));
    }
    let bnd_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SOCKS5 reply has unknown ATYP {other:#04x}"),
            ))
        }
    };
    let mut discard = vec![0u8; bnd_len + 2];
    stream.read_exact(&mut discard).await?;
    Ok(())
}

/// Interface task owning the TCP stream
///
/// Handles bidirectional I/O:
/// - Read path: poll_read_ready → try_read → HDLC deframe → incoming_tx.send()
/// - Write path: outgoing_rx.recv() → HDLC frame → stream.write_all()
///
/// Returns the `outgoing_rx` when the connection is lost, enabling the
/// reconnect wrapper to reuse the channel with a new stream. Packets
/// queued during disconnect are sent on the new connection.
async fn tcp_interface_task(
    name: String,
    stream: tokio::net::TcpStream,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    corrupt_every: Option<u64>,
    counters: Arc<InterfaceCounters>,
) -> mpsc::Receiver<OutgoingPacket> {
    let (reader, mut writer) = stream.into_split();

    let mut deframer = Deframer::with_max_frame(TCP_HW_MTU as usize);
    let mut read_buf = vec![0u8; MTU * READ_BUFFER_MULTIPLIER];
    let mut frame_buf = Vec::with_capacity(MTU * FRAME_BUFFER_MULTIPLIER);
    let mut corrupt_rng = Xorshift64::from_entropy();

    loop {
        tokio::select! {
            // Read path: wait for socket readability, then try_read + deframe
            result = reader.readable() => {
                match result {
                    Ok(()) => {
                        // Drain all available data from the socket
                        loop {
                            match reader.try_read(&mut read_buf) {
                                Ok(0) => {
                                    tracing::debug!("TCP interface {} disconnected (EOF)", name);
                                    return outgoing_rx;
                                }
                                Ok(n) => {
                                    counters.rx_bytes.fetch_add(n as usize, Ordering::Relaxed);
                                    let results = deframer.process(&read_buf[..n]);
                                    for r in results {
                                        // HW_MTU enforcement lives in the deframer now.
                                        if matches!(r, DeframeResult::Oversized) {
                                            tracing::trace!(
                                                "TCP {}: frame exceeds HW_MTU, discarded", name);
                                            continue;
                                        }
                                        if let DeframeResult::Frame(data) = r {
                                            if incoming_tx.send(IncomingPacket { data }).await.is_err() {
                                                // Event loop dropped its receiver
                                                return outgoing_rx;
                                            }
                                        }
                                    }
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    // No more data, go back to select!
                                    break;
                                }
                                Err(e) => {
                                    tracing::debug!("TCP interface {} read error: {}", name, e);
                                    return outgoing_rx;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("TCP interface {} readability error: {}", name, e);
                        return outgoing_rx;
                    }
                }
            }

            // Write path: receive outgoing packets and write to stream
            msg = outgoing_rx.recv() => {
                match msg {
                    Some(pkt) => {
                        frame(&pkt.data, &mut frame_buf);
                        if let Some(n) = corrupt_every {
                            if CORRUPT_ACTIVE.load(Ordering::Relaxed) {
                                let count = maybe_corrupt(&mut frame_buf, n, &mut corrupt_rng);
                                if count > 0 {
                                    tracing::trace!(
                                        "TCP {} corrupted {} byte(s) in {} byte frame",
                                        name, count, frame_buf.len()
                                    );
                                }
                            }
                        }
                        if let Err(e) = writer.write_all(&frame_buf).await {
                            tracing::debug!("TCP interface {} write error: {}", name, e);
                            return outgoing_rx;
                        }
                        counters.tx_bytes.fetch_add(frame_buf.len() as usize, Ordering::Relaxed);
                    }
                    None => {
                        // Event loop dropped its sender, shut down
                        tracing::debug!("TCP interface {} outgoing channel closed", name);
                        return outgoing_rx;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hook fires once, with a real fd, before the connect socket dials.
    #[cfg(unix)]
    #[tokio::test]
    async fn connect_hook_sees_the_socket_before_dial() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let seen = Arc::new(std::sync::Mutex::new(Vec::<std::os::fd::RawFd>::new()));
        let recorded = Arc::clone(&seen);
        let hook: crate::socket_hook::OutboundSocketHook =
            Arc::new(move |fd| recorded.lock().unwrap().push(fd));

        let stream = connect_hooked(addr, &Some(hook)).await.expect("connect");
        assert_eq!(stream.peer_addr().unwrap(), addr);

        let fds = seen.lock().unwrap();
        assert_eq!(fds.len(), 1, "hook invoked exactly once");
        assert!(fds[0] >= 0, "hook received a real fd");
    }

    /// Failure semantics: the hook has no error return, so an embedder whose
    /// socket policy fails (setsockopt EPERM, missing device) can only veto the
    /// dial by panicking. The panic must fail closed — abort the interface task
    /// before connect, so the peer never sees a connection and the interface
    /// detaches cleanly instead of dialing unconfined.
    #[tokio::test]
    async fn panicking_hook_fails_closed_without_dialing() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let hook: crate::socket_hook::OutboundSocketHook = Arc::new(|_fd| {
            panic!("socket policy failed, veto the dial");
        });

        let mut handle = spawn_tcp_client_with_reconnect(TcpClientConfig {
            id: InterfaceId(0),
            name: "test_panic_hook".to_string(),
            target_host: addr.ip().to_string(),
            target_port: addr.port(),
            buffer_size: 16,
            corrupt_every: None,
            reconnect_interval: Duration::from_millis(100),
            max_reconnect_tries: None,
            reconnect_max_interval: DEFAULT_RECONNECT_MAX_INTERVAL,
            connect_timeout: DEFAULT_TCP_CONNECT_TIMEOUT,
            reconnect_notify: None,
            tunnel_notify: None,
            socks_target: None,
            shutdown: None,
            outbound_socket_hook: Some(hook),
        });

        let closed = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv()).await;
        assert!(
            matches!(closed, Ok(None)),
            "interface must detach cleanly after the hook panic"
        );
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "a vetoed dial must never reach the peer");
    }

    #[test]
    fn test_backoff_delay_schedule() {
        let base = Duration::from_secs(5);
        let max = Duration::from_secs(60);
        // Attempts 1..=3 heal at base rate.
        assert_eq!(backoff_delay(1, base, max), base);
        assert_eq!(backoff_delay(2, base, max), base);
        assert_eq!(backoff_delay(3, base, max), base);
        // Then doubling.
        assert_eq!(backoff_delay(4, base, max), Duration::from_secs(10));
        assert_eq!(backoff_delay(5, base, max), Duration::from_secs(20));
        assert_eq!(backoff_delay(6, base, max), Duration::from_secs(40));
        // Clamped at max, never exceeding it.
        assert_eq!(backoff_delay(7, base, max), max); // 80s -> 60s
        assert_eq!(backoff_delay(8, base, max), max); // 160s -> 60s
    }

    #[test]
    fn test_backoff_delay_bounds_and_monotonic() {
        let base = Duration::from_secs(5);
        let max = Duration::from_secs(60);
        let mut prev = Duration::ZERO;
        for attempt in 1..=200u64 {
            let d = backoff_delay(attempt, base, max);
            assert!(d <= max, "attempt {attempt}: {d:?} exceeds max {max:?}");
            assert!(
                d >= prev,
                "attempt {attempt}: {d:?} < prev {prev:?} (not monotonic)"
            );
            prev = d;
        }
        // A large attempt count saturates rather than overflowing.
        assert_eq!(backoff_delay(u64::MAX, base, max), max);
    }

    #[test]
    fn test_backoff_delay_base_above_max_clamps() {
        let base = Duration::from_secs(90);
        let max = Duration::from_secs(60);
        // Even the base-rate attempts never exceed the cap.
        assert_eq!(backoff_delay(1, base, max), max);
        assert_eq!(backoff_delay(4, base, max), max);
    }

    #[test]
    fn test_backoff_jitter_within_twenty_percent() {
        let delay = Duration::from_secs(40);
        let lo = Duration::from_secs(32); // 0.8x
        let hi = Duration::from_secs(48); // 1.2x
        for attempt in 1..=64u64 {
            let j = backoff_jitter("tcp_client_0", attempt, delay);
            assert!(
                j >= lo && j <= hi,
                "attempt {attempt}: {j:?} outside +/-20% of {delay:?}"
            );
        }
    }

    #[test]
    fn test_backoff_jitter_deterministic() {
        let delay = Duration::from_secs(40);
        for attempt in 1..=16u64 {
            let a = backoff_jitter("autoconnect/peer_A", attempt, delay);
            let b = backoff_jitter("autoconnect/peer_A", attempt, delay);
            assert_eq!(
                a, b,
                "jitter must be deterministic for the same (name, attempt)"
            );
        }
    }

    #[test]
    fn test_backoff_jitter_differs_across_names() {
        // Anti-lockstep: different interface names draw different offsets, so a
        // fleet retrying the same dead peer does not knock in unison.
        let delay = Duration::from_secs(40);
        let mut differed = false;
        for attempt in 1..=16u64 {
            let a = backoff_jitter("tcp_client_0", attempt, delay);
            let b = backoff_jitter("tcp_client_1", attempt, delay);
            if a != b {
                differed = true;
                break;
            }
        }
        assert!(
            differed,
            "distinct names must produce distinct jitter for at least one attempt"
        );
    }

    #[test]
    fn test_should_log_failure_throttle() {
        // Always log the first three (base-rate) attempts.
        assert!(should_log_failure(1));
        assert!(should_log_failure(2));
        assert!(should_log_failure(3));
        // Log exactly on the doubling boundaries, silent in between.
        assert!(should_log_failure(4));
        assert!(!should_log_failure(5));
        assert!(!should_log_failure(6));
        assert!(!should_log_failure(7));
        assert!(should_log_failure(8));
        for a in 9..=15u64 {
            assert!(!should_log_failure(a), "attempt {a} should be silent");
        }
        assert!(should_log_failure(16));
        // Once capped, logging spacing keeps widening: at most one line per
        // capped interval. Count the logging attempts in a wide window.
        let logged = (17..=1024u64).filter(|&a| should_log_failure(a)).count();
        // Only 32, 64, 128, 256, 512, 1024 -> 6 lines across ~1000 attempts.
        assert_eq!(logged, 6);
    }

    #[test]
    fn test_connect_log_line_single_variant() {
        let addr_str = "127.0.0.1:9050";
        // Clean first connect: just the endpoint.
        let clean = connect_log_line("tcp_client_0", addr_str, 0, None);
        assert_eq!(clean, "tcp_client_0: connected to 127.0.0.1:9050");
        // A connect that succeeded on attempt N (after N-1... here N failures)
        // reports recovery with the count and elapsed outage as ONE line.
        let recovered = connect_log_line("tcp_client_0", addr_str, 3, Some(Duration::from_secs(12)));
        assert!(recovered.contains("reconnected to 127.0.0.1:9050"));
        assert!(recovered.contains("after 3 attempt(s)"));
        assert!(recovered.contains("12.0s"));
        // Exactly one line either way — no embedded newline.
        assert!(!clean.contains('\n'));
        assert!(!recovered.contains('\n'));
    }

    #[test]
    fn test_backoff_resets_after_success() {
        // The reset is enforced in the task by `attempt = 0` on a successful
        // connect. Model it here: after a success the counter is 0, so the next
        // failure is attempt 1 and waits the base delay again.
        let base = Duration::from_secs(5);
        let max = Duration::from_secs(60);
        // Deep into backoff...
        assert_eq!(backoff_delay(7, base, max), max);
        // ...success resets the counter, so the next failure is attempt 1.
        let after_reset = 1u64;
        assert_eq!(backoff_delay(after_reset, base, max), base);
    }

    #[test]
    fn test_tcp_interface_connect_refused() {
        // Connecting to a port with nothing listening should fail
        let result = spawn_tcp_interface(
            InterfaceId(0),
            "test".to_string(),
            "127.0.0.1:19999",
            Duration::from_millis(500),
            16,
            None,
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_spawn_tcp_interface() {
        // Start a listener, connect via spawn_tcp_interface
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = spawn_tcp_interface(
            InterfaceId(0),
            "test_tcp".to_string(),
            addr,
            Duration::from_secs(2),
            16,
            None,
        )
        .unwrap();

        assert_eq!(handle.info.name, "test_tcp");
        assert_eq!(handle.info.id, InterfaceId(0));

        // Accept the connection on the listener side
        let (_server_stream, _peer) = listener.accept().unwrap();

        // The handle is valid and channels are open
        assert!(!handle.outgoing.is_closed());
    }

    #[test]
    fn test_maybe_corrupt_zero_means_no_corruption() {
        let mut buf = vec![0xAA; 100];
        let original = buf.clone();
        let mut rng = Xorshift64::from_entropy();
        let count = maybe_corrupt(&mut buf, 0, &mut rng);
        assert_eq!(count, 0);
        assert_eq!(buf, original);
    }

    #[test]
    fn test_maybe_corrupt_every_one_corrupts_all() {
        let mut buf = vec![0xAA; 500];
        let original = buf.clone();
        let mut rng = Xorshift64::from_entropy();
        let count = maybe_corrupt(&mut buf, 1, &mut rng);
        assert_eq!(count, 500);
        // XOR with non-zero guarantees every byte changed
        for (i, &b) in buf.iter().enumerate() {
            assert_ne!(b, original[i], "byte {i} should have changed");
        }
    }

    #[test]
    fn test_maybe_corrupt_rare_practically_none() {
        let mut buf = vec![0xAA; 500];
        let original = buf.clone();
        let mut rng = Xorshift64::from_entropy();
        let count = maybe_corrupt(&mut buf, 1_000_000, &mut rng);
        // With 500 bytes and 1/1M probability, expect ~0 corruptions
        assert!(count <= 2, "expected near-zero corruption, got {count}");
        // Most bytes should be unchanged
        let unchanged = buf
            .iter()
            .zip(original.iter())
            .filter(|(a, b)| a == b)
            .count();
        assert!(unchanged >= 498);
    }

    #[test]
    fn test_maybe_corrupt_empty_buffer() {
        let mut buf: Vec<u8> = Vec::new();
        let mut rng = Xorshift64::from_entropy();
        let count = maybe_corrupt(&mut buf, 1, &mut rng);
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_tcp_server_accepts_connection() {
        let next_id = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::channel::<InterfaceHandle>(4);

        // Bind on ephemeral port
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let std_listener = std::net::TcpListener::bind(addr).unwrap();
        let bound_addr = std_listener.local_addr().unwrap();
        drop(std_listener); // free the port for spawn_tcp_server

        spawn_tcp_server(TcpServerConfig {
            bind_addr: bound_addr,
            section: "Test Server".to_string(),
            next_id: next_id.clone(),
            new_interface_tx: tx,
            buffer_size: 16,
            corrupt_every: None,
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            ingress_control: false,
            listener_id: 99,
            inventory: crate::interfaces::inventory::InterfaceInventory::shared(),
            announce_rate: (None, None, None),
        })
        .unwrap();

        // Connect a raw TCP client
        let _client = tokio::net::TcpStream::connect(bound_addr).await.unwrap();

        // Verify an InterfaceHandle arrives on the channel
        let handle = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for handle")
            .expect("channel closed");

        assert!(handle.info.name.starts_with("tcp_server/"));
        assert_eq!(handle.info.id, InterfaceId(0));
        assert!(!handle.outgoing.is_closed());
    }

    #[tokio::test]
    async fn test_tcp_server_spawned_child_inherits_mode() {
        // Codeberg #104: an accepted (spawned-per-connection) interface inherits
        // the listener's propagation mode, mirroring Python
        // `spawned_interface.mode = self.mode` (TCPInterface.py:625).
        use leviculum_core::traits::{Interface, InterfaceMode};

        let next_id = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::channel::<InterfaceHandle>(4);

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let std_listener = std::net::TcpListener::bind(addr).unwrap();
        let bound_addr = std_listener.local_addr().unwrap();
        drop(std_listener);

        spawn_tcp_server(TcpServerConfig {
            bind_addr: bound_addr,
            section: "Test Server".to_string(),
            next_id: next_id.clone(),
            new_interface_tx: tx,
            buffer_size: 16,
            corrupt_every: None,
            ifac: None,
            mode: InterfaceMode::AccessPoint,
            ingress_control: false,
            listener_id: 99,
            inventory: crate::interfaces::inventory::InterfaceInventory::shared(),
            announce_rate: (None, None, None),
        })
        .unwrap();

        let _client = tokio::net::TcpStream::connect(bound_addr).await.unwrap();

        let handle = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for handle")
            .expect("channel closed");

        assert_eq!(
            handle.info.mode,
            InterfaceMode::AccessPoint,
            "spawned child must carry the listener's configured mode in its info"
        );
        assert_eq!(
            Interface::mode(&handle),
            InterfaceMode::AccessPoint,
            "the Interface::mode() trait accessor must report the inherited mode"
        );
    }

    /// Codeberg #189: an accepted connection carries the listener's
    /// `ingress_control` on its handle, mirroring Python
    /// `spawned_interface.ingress_control = self.ingress_control`
    /// (TCPInterface.py:582). Both directions are asserted: before #189 the
    /// driver forced every spawned interface off, so a listener configured ON
    /// could never limit an ingress burst.
    #[tokio::test]
    async fn test_tcp_server_spawned_child_inherits_ingress_control() {
        for listener_ingress in [true, false] {
            let next_id = Arc::new(AtomicUsize::new(0));
            let (tx, mut rx) = mpsc::channel::<InterfaceHandle>(4);

            let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let std_listener = std::net::TcpListener::bind(addr).unwrap();
            let bound_addr = std_listener.local_addr().unwrap();
            drop(std_listener);

            spawn_tcp_server(TcpServerConfig {
                bind_addr: bound_addr,
                section: "Test Server".to_string(),
                next_id: next_id.clone(),
                new_interface_tx: tx,
                buffer_size: 16,
                corrupt_every: None,
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                ingress_control: listener_ingress,
                listener_id: 99,
                inventory: crate::interfaces::inventory::InterfaceInventory::shared(),
                announce_rate: (None, None, None),
            })
            .unwrap();

            let _client = tokio::net::TcpStream::connect(bound_addr).await.unwrap();

            let handle = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timeout waiting for handle")
                .expect("channel closed");

            assert_eq!(
                handle.info.ingress_control,
                Some(listener_ingress),
                "spawned child must carry the listener's ingress_control \
                 (listener had {listener_ingress})"
            );
        }
    }

    #[tokio::test]
    async fn test_tcp_client_reconnects_after_disconnect() {
        use leviculum_core::framing::hdlc;

        // 1. Start TCP listener on ephemeral port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // 2. Spawn reconnecting client with short interval
        let mut handle = spawn_tcp_client_with_reconnect(TcpClientConfig {
            id: InterfaceId(0),
            name: "test_reconnect".to_string(),
            target_host: addr.ip().to_string(),
            target_port: addr.port(),
            buffer_size: 32,
            corrupt_every: None,
            reconnect_interval: Duration::from_millis(200),
            max_reconnect_tries: Some(10),
            reconnect_max_interval: DEFAULT_RECONNECT_MAX_INTERVAL,
            connect_timeout: DEFAULT_TCP_CONNECT_TIMEOUT,
            reconnect_notify: None,
            tunnel_notify: None,
            socks_target: None,
            shutdown: None,
            outbound_socket_hook: None,
        });

        // 3. Accept first connection, send an HDLC-framed packet
        let (mut conn, _peer) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
            .await
            .expect("timeout accepting first connection")
            .unwrap();

        let payload = b"hello-first";
        let mut frame_buf = Vec::new();
        hdlc::frame(payload, &mut frame_buf);
        tokio::io::AsyncWriteExt::write_all(&mut conn, &frame_buf)
            .await
            .unwrap();

        // Verify packet arrives on incoming channel
        let pkt = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv())
            .await
            .expect("timeout waiting for first packet")
            .expect("channel closed");
        assert_eq!(pkt.data, payload);

        // 4. Drop the connection (simulate disconnect)
        drop(conn);

        // 5. Accept the reconnection
        let (mut conn2, _peer2) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("timeout accepting reconnection")
            .unwrap();

        // 6. Send another framed packet on the new connection
        let payload2 = b"hello-second";
        let mut frame_buf2 = Vec::new();
        hdlc::frame(payload2, &mut frame_buf2);
        tokio::io::AsyncWriteExt::write_all(&mut conn2, &frame_buf2)
            .await
            .unwrap();

        // Verify second packet arrives
        let pkt2 = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv())
            .await
            .expect("timeout waiting for second packet")
            .expect("channel closed");
        assert_eq!(pkt2.data, payload2);

        // 7. Outgoing channel should still be open
        assert!(!handle.outgoing.is_closed());
    }

    #[tokio::test]
    async fn test_tcp_client_gives_up_after_max_retries() {
        // Use a port that nothing is listening on (bind and immediately drop)
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // nobody listening

        let mut handle = spawn_tcp_client_with_reconnect(TcpClientConfig {
            id: InterfaceId(0),
            name: "test_giveup".to_string(),
            target_host: addr.ip().to_string(),
            target_port: addr.port(),
            buffer_size: 16,
            corrupt_every: None,
            reconnect_interval: Duration::from_millis(100),
            max_reconnect_tries: Some(2),
            reconnect_max_interval: DEFAULT_RECONNECT_MAX_INTERVAL,
            // Short, explicit bound so give-up is deterministic regardless of
            // how long the OS takes to refuse a dead loopback port (instant on
            // Linux, ~1s+ SYN-retransmit on Windows). 2 tries × (≤300ms connect
            // + 100ms interval) stays well under the 3s test budget everywhere.
            connect_timeout: Duration::from_millis(300),
            reconnect_notify: None,
            tunnel_notify: None,
            socks_target: None,
            shutdown: None,
            outbound_socket_hook: None,
        });

        // Wait for the reconnect task to give up (2 attempts * 100ms + overhead)
        let result = tokio::time::timeout(Duration::from_secs(3), handle.incoming.recv()).await;

        // The incoming channel should close (recv returns None) because
        // the reconnect task dropped incoming_tx after max retries
        match result {
            Ok(None) => {} // expected: channel closed
            Ok(Some(_)) => panic!("should not receive a packet"),
            Err(_) => panic!("timeout — reconnect task did not give up in time"),
        }
    }

    #[tokio::test]
    async fn test_tcp_client_connects_through_socks5_proxy() {
        use leviculum_core::framing::hdlc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A minimal no-auth SOCKS5 proxy: it validates the handshake bytes, then
        // becomes the peer and frames one packet on the same stream.
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut conn, _) = proxy.accept().await.unwrap();
            let mut greeting = [0u8; 3];
            conn.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [0x05, 0x01, 0x00]);
            conn.write_all(&[0x05, 0x00]).await.unwrap();

            let mut head = [0u8; 5];
            conn.read_exact(&mut head).await.unwrap();
            assert_eq!(&head[..4], &[0x05, 0x01, 0x00, 0x03]); // CONNECT, domain
            let host_len = head[4] as usize;
            let mut rest = vec![0u8; host_len + 2];
            conn.read_exact(&mut rest).await.unwrap();
            assert_eq!(&rest[..host_len], b"peer.example");
            assert_eq!(&rest[host_len..], &4242u16.to_be_bytes());
            // Success, bound 0.0.0.0:0 (ATYP=IPv4).
            conn.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            let mut framed = Vec::new();
            hdlc::frame(b"through-socks", &mut framed);
            conn.write_all(&framed).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let mut handle = spawn_tcp_client_with_reconnect(TcpClientConfig {
            id: InterfaceId(0),
            name: "test_socks".to_string(),
            addr: proxy_addr,
            buffer_size: 32,
            corrupt_every: None,
            reconnect_interval: Duration::from_millis(200),
            max_reconnect_tries: Some(1),
            reconnect_max_interval: DEFAULT_RECONNECT_MAX_INTERVAL,
            connect_timeout: DEFAULT_TCP_CONNECT_TIMEOUT,
            reconnect_notify: None,
            tunnel_notify: None,
            socks_target: Some(("peer.example".to_string(), 4242)),
            shutdown: None,
            outbound_socket_hook: None,
        });

        let pkt = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv())
            .await
            .expect("timeout waiting for packet through proxy")
            .expect("channel closed");
        assert_eq!(pkt.data, b"through-socks");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_tcp_client_detaches_on_shutdown() {
        // No give-up bound: only the detach signal can stop this loop, so a
        // closed incoming channel proves the shutdown path fired.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut handle = spawn_tcp_client_with_reconnect(TcpClientConfig {
            id: InterfaceId(0),
            name: "test_detach".to_string(),
            addr,
            buffer_size: 16,
            corrupt_every: None,
            reconnect_interval: Duration::from_millis(100),
            max_reconnect_tries: None,
            reconnect_max_interval: DEFAULT_RECONNECT_MAX_INTERVAL,
            connect_timeout: DEFAULT_TCP_CONNECT_TIMEOUT,
            reconnect_notify: None,
            tunnel_notify: None,
            socks_target: None,
            shutdown: Some(shutdown_rx),
            outbound_socket_hook: None,
        });
        let _ = tokio::time::timeout(Duration::from_millis(500), listener.accept()).await;
        drop(shutdown_tx); // resolves the receiver -> loop stops -> incoming closes
        let closed = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv()).await;
        assert!(matches!(closed, Ok(None)), "incoming closes after detach");
    }
}
