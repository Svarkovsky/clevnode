//! UDP interface
//!
//! Point-to-point or broadcast UDP with fixed addresses.
//! No discovery, no peer management, no framing, each datagram is one
//! Reticulum packet. Matches Python's `UDPInterface`.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::{IncomingPacket, InterfaceCounters, InterfaceHandle, InterfaceInfo, OutgoingPacket};
use leviculum_core::transport::InterfaceId;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Maximum datagram size accepted from the wire.
/// Matches Python `UDPInterface.HW_MTU = 1064` (UDPInterface.py:74).
/// Core already ensures outgoing packets are <= 500 bytes (protocol MTU),
/// so this only bounds the recv buffer.
const UDP_MTU: usize = 1064;

/// Default channel buffer size for UDP interfaces.
const UDP_DEFAULT_BUFFER_SIZE: usize = 256;

/// Why parsing the `forward_ip` config value failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ForwardAddrError {
    /// A plain-IP entry needs `forward_port`, which is unset.
    MissingPort,
    /// An entry is not a valid address; carries the entry and parse error.
    Invalid(String),
}

/// One configured forward destination: a literal socket address, or a
/// hostname resolved at interface runtime (Codeberg #148).
///
/// Python's `UDPInterface.process_outgoing` passes `forward_ip` straight to
/// `sendto` (UDPInterface.py:124-127), so the OS resolves a hostname on
/// every send. A config naming its UDP peer by hostname therefore works
/// against rnsd and must work here too; resolution failures are interface
/// errors, never config errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForwardTarget {
    /// Numeric address, used as-is.
    Literal(SocketAddr),
    /// Hostname plus port, resolved by the interface's resolver.
    Named { host: String, port: u16 },
}

impl std::fmt::Display for ForwardTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ForwardTarget::Literal(addr) => addr.fmt(f),
            ForwardTarget::Named { host, port } => write!(f, "{}:{}", host, port),
        }
    }
}

impl From<SocketAddr> for ForwardTarget {
    fn from(addr: SocketAddr) -> Self {
        ForwardTarget::Literal(addr)
    }
}

/// Liberal hostname plausibility check. getaddrinfo is the real authority
/// (Python performs no validation at all — a bad name fails per send); this
/// only rejects entries that cannot possibly be a hostname, so config typos
/// like stray spaces still fail at parse time.
fn is_plausible_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// Parse the `forward_ip` config value into forward targets.
///
/// `forward_ip` is a comma-separated list. Each entry is a plain IP address
/// or hostname (combined with `forward_port`), or an `ip:port` (IPv6:
/// `[ip]:port`) / `host:port` form carrying its own port. A single plain
/// value therefore parses exactly as rnsd reads it, keeping unchanged
/// rnsd-style configs working; multiple entries and per-entry ports are a
/// Rust-only extension (Python's UDPInterface supports one forward address).
pub(crate) fn parse_forward_addrs(
    forward_ip: &str,
    forward_port: Option<u16>,
) -> Result<Vec<ForwardTarget>, ForwardAddrError> {
    let mut targets = Vec::new();
    for entry in forward_ip.split(',') {
        let entry = entry.trim();
        // ip:port / [ipv6]:port with its own port
        if let Ok(addr) = entry.parse::<SocketAddr>() {
            targets.push(ForwardTarget::Literal(addr));
            continue;
        }
        // Plain IP (v4 or v6) plus forward_port. SocketAddr::new (instead of
        // reparsing a formatted string) keeps a bare IPv6 entry valid.
        if let Ok(ip) = entry.parse::<IpAddr>() {
            let port = forward_port.ok_or(ForwardAddrError::MissingPort)?;
            targets.push(ForwardTarget::Literal(SocketAddr::new(ip, port)));
            continue;
        }
        // host:port with its own port
        if let Some((host, port_str)) = entry.rsplit_once(':') {
            if let Ok(port) = port_str.parse::<u16>() {
                if is_plausible_hostname(host) {
                    targets.push(ForwardTarget::Named {
                        host: host.to_string(),
                        port,
                    });
                    continue;
                }
            }
        }
        // Bare hostname plus forward_port
        if is_plausible_hostname(entry) {
            let port = forward_port.ok_or(ForwardAddrError::MissingPort)?;
            targets.push(ForwardTarget::Named {
                host: entry.to_string(),
                port,
            });
            continue;
        }
        return Err(ForwardAddrError::Invalid(format!(
            "\"{}\": not an address or hostname",
            entry
        )));
    }
    Ok(targets)
}

/// Async hostname resolver: `(host, port)` to resolved socket addresses.
/// Injectable so tests can fail or redirect resolution deterministically.
pub(crate) type Resolver = Arc<
    dyn Fn(String, u16) -> Pin<Box<dyn Future<Output = io::Result<Vec<SocketAddr>>> + Send>>
        + Send
        + Sync,
>;

/// OS resolver via getaddrinfo, like Python's `sendto`.
fn system_resolver() -> Resolver {
    Arc::new(|host, port| {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
                .await?
                .collect();
            Ok(addrs)
        })
    })
}

/// Re-resolve a healthy name this long after its last successful resolution,
/// so a long-running daemon follows a peer whose address changes (container
/// restart, DHCP). Python re-resolves on every send; a bounded refresh off
/// the send path gives the same effect without ever blocking I/O on a slow
/// resolver.
const UDP_RESOLVE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// Minimum spacing between resolution attempts, bounding resolver load and
/// log volume while sends are failing or a refresh keeps erroring.
const UDP_RESOLVE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// How named forward targets are resolved and refreshed.
#[derive(Clone)]
pub(crate) struct UdpResolveOpts {
    pub resolver: Resolver,
    /// Re-resolution age for a successfully resolved name.
    pub refresh_interval: Duration,
    /// Minimum spacing between resolution attempts.
    pub retry_interval: Duration,
}

impl Default for UdpResolveOpts {
    fn default() -> Self {
        Self {
            resolver: system_resolver(),
            refresh_interval: UDP_RESOLVE_REFRESH_INTERVAL,
            retry_interval: UDP_RESOLVE_RETRY_INTERVAL,
        }
    }
}

/// Create channels, bind the socket, spawn the I/O task, and return
/// the resulting `InterfaceHandle`.
///
/// # Arguments
/// * `id` - Interface identifier assigned by the driver
/// * `name` - Human-readable name for logging
/// * `listen_addr` - Local address to bind (receive datagrams)
/// * `forward_targets` - Remote targets for outgoing datagrams; each
///   outgoing datagram is sent to every one. Must be non-empty.
///   Python's UDPInterface has exactly one forward address; more than
///   one is a Rust-only extension with no wire difference per receiver.
pub(crate) fn spawn_udp_interface(
    id: InterfaceId,
    name: String,
    listen_addr: SocketAddr,
    forward_targets: Vec<ForwardTarget>,
) -> io::Result<InterfaceHandle> {
    // Bind synchronously so errors propagate to the caller immediately.
    let std_socket = std::net::UdpSocket::bind(listen_addr)?;
    spawn_udp_interface_from_socket(id, name, std_socket, forward_targets)
}

/// Like [`spawn_udp_interface`] but adopts an already-bound socket instead of
/// binding `listen_addr` itself. A caller that must learn the OS-assigned
/// ephemeral port BEFORE spawning (e.g. two interfaces that forward to each
/// other) can bind, read `local_addr()`, and hand over the live socket without
/// the bind -> drop -> rebind window that races another binder for the same
/// port under parallel test execution.
pub(crate) fn spawn_udp_interface_from_socket(
    id: InterfaceId,
    name: String,
    std_socket: std::net::UdpSocket,
    forward_targets: Vec<ForwardTarget>,
) -> io::Result<InterfaceHandle> {
    spawn_udp_interface_with_opts(
        id,
        name,
        std_socket,
        forward_targets,
        UdpResolveOpts::default(),
    )
}

/// Like [`spawn_udp_interface_from_socket`] with explicit resolution options
/// (injectable resolver and refresh timing) for tests.
pub(crate) fn spawn_udp_interface_with_opts(
    id: InterfaceId,
    name: String,
    std_socket: std::net::UdpSocket,
    forward_targets: Vec<ForwardTarget>,
    resolve_opts: UdpResolveOpts,
) -> io::Result<InterfaceHandle> {
    if forward_targets.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UDP interface needs at least one forward address",
        ));
    }
    std_socket.set_nonblocking(true)?;
    // SO_BROADCAST is a permission flag, harmless on non-broadcast sockets.
    // Matches Python behavior (UDPInterface.py:123).
    std_socket.set_broadcast(true)?;
    let socket = tokio::net::UdpSocket::from_std(std_socket)?;

    let (incoming_tx, incoming_rx) = mpsc::channel(UDP_DEFAULT_BUFFER_SIZE);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(UDP_DEFAULT_BUFFER_SIZE);
    let counters = Arc::new(InterfaceCounters::new());

    let task_name = name.clone();
    let task_counters = Arc::clone(&counters);

    tokio::spawn(async move {
        udp_io_task(
            task_name,
            socket,
            forward_targets,
            resolve_opts,
            incoming_tx,
            outgoing_rx,
            task_counters,
        )
        .await;
    });

    Ok(InterfaceHandle {
        info: InterfaceInfo {
            id,
            name,
            hw_mtu: Some(1064),
            is_local_client: false,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::Udp,
            ingress_control: None,
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        // UDP sockets are bound before the handle is returned;
        // immediate-ready.
        ready: super::ReadySignal::ready_immediate(),
    })
}

/// Runtime state of one forward target inside the I/O task.
enum TargetState {
    Literal(SocketAddr),
    Named(NamedState),
}

/// Resolution state of a hostname target: the last successfully resolved
/// address (kept as last-known-good across later failures) plus the timing
/// gates for refresh and retry.
struct NamedState {
    host: String,
    port: u16,
    cached: Option<SocketAddr>,
    last_attempt: Option<Instant>,
    last_success: Option<Instant>,
    in_flight: bool,
}

/// Start a background resolution for `st` if one is due: never while one is
/// in flight, at most every `retry_interval`, and for an already-resolved
/// name only once its `refresh_interval` age has passed. The result comes
/// back through `resolve_tx`, so the I/O loop never waits on the resolver.
fn kick_resolution(
    idx: usize,
    st: &mut NamedState,
    opts: &UdpResolveOpts,
    resolve_tx: &mpsc::Sender<(usize, io::Result<Vec<SocketAddr>>)>,
) {
    if st.in_flight {
        return;
    }
    let now = Instant::now();
    let attempt_due = st
        .last_attempt
        .is_none_or(|t| now.duration_since(t) >= opts.retry_interval);
    let refresh_due = st
        .last_success
        .is_none_or(|t| now.duration_since(t) >= opts.refresh_interval);
    if !(attempt_due && refresh_due) {
        return;
    }
    st.in_flight = true;
    st.last_attempt = Some(now);
    let fut = (opts.resolver)(st.host.clone(), st.port);
    let tx = resolve_tx.clone();
    tokio::spawn(async move {
        let result = fut.await;
        // The I/O task owns the receiver; if it is gone the interface is
        // shutting down and the result is moot.
        let _ = tx.send((idx, result)).await;
    });
}

/// Pick the resolved address to send to: same family as the bound socket
/// first (the only family `send_to` can reach), otherwise the first entry
/// so the send error names the mismatch.
fn select_addr(addrs: &[SocketAddr], local_is_v6: bool) -> SocketAddr {
    addrs
        .iter()
        .find(|a| a.is_ipv6() == local_is_v6)
        .copied()
        .unwrap_or(addrs[0])
}

/// Fold a completed resolution into the target state. Failures keep the
/// last-known-good address: sending to a possibly stale peer beats sending
/// nothing while DNS hiccups, and the retry gate keeps re-checking.
fn apply_resolution(
    name: &str,
    st: &mut NamedState,
    result: io::Result<Vec<SocketAddr>>,
    local_is_v6: bool,
) {
    st.in_flight = false;
    match result {
        Ok(addrs) if !addrs.is_empty() => {
            let addr = select_addr(&addrs, local_is_v6);
            st.last_success = Some(Instant::now());
            if st.cached != Some(addr) {
                tracing::info!(
                    "UDP {} forward {}:{} resolved to {}",
                    name,
                    st.host,
                    st.port,
                    addr
                );
            }
            st.cached = Some(addr);
        }
        Ok(_) => {
            tracing::warn!(
                "UDP {} forward {}:{} resolved to no addresses{}",
                name,
                st.host,
                st.port,
                keeping_note(st)
            );
        }
        Err(e) => {
            tracing::warn!(
                "UDP {} cannot resolve forward {}:{}: {}{}",
                name,
                st.host,
                st.port,
                e,
                keeping_note(st)
            );
        }
    }
}

fn keeping_note(st: &NamedState) -> String {
    match st.cached {
        Some(addr) => format!("; keeping last-known address {}", addr),
        None => String::new(),
    }
}

/// Single I/O task owning the UDP socket.
///
/// Handles bidirectional I/O:
/// - Read path: `recv_from()` → `incoming_tx.send()`
/// - Write path: `outgoing_rx.recv()` → `send_to()` to every forward target
/// - Resolution path: background lookups for named targets report back over
///   an internal channel; the loop itself never blocks on the resolver
///
/// Recv errors are transient: log (throttled) and keep receiving (L-0021).
/// That is the reference behavior — Python-RNS serves UDP via
/// `socketserver.UDPServer.serve_forever`, whose
/// `BaseServer._handle_request_noblock` swallows a failing `get_request()`
/// outright (`except OSError: return`, CPython socketserver.py), so an rnsd
/// UDP interface survives every recv error; ours used to die on the first
/// (a single ECONNREFUSED from an ICMP port-unreachable killed it until
/// daemon restart). Consecutive errors back off briefly so a socket that
/// fails every call (e.g. gone bad) cannot spin the loop hot.
/// Send errors are logged but do not kill the interface, and a failed send
/// to one forward target does not skip the remaining targets. UDP send
/// errors (network unreachable, host unreachable) are transient, and so are
/// resolution failures: an unresolved name drops that target's copy of the
/// packet with a warning and is retried on later sends.
async fn udp_io_task(
    name: String,
    socket: tokio::net::UdpSocket,
    forward_targets: Vec<ForwardTarget>,
    resolve_opts: UdpResolveOpts,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
) {
    let mut buf = [0u8; UDP_MTU];
    let local_is_v6 = socket.local_addr().map(|a| a.is_ipv6()).unwrap_or(false);

    let mut targets: Vec<TargetState> = forward_targets
        .into_iter()
        .map(|t| match t {
            ForwardTarget::Literal(addr) => TargetState::Literal(addr),
            ForwardTarget::Named { host, port } => TargetState::Named(NamedState {
                host,
                port,
                cached: None,
                last_attempt: None,
                last_success: None,
                in_flight: false,
            }),
        })
        .collect();

    // One slot per target bounds the channel even if every lookup completes
    // at once; the loop holds the sender, so recv() never yields None here.
    let (resolve_tx, mut resolve_rx) =
        mpsc::channel::<(usize, io::Result<Vec<SocketAddr>>)>(targets.len().max(1));

    // Eager first resolution so the earliest outgoing packets already have
    // an address instead of paying one dropped send per named target.
    for (idx, target) in targets.iter_mut().enumerate() {
        if let TargetState::Named(st) = target {
            kick_resolution(idx, st, &resolve_opts, &resolve_tx);
        }
    }

    // Consecutive-recv-error streak, reset by any successful recv. Drives
    // the log throttle and the anti-spin backoff below.
    let mut recv_error_streak: u64 = 0;

    loop {
        tokio::select! {
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, _src_addr)) => {
                        recv_error_streak = 0;
                        if len > 0 && len <= UDP_MTU {
                            counters.rx_bytes.fetch_add(len as usize, Ordering::Relaxed);
                            if incoming_tx
                                .send(IncomingPacket {
                                    data: buf[..len].to_vec(),
                                })
                                .await
                                .is_err()
                            {
                                // Event loop dropped its receiver
                                tracing::debug!("UDP {} incoming channel closed", name);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        // Transient by reference (see the task docs): log
                        // throttled — the first few, then each doubling —
                        // and keep receiving.
                        recv_error_streak += 1;
                        if recv_error_streak <= 3 || recv_error_streak.is_power_of_two() {
                            tracing::warn!(
                                "UDP {} recv error (#{} in a row, continuing): {}",
                                name,
                                recv_error_streak,
                                e
                            );
                        }
                        // A socket that fails EVERY call must not spin the
                        // loop hot; one error costs no delay, a streak eases
                        // off. Wire-invisible, so the deviation rule allows
                        // it (Python's selector timeout paces its loop too).
                        if recv_error_streak > 1 {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                (10 * recv_error_streak).min(1_000),
                            ))
                            .await;
                        }
                    }
                }
            }

            completed = resolve_rx.recv() => {
                if let Some((idx, result)) = completed {
                    if let Some(TargetState::Named(st)) = targets.get_mut(idx) {
                        apply_resolution(&name, st, result, local_is_v6);
                    }
                }
            }

            msg = outgoing_rx.recv() => {
                match msg {
                    Some(pkt) => {
                        for (idx, target) in targets.iter_mut().enumerate() {
                            let addr = match target {
                                TargetState::Literal(addr) => *addr,
                                TargetState::Named(st) => {
                                    // Refresh off the send path: stale or
                                    // unresolved names kick a background
                                    // lookup, the send uses what is cached.
                                    kick_resolution(idx, st, &resolve_opts, &resolve_tx);
                                    match st.cached {
                                        Some(addr) => addr,
                                        None => {
                                            tracing::warn!(
                                                "UDP {} dropping packet for {}:{}: \
                                                 hostname not resolved yet",
                                                name,
                                                st.host,
                                                st.port
                                            );
                                            continue;
                                        }
                                    }
                                }
                            };
                            match socket.send_to(&pkt.data, addr).await {
                                Ok(n) => {
                                    counters.tx_bytes.fetch_add(n as usize, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    tracing::warn!("UDP {} send error to {}: {}", name, addr, e);
                                    // Don't break, send errors are transient for
                                    // UDP; keep sending to the other targets
                                }
                            }
                        }
                    }
                    None => {
                        // Event loop dropped its sender, shut down
                        tracing::debug!("UDP {} outgoing channel closed", name);
                        break;
                    }
                }
            }
        }
    }
    // Dropping incoming_tx signals interface-down to the event loop
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// L-0021: one `recv_from` error must not end the UDP I/O task. The
    /// reference never dies here: Python-RNS serves UDP via
    /// `socketserver.UDPServer.serve_forever`, whose
    /// `BaseServer._handle_request_noblock` swallows the failing
    /// `get_request()` outright (`except OSError: return`, CPython
    /// socketserver.py) and keeps serving — so an rnsd UDP interface
    /// survives every recv error for the process lifetime, while ours died
    /// on the first one until the daemon restarted.
    ///
    /// The reproducible recv error on Linux: a CONNECTED UDP socket that
    /// sent to a closed local port gets the ICMP port-unreachable queued
    /// as `ECONNREFUSED` on its next `recv`.
    #[tokio::test]
    async fn recv_error_does_not_kill_the_interface() {
        // A dead port: bound once to learn a free number, then closed.
        let dead = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);

        let victim = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let victim_addr = victim.local_addr().unwrap();
        victim.connect(dead_addr).unwrap();

        // Positive control: this platform really surfaces the ICMP error as
        // a recv failure — otherwise the rest of the test proves nothing.
        victim.send(b"probe").unwrap();
        victim
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let control = victim.recv(&mut [0u8; 16]);
        assert!(
            control.is_err(),
            "positive control: sending to a closed local port must queue a \
             recv error, got {control:?}"
        );
        victim.set_read_timeout(None).unwrap();

        // Queue a FRESH pending error and hand the socket to the interface
        // task un-drained: its first recv_from consumes the ECONNREFUSED.
        victim.send(b"probe").unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let mut handle = spawn_udp_interface_from_socket(
            InterfaceId(0),
            "udp_victim".into(),
            victim,
            vec![dead_addr.into()],
        )
        .unwrap();

        // After the error, a datagram from the connected peer's address must
        // still come through — i.e. the recv loop is alive. Re-send with a
        // short poll: the exact interleaving of the queued error and the
        // datagram is the kernel's business, not the test's.
        let sender = tokio::net::UdpSocket::bind(dead_addr)
            .await
            .expect("rebind the freed port as the peer");
        let mut received = None;
        for _ in 0..20 {
            sender.send_to(b"still-alive", victim_addr).await.unwrap();
            if let Ok(Some(pkt)) =
                tokio::time::timeout(Duration::from_millis(250), handle.incoming.recv()).await
            {
                received = Some(pkt);
                break;
            }
        }
        let received = received.expect(
            "L-0021: the UDP interface died on a single recv error \
             (no datagram surfaced after it)",
        );
        assert_eq!(received.data, b"still-alive");
    }

    #[tokio::test]
    async fn test_udp_loopback() {
        // Two UDP interfaces on localhost pointing at each other
        let addr_a: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let addr_b: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Bind both sockets up front to learn the OS-assigned ports, then hand
        // the LIVE sockets to the interfaces. Dropping and rebinding the same
        // port opens a window where a parallel test binding an ephemeral port
        // steals it, which is the source of this test's flakiness.
        let std_a = std::net::UdpSocket::bind(addr_a).unwrap();
        let bound_a = std_a.local_addr().unwrap();
        let std_b = std::net::UdpSocket::bind(addr_b).unwrap();
        let bound_b = std_b.local_addr().unwrap();

        // A listens on bound_a, forwards to bound_b
        let mut handle_a = spawn_udp_interface_from_socket(
            InterfaceId(0),
            "udp_a".into(),
            std_a,
            vec![bound_b.into()],
        )
        .unwrap();
        // B listens on bound_b, forwards to bound_a
        let mut handle_b = spawn_udp_interface_from_socket(
            InterfaceId(1),
            "udp_b".into(),
            std_b,
            vec![bound_a.into()],
        )
        .unwrap();

        // Send from A → B
        let payload = b"hello from A";
        handle_a
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: payload.to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        let pkt = tokio::time::timeout(Duration::from_secs(2), handle_b.incoming.recv())
            .await
            .expect("timeout waiting for packet at B")
            .expect("channel closed");
        assert_eq!(pkt.data, payload);

        // Send from B → A
        let payload2 = b"hello from B";
        handle_b
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: payload2.to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        let pkt2 = tokio::time::timeout(Duration::from_secs(2), handle_a.incoming.recv())
            .await
            .expect("timeout waiting for packet at A")
            .expect("channel closed");
        assert_eq!(pkt2.data, payload2);
    }

    #[tokio::test]
    async fn test_udp_send_error_does_not_kill_interface() {
        // Interface that sends to an unreachable address but listens on a real port
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        // Port 1 is almost certainly unreachable/firewalled
        let unreachable: SocketAddr = "192.0.2.1:1".parse().unwrap();

        // Hand the live socket to the interface (no drop/rebind race); `bound`
        // is reused below to send a datagram directly to the interface's port.
        let std_sock = std::net::UdpSocket::bind(listen).unwrap();
        let bound = std_sock.local_addr().unwrap();

        let mut handle = spawn_udp_interface_from_socket(
            InterfaceId(0),
            "udp_unreachable".into(),
            std_sock,
            vec![unreachable.into()],
        )
        .unwrap();

        // Send to unreachable, should not crash
        handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: b"test".to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        // Brief delay so the task processes the send
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Interface should still be alive (outgoing channel open)
        assert!(!handle.outgoing.is_closed());

        // Verify we can still receive: send a datagram directly to the interface
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"still alive", bound).await.unwrap();

        let pkt = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv())
            .await
            .expect("timeout — interface should still receive")
            .expect("channel closed");
        assert_eq!(pkt.data, b"still alive");
    }

    #[tokio::test]
    async fn test_udp_interface_info() {
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let forward: SocketAddr = "127.0.0.1:9999".parse().unwrap();

        let handle = spawn_udp_interface(
            InterfaceId(42),
            "my_udp".into(),
            listen,
            vec![forward.into()],
        )
        .unwrap();

        assert_eq!(handle.info.id, InterfaceId(42));
        assert_eq!(handle.info.name, "my_udp");
        assert!(!handle.outgoing.is_closed());
    }

    #[tokio::test]
    async fn test_udp_multi_forward_delivers_to_all() {
        // Two listener sockets on OS-assigned ephemeral ports
        let listener_1 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listener_2 = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_1 = listener_1.local_addr().unwrap();
        let addr_2 = listener_2.local_addr().unwrap();

        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = spawn_udp_interface(
            InterfaceId(0),
            "udp_multi".into(),
            listen,
            vec![addr_1.into(), addr_2.into()],
        )
        .unwrap();

        let payload = b"fan out";
        handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: payload.to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        let mut buf = [0u8; UDP_MTU];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), listener_1.recv_from(&mut buf))
            .await
            .expect("timeout waiting at listener 1")
            .expect("recv failed at listener 1");
        assert_eq!(&buf[..len], payload);

        let (len, _) = tokio::time::timeout(Duration::from_secs(2), listener_2.recv_from(&mut buf))
            .await
            .expect("timeout waiting at listener 2")
            .expect("recv failed at listener 2");
        assert_eq!(&buf[..len], payload);
    }

    #[tokio::test]
    async fn test_udp_multi_forward_error_on_one_addr_still_delivers_to_other() {
        let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let reachable = listener.local_addr().unwrap();
        // TEST-NET-1, never routable
        let unreachable: SocketAddr = "192.0.2.1:1".parse().unwrap();

        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = spawn_udp_interface(
            InterfaceId(0),
            "udp_multi_err".into(),
            listen,
            vec![unreachable.into(), reachable.into()],
        )
        .unwrap();

        let payload = b"past the error";
        handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: payload.to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        // The failing first address must not prevent delivery to the second
        let mut buf = [0u8; UDP_MTU];
        let (len, _) = tokio::time::timeout(Duration::from_secs(2), listener.recv_from(&mut buf))
            .await
            .expect("timeout — send error to one address blocked the others")
            .expect("recv failed");
        assert_eq!(&buf[..len], payload);
        assert!(!handle.outgoing.is_closed());
    }

    #[tokio::test]
    async fn test_udp_empty_forward_list_rejected() {
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        match spawn_udp_interface(InterfaceId(0), "udp_empty".into(), listen, Vec::new()) {
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidInput),
            Ok(_) => panic!("empty forward address list must be rejected"),
        }
    }

    #[test]
    fn test_parse_forward_addrs_single_plain_ip() {
        // The Python/rnsd-compatible form: one plain IP plus forward_port
        let addrs = parse_forward_addrs("192.168.1.255", Some(4242)).unwrap();
        assert_eq!(
            addrs,
            vec!["192.168.1.255:4242".parse::<SocketAddr>().unwrap().into()]
        );
    }

    #[test]
    fn test_parse_forward_addrs_comma_separated_plain_ips() {
        let addrs = parse_forward_addrs("10.0.0.255, 10.1.0.255", Some(4242)).unwrap();
        assert_eq!(
            addrs,
            vec![
                "10.0.0.255:4242".parse::<SocketAddr>().unwrap().into(),
                "10.1.0.255:4242".parse::<SocketAddr>().unwrap().into(),
            ]
        );
    }

    #[test]
    fn test_parse_forward_addrs_entry_with_own_port() {
        let addrs = parse_forward_addrs("10.0.0.255:5000,10.1.0.255", Some(4242)).unwrap();
        assert_eq!(
            addrs,
            vec![
                "10.0.0.255:5000".parse::<SocketAddr>().unwrap().into(),
                "10.1.0.255:4242".parse::<SocketAddr>().unwrap().into(),
            ]
        );
    }

    #[test]
    fn test_parse_forward_addrs_all_own_ports_without_forward_port() {
        let addrs = parse_forward_addrs("10.0.0.1:5000,[::1]:6000", None).unwrap();
        assert_eq!(
            addrs,
            vec![
                "10.0.0.1:5000".parse::<SocketAddr>().unwrap().into(),
                "[::1]:6000".parse::<SocketAddr>().unwrap().into()
            ]
        );
    }

    #[test]
    fn test_parse_forward_addrs_plain_ip_requires_forward_port() {
        let err = parse_forward_addrs("10.0.0.255", None).unwrap_err();
        assert_eq!(err, ForwardAddrError::MissingPort);
    }

    #[test]
    fn test_parse_forward_addrs_hostname_with_forward_port() {
        // Codeberg #148: rnsd accepts a hostname in forward_ip (Python passes
        // it straight to sendto, where the OS resolves it); lnsd must too.
        let targets = parse_forward_addrs("peer.example.com", Some(4242)).unwrap();
        assert_eq!(
            targets,
            vec![ForwardTarget::Named {
                host: "peer.example.com".to_string(),
                port: 4242,
            }]
        );
    }

    #[test]
    fn test_parse_forward_addrs_hostname_with_own_port() {
        let targets = parse_forward_addrs("peer.example.com:5000", None).unwrap();
        assert_eq!(
            targets,
            vec![ForwardTarget::Named {
                host: "peer.example.com".to_string(),
                port: 5000,
            }]
        );
    }

    #[test]
    fn test_parse_forward_addrs_plain_ipv6_with_forward_port() {
        // A bare IPv6 address combined with forward_port. The old
        // format!("{}:{}") + SocketAddr::parse path produced "::1:4242",
        // which is not a valid socket address.
        let targets = parse_forward_addrs("::1", Some(4242)).unwrap();
        assert_eq!(
            targets,
            vec!["[::1]:4242".parse::<SocketAddr>().unwrap().into()]
        );
    }

    #[test]
    fn test_parse_forward_addrs_mixed_hostname_and_ip() {
        let targets = parse_forward_addrs("10.0.0.1:5000, peer.example.com", Some(4242)).unwrap();
        assert_eq!(
            targets,
            vec![
                "10.0.0.1:5000".parse::<SocketAddr>().unwrap().into(),
                ForwardTarget::Named {
                    host: "peer.example.com".to_string(),
                    port: 4242,
                },
            ]
        );
    }

    #[test]
    fn test_parse_forward_addrs_hostname_requires_port() {
        let err = parse_forward_addrs("peer.example.com", None).unwrap_err();
        assert_eq!(err, ForwardAddrError::MissingPort);
    }

    #[test]
    fn test_parse_forward_addrs_invalid_entry() {
        let err = parse_forward_addrs("10.0.0.255,not an address", Some(4242)).unwrap_err();
        match err {
            ForwardAddrError::Invalid(msg) => assert!(msg.contains("not an address")),
            other => panic!("expected Invalid, got {:?}", other),
        }
    }

    /// A resolver that always fails must not kill the interface: sends are
    /// dropped with a warning, the receive path keeps working, and the
    /// outgoing channel stays open — Python's process_outgoing semantics.
    #[tokio::test]
    async fn test_udp_named_forward_resolution_failure_keeps_interface_alive() {
        let failing: Resolver = Arc::new(|host, _port| {
            Box::pin(async move {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no such host: {}", host),
                ))
            })
        });

        let std_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let bound = std_sock.local_addr().unwrap();
        let mut handle = spawn_udp_interface_with_opts(
            InterfaceId(0),
            "udp_unresolvable".into(),
            std_sock,
            vec![ForwardTarget::Named {
                host: "peer.invalid".to_string(),
                port: 4242,
            }],
            UdpResolveOpts {
                resolver: failing,
                refresh_interval: Duration::from_millis(50),
                retry_interval: Duration::from_millis(10),
            },
        )
        .unwrap();

        // Send while unresolved: dropped at the interface, no crash.
        handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: b"into the void".to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.outgoing.is_closed());

        // Receive path must still work.
        let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"still alive", bound).await.unwrap();
        let pkt = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv())
            .await
            .expect("timeout — interface must still receive with failing resolver")
            .expect("channel closed");
        assert_eq!(pkt.data, b"still alive");
    }

    /// A changed hostname→address mapping is picked up: after the refresh
    /// interval, sends follow the resolver to the peer's new address
    /// (container restart / DHCP case from Codeberg #148).
    #[tokio::test]
    async fn test_udp_named_forward_reresolution_picks_up_address_change() {
        let listener_old = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listener_new = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr_old = listener_old.local_addr().unwrap();
        let addr_new = listener_new.local_addr().unwrap();

        let mapping = Arc::new(std::sync::Mutex::new(addr_old));
        let mapping_for_resolver = Arc::clone(&mapping);
        let switching: Resolver = Arc::new(move |_host, _port| {
            let addr = *mapping_for_resolver.lock().unwrap();
            Box::pin(async move { Ok(vec![addr]) })
        });

        let std_sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let handle = spawn_udp_interface_with_opts(
            InterfaceId(0),
            "udp_moving_peer".into(),
            std_sock,
            vec![ForwardTarget::Named {
                host: "peer.test".to_string(),
                port: 4242,
            }],
            UdpResolveOpts {
                resolver: switching,
                refresh_interval: Duration::from_millis(100),
                retry_interval: Duration::from_millis(10),
            },
        )
        .unwrap();

        // Phase 1: sends reach the old address.
        let mut buf = [0u8; UDP_MTU];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut reached_old = false;
        while tokio::time::Instant::now() < deadline {
            handle
                .outgoing
                .send(OutgoingPacket {
                    peer: None,
                    data: b"to old".to_vec(),
                    high_priority: false,
                })
                .await
                .unwrap();
            if let Ok(recv) =
                tokio::time::timeout(Duration::from_millis(100), listener_old.recv_from(&mut buf))
                    .await
            {
                recv.expect("recv at old peer address");
                reached_old = true;
                break;
            }
        }
        assert!(
            reached_old,
            "sends must reach the initially resolved address"
        );

        // The peer moves: same name, new address.
        *mapping.lock().unwrap() = addr_new;

        // Phase 2: keep sending; after the refresh interval the interface
        // must follow the name to the new address.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut reached_new = false;
        while tokio::time::Instant::now() < deadline {
            handle
                .outgoing
                .send(OutgoingPacket {
                    peer: None,
                    data: b"to new".to_vec(),
                    high_priority: false,
                })
                .await
                .unwrap();
            if let Ok(recv) =
                tokio::time::timeout(Duration::from_millis(100), listener_new.recv_from(&mut buf))
                    .await
            {
                recv.expect("recv at new peer address");
                reached_new = true;
                break;
            }
        }
        assert!(
            reached_new,
            "a changed hostname mapping must be picked up after the refresh interval"
        );
    }

    #[tokio::test]
    async fn test_udp_dropping_handle_stops_task() {
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let forward: SocketAddr = "127.0.0.1:9999".parse().unwrap();

        let handle = spawn_udp_interface(
            InterfaceId(0),
            "udp_drop".into(),
            listen,
            vec![forward.into()],
        )
        .unwrap();

        // Drop the handle (both incoming and outgoing channels)
        drop(handle);

        // The I/O task should exit soon (outgoing channel closed)
        tokio::time::sleep(Duration::from_millis(100)).await;
        // No assertion needed, if the task doesn't exit, it would leak,
        // but tokio cleans up on runtime drop. This test verifies no panic.
    }
}
