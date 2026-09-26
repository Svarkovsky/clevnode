//! Local (Unix socket) interface for shared instance IPC
//!
//! Implements the data channel for Python Reticulum's "shared instance" feature.
//! A daemon listens on an abstract Unix domain socket (`\0rns/{instance_name}`)
//! and accepts connections from local client programs. Each connection becomes
//! an `InterfaceHandle` with `is_local_client = true`, which tells core to
//! forward announces and path requests to/from this client.
//!
//! Uses the same HDLC framing as TCP interfaces.

use std::io;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use leviculum_core::constants::MTU;
use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};
use leviculum_core::transport::InterfaceId;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use super::inventory::{self as inventory_names, InterfaceIdentity, ListenerRow, SharedInventory};
use super::{IncomingPacket, InterfaceCounters, InterfaceHandle, InterfaceInfo, OutgoingPacket};
use crate::sync_ext::MutexRecover;

// Platform IPC transport. Unix domain sockets on Unix; TCP loopback on Windows,
// matching Python-RNS, which falls back to 127.0.0.1 (AF_INET) when AF_UNIX is
// unavailable (default local_interface_port 37428 / local_control_port 37429).
// `UnixStream`/`TcpStream` are symmetric (both halves impl AsyncRead/AsyncWrite
// and `into_split()`), so the I/O code below is unchanged across platforms.
//
// Platform support: Linux (abstract Unix sockets) is the tested path, exercised
// by our CI. The macOS/BSD filesystem-socket and Windows TCP-loopback fallbacks
// below are community-supported and are not exercised by our CI.
#[cfg(windows)]
use tokio::net::TcpListener as LocalListener;
#[cfg(windows)]
use tokio::net::TcpStream as LocalStream;
#[cfg(unix)]
use tokio::net::UnixListener as LocalListener;
#[cfg(unix)]
use tokio::net::UnixStream as LocalStream;

/// Bind a local listener for the given abstract instance name.
///
/// On Linux, uses abstract Unix sockets (`\0name`); on other Unix systems,
/// filesystem sockets in the temp directory.
#[cfg(unix)]
fn bind_local_listener(abstract_name: &str) -> Result<std::os::unix::net::UnixListener, io::Error> {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        std::os::unix::net::UnixListener::bind_addr(&addr)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path =
            std::env::temp_dir().join(format!("leviculum-{}", abstract_name.replace('/', "-")));
        // Remove stale socket file if it exists
        let _ = std::fs::remove_file(&path);
        std::os::unix::net::UnixListener::bind(&path)
    }
}

/// Windows: bind a TCP loopback listener, matching Python-RNS's AF_INET fallback.
#[cfg(windows)]
fn bind_local_listener(abstract_name: &str) -> Result<std::net::TcpListener, io::Error> {
    std::net::TcpListener::bind(loopback_addr(abstract_name))
}

/// Connect to a local shared instance by abstract name.
#[cfg(unix)]
fn connect_local(abstract_name: &str) -> Result<std::os::unix::net::UnixStream, io::Error> {
    #[cfg(target_os = "linux")]
    {
        use std::os::linux::net::SocketAddrExt;
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        std::os::unix::net::UnixStream::connect_addr(&addr)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let path =
            std::env::temp_dir().join(format!("leviculum-{}", abstract_name.replace('/', "-")));
        std::os::unix::net::UnixStream::connect(&path)
    }
}

/// Windows: connect to the TCP loopback shared instance.
#[cfg(windows)]
fn connect_local(abstract_name: &str) -> Result<std::net::TcpStream, io::Error> {
    std::net::TcpStream::connect(loopback_addr(abstract_name))
}

/// Configured TCP-loopback ports for the shared-instance data (`.0`) and RPC
/// (`.1`) channels (`shared_instance_port` / `instance_control_port`). `0`
/// means unset, so `loopback_addr` falls back to the Python defaults. Set once
/// at process startup from the parsed config (see `set_loopback_ports`).
///
/// Only the AF_INET (Windows / `shared_instance_type = tcp`) path reads these;
/// the AF_UNIX path keys the socket by `instance_name` and ignores the ports,
/// matching Python. The store is unconditional so the config value is captured
/// on every platform; only the Windows reader consults it.
static CONFIGURED_LOOPBACK_PORTS: (AtomicU32, AtomicU32) = (AtomicU32::new(0), AtomicU32::new(0));

/// Record the configured shared-instance TCP-loopback ports for later binds.
///
/// Called from the node builder with the parsed `shared_instance_port` /
/// `instance_control_port`. `None` leaves the Python default in force. A no-op
/// on the AF_UNIX path (the ports are never read there).
pub(crate) fn set_loopback_ports(
    shared_instance_port: Option<u16>,
    instance_control_port: Option<u16>,
) {
    CONFIGURED_LOOPBACK_PORTS.0.store(
        u32::from(shared_instance_port.unwrap_or(0)),
        Ordering::Relaxed,
    );
    CONFIGURED_LOOPBACK_PORTS.1.store(
        u32::from(instance_control_port.unwrap_or(0)),
        Ordering::Relaxed,
    );
}

/// Resolve the TCP-loopback port for an abstract shared-instance name.
///
/// A configured override (`shared_instance_port` for the data channel,
/// `instance_control_port` for the `/rpc` channel) wins. Otherwise the Python
/// defaults hold — 37428 (`local_interface_port`) for the data channel, 37429
/// (`local_control_port`) for RPC — and a non-default instance name derives a
/// stable FNV-1a port so independent *leviculum* peers agree without config
/// (a leviculum-local convention that does not match Python's port for the same
/// name). Kept platform-agnostic so the resolution is unit-testable off Windows.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub(crate) fn resolve_loopback_port(
    abstract_name: &str,
    configured_data: u16,
    configured_control: u16,
) -> u16 {
    let is_rpc = abstract_name.ends_with("/rpc");
    let configured = if is_rpc {
        configured_control
    } else {
        configured_data
    };
    if configured != 0 {
        return configured;
    }
    match abstract_name {
        "rns/default" => 37428,
        "rns/default/rpc" => 37429,
        other => name_to_port(other),
    }
}

/// Map an abstract instance name to a TCP loopback address (Windows).
///
/// Python-RNS, when AF_UNIX is unavailable, binds fixed ports — 37428
/// (`local_interface_port`) for the shared instance and 37429
/// (`local_control_port`) for RPC — and, critically, does **not** derive a
/// port from `instance_name` on the AF_INET path (instance_name only varies
/// the AF_UNIX socket name; see Reticulum.py). A Windows `rnsd` runs multiple
/// instances by setting `shared_instance_port`/`instance_control_port`
/// explicitly, not by hashing the name.
///
/// A configured `shared_instance_port` / `instance_control_port` (captured by
/// `set_loopback_ports`) overrides the default; otherwise the default-instance
/// path matches 37428/37429 and interops cleanly with a Windows `rnsd`, and a
/// non-default instance name derives a stable FNV-1a port (see
/// `resolve_loopback_port`).
#[cfg(windows)]
pub(crate) fn loopback_addr(abstract_name: &str) -> std::net::SocketAddr {
    use std::net::{Ipv4Addr, SocketAddr};
    let port = resolve_loopback_port(
        abstract_name,
        CONFIGURED_LOOPBACK_PORTS.0.load(Ordering::Relaxed) as u16,
        CONFIGURED_LOOPBACK_PORTS.1.load(Ordering::Relaxed) as u16,
    );
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// Stable FNV-1a hash of a name into the unprivileged 37430..=65534 range.
///
/// Only reached via the Windows loopback path (or its unit tests); allowed to
/// be dead on the non-test AF_UNIX build.
#[cfg_attr(not(any(test, windows)), allow(dead_code))]
pub(crate) fn name_to_port(name: &str) -> u16 {
    let mut h: u32 = 0x811c_9dc5;
    for b in name.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    37430 + (h % (65535 - 37430)) as u16
}

/// Default channel buffer size for local interfaces.
///
/// Sized to absorb announce-burst fan-out from transit peers: a single
/// transit-active node has been observed emitting ~500 directed
/// SendPackets per Local-Client in a single event-loop tick. 4096 gives
/// 16× headroom on the original 256-cap and ~8× on the worst-case burst.
pub(crate) const LOCAL_DEFAULT_BUFFER_SIZE: usize = 4096;

/// Hardware MTU for local interfaces (same as TCP, local IPC).
const LOCAL_HW_MTU: u32 = 262_144;

/// Bitrate a shared-instance interface reports: 1 Gbps, as the reference sets
/// on both the server and every accepted client (LocalInterface.py:431).
pub(crate) const LOCAL_BITRATE: i64 = 1_000_000_000;

/// Frame buffer multiplier (accounts for HDLC escaping overhead)
const FRAME_BUFFER_MULTIPLIER: usize = 2;

/// Read buffer multiplier (handles multiple packets per read)
const READ_BUFFER_MULTIPLIER: usize = 4;

/// Start a local (Unix socket) server for shared instance IPC.
///
/// Binds to an abstract Unix socket at `\0rns/{instance_name}` and spawns an
/// async accept loop. Each accepted connection becomes an `InterfaceHandle`
/// sent to the event loop via `new_interface_tx`.
///
/// The shared-instance server itself carries no packets, so it never becomes
/// a routable interface; like a TCP listener it is announced to the reporting
/// inventory (Codeberg #177), and every accepted IPC client registers its
/// reference display identity there.
///
/// The accept loop exits when the event loop drops `new_interface_rx`
/// (detected via `Sender::closed()`).
pub(crate) fn spawn_local_server(
    instance_name: &str,
    next_id: Arc<AtomicUsize>,
    new_interface_tx: mpsc::Sender<InterfaceHandle>,
    buffer_size: usize,
    server_id: usize,
    inventory: SharedInventory,
    live_clients: Arc<AtomicUsize>,
) -> Result<(), io::Error> {
    // Build abstract socket name: "rns/{instance_name}"
    let abstract_name = format!("rns/{}", instance_name);

    let std_listener = bind_local_listener(&abstract_name)?;
    std_listener.set_nonblocking(true)?;
    let listener = LocalListener::from_std(std_listener)?;

    tracing::info!("Local server listening on socket {}", abstract_name);

    inventory.lock_recover().add_listener(
        server_id,
        ListenerRow {
            identity: InterfaceIdentity {
                name: inventory_names::shared_instance_name(&abstract_name),
                // Python names the shared-instance server "Reticulum"
                // (LocalInterface.py:391).
                short_name: "Reticulum".to_string(),
                type_name: "LocalServerInterface",
                parent: None,
            },
            bitrate: LOCAL_BITRATE,
            hw_mtu: LOCAL_HW_MTU as i64,
            mode: leviculum_core::traits::InterfaceMode::default(),
            // The reference pins all three to None on this interface
            // (LocalInterface.py:427-429), unlike a config interface.
            announce_rate: (None, None, None),
            ifac_size_bits: None,
            departed_rxb: 0,
            departed_txb: 0,
            bound_addr: None,
        },
    );

    let instance_name_owned = abstract_name.clone();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, _peer_addr)) => {
                            let id = InterfaceId(next_id.fetch_add(1, Ordering::Relaxed));
                            // Python labels an accepted client with the LIVE
                            // client count at accept time (LocalInterface.py:441
                            // with the matching `clients -= 1` on teardown,
                            // LocalInterface.py:355), so the counter has to be
                            // the live one, not a monotonic connection index.
                            let client_num = live_clients.fetch_add(1, Ordering::Relaxed);
                            let name = format!("Local[{}]/{}", instance_name_owned, client_num);
                            inventory.lock_recover().add_spawned(
                                id.0,
                                InterfaceIdentity {
                                    name: inventory_names::local_client_name(&instance_name_owned),
                                    short_name: inventory_names::local_client_short_name(
                                        client_num,
                                        &instance_name_owned,
                                    ),
                                    type_name: "LocalClientInterface",
                                    parent: Some(server_id),
                                },
                            );
                            let handle = spawn_local_interface_from_stream(
                                id, name.clone(), stream, buffer_size,
                            );
                            tracing::info!("Local client connected: {} ({})", name, id);
                            if new_interface_tx.send(handle).await.is_err() {
                                break; // event loop shut down
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Local accept error: {}", e);
                        }
                    }
                }
                _ = new_interface_tx.closed() => {
                    tracing::debug!("Local server shutting down (event loop exited)");
                    break;
                }
            }
        }
    });

    Ok(())
}

/// Create channels, spawn the I/O task for an accepted Unix stream,
/// and return the resulting `InterfaceHandle`.
fn spawn_local_interface_from_stream(
    id: InterfaceId,
    name: String,
    stream: LocalStream,
    buffer_size: usize,
) -> InterfaceHandle {
    let (incoming_tx, incoming_rx) = mpsc::channel(buffer_size);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(buffer_size);
    let counters = Arc::new(InterfaceCounters::new());

    let task_name = name.clone();
    let task_counters = Arc::clone(&counters);

    tokio::spawn(async move {
        local_interface_task(task_name, stream, incoming_tx, outgoing_rx, task_counters).await;
    });

    InterfaceHandle {
        info: InterfaceInfo {
            id,
            name,
            hw_mtu: Some(LOCAL_HW_MTU),
            is_local_client: true,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::Local,
            // A shared-instance IPC client is never ingress-limited, in the
            // reference by construction: `LocalClientInterface` overrides
            // `should_ingress_limit()` to return False unconditionally
            // (LocalInterface.py:137-138), regardless of the flat
            // `ingress_control = True` it inherits from `Interface.__init__`.
            // Stated here rather than left to a fallback so the local server
            // says what it means (Codeberg #189).
            ingress_control: Some(false),
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        // The IPC stream already exists when this function
        // is called (server-accepted), so the interface is ready
        // immediately.
        ready: super::ReadySignal::ready_immediate(),
    }
}

/// Name the socket a failed shared-instance connect was aiming at, and, when
/// nothing was listening there, what to do about it.
///
/// The raw error is `Connection refused (os error 111)` with no hint of what
/// was being connected to, which every client of the shared instance
/// (`lblogd`, `lnomad`, `lncp`, `lnstatus`) then surfaces verbatim. Naming
/// the socket also makes an `instance_name` mismatch visible: the socket in
/// the message is the one the client wanted, so it can be compared against
/// what the daemon actually listens on. The error kind is preserved so
/// callers matching on it keep working.
fn absent_daemon_error(source: io::Error, abstract_name: &str) -> io::Error {
    // NotFound is the same condition on the non-Linux filesystem-socket path.
    let nobody_listening = matches!(
        source.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    );
    let message = if nobody_listening {
        format!(
            "no local Leviculum daemon reachable over IPC socket \"{abstract_name}\": \
             is lnsd or rnsd running?"
        )
    } else {
        format!("IPC socket \"{abstract_name}\": {source}")
    };
    io::Error::new(source.kind(), message)
}

/// Connect to an existing shared instance daemon as a client.
///
/// Connects to the abstract Unix socket `\0rns/{instance_name}` and returns
/// an `InterfaceHandle`. The handle has `is_local_client = false` because
/// from the client's perspective this is a regular interface; the daemon
/// marks its side as `is_local_client = true`.
///
/// Calls `tokio::spawn` for the I/O task, must be called from a context
/// where a tokio runtime is active (same as `spawn_local_server`).
///
/// No reconnection, returns an error if the daemon is not running.
pub(crate) fn spawn_local_client(
    id: InterfaceId,
    instance_name: &str,
    buffer_size: usize,
) -> Result<InterfaceHandle, io::Error> {
    let abstract_name = format!("rns/{}", instance_name);

    let std_stream =
        connect_local(&abstract_name).map_err(|e| absent_daemon_error(e, &abstract_name))?;
    std_stream.set_nonblocking(true)?;
    let stream = LocalStream::from_std(std_stream)?;

    let (incoming_tx, incoming_rx) = mpsc::channel(buffer_size);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(buffer_size);
    let counters = Arc::new(InterfaceCounters::new());

    let name = format!("LocalClient[{}]", instance_name);
    let task_name = name.clone();
    let task_counters = Arc::clone(&counters);

    tokio::spawn(async move {
        local_interface_task(task_name, stream, incoming_tx, outgoing_rx, task_counters).await;
    });

    Ok(InterfaceHandle {
        info: InterfaceInfo {
            id,
            name,
            hw_mtu: Some(LOCAL_HW_MTU),
            is_local_client: false,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::Local,
            ingress_control: None,
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        // Local IPC client connect already succeeded
        // (`connect_local` above is synchronous), so the
        // interface is ready immediately.
        ready: super::ReadySignal::ready_immediate(),
    })
}

/// I/O task owning the IPC stream.
///
/// Handles bidirectional I/O using HDLC framing, identical to the TCP
/// interface task. Uses poll_read_ready + try_read for edge-triggered reads.
async fn local_interface_task(
    name: String,
    stream: LocalStream,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
) {
    let (reader, mut writer) = stream.into_split();

    let mut deframer = Deframer::with_max_frame(LOCAL_HW_MTU as usize);
    let mut read_buf = vec![0u8; MTU * READ_BUFFER_MULTIPLIER];
    let mut frame_buf = Vec::with_capacity(MTU * FRAME_BUFFER_MULTIPLIER);

    loop {
        tokio::select! {
            // Read path: wait for socket readability, then try_read + deframe
            result = reader.readable() => {
                match result {
                    Ok(()) => {
                        loop {
                            match reader.try_read(&mut read_buf) {
                                Ok(0) => {
                                    tracing::debug!("Local interface {} disconnected (EOF)", name);
                                    return;
                                }
                                Ok(n) => {
                                    counters.rx_bytes.fetch_add(n as usize, Ordering::Relaxed);
                                    let results = deframer.process(&read_buf[..n]);
                                    for r in results {
                                        // HW_MTU enforcement lives in the deframer now.
                                        if matches!(r, DeframeResult::Oversized) {
                                            tracing::trace!(
                                                "Local {}: frame exceeds HW_MTU, discarded", name);
                                            continue;
                                        }
                                        if let DeframeResult::Frame(data) = r {
                                            if incoming_tx.send(IncomingPacket { data }).await.is_err() {
                                                return;
                                            }
                                        }
                                    }
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                    break; // no more data, back to select!
                                }
                                Err(e) => {
                                    tracing::debug!("Local interface {} read error: {}", name, e);
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Local interface {} readability error: {}", name, e);
                        return;
                    }
                }
            }

            // Write path: receive outgoing packets and write HDLC-framed to stream
            msg = outgoing_rx.recv() => {
                match msg {
                    Some(pkt) => {
                        frame(&pkt.data, &mut frame_buf);
                        if let Err(e) = writer.write_all(&frame_buf).await {
                            tracing::debug!("Local interface {} write error: {}", name, e);
                            return;
                        }
                        counters.tx_bytes.fetch_add(frame_buf.len() as usize, Ordering::Relaxed);
                    }
                    None => {
                        tracing::debug!("Local interface {} outgoing channel closed", name);
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {

    /// `spawn_local_server` with a private reporting inventory and client
    /// counter, for tests that only exercise the socket itself.
    fn spawn_test_local_server(
        instance_name: &str,
        next_id: Arc<AtomicUsize>,
        tx: mpsc::Sender<InterfaceHandle>,
        buffer_size: usize,
    ) -> Result<(), io::Error> {
        spawn_local_server(
            instance_name,
            next_id,
            tx,
            buffer_size,
            0,
            crate::interfaces::inventory::InterfaceInventory::shared(),
            Arc::new(AtomicUsize::new(0)),
        )
    }
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Connect to a local server in tests, using the platform-appropriate socket.
    fn test_connect(instance_name: &str) -> std::os::unix::net::UnixStream {
        let abstract_name = format!("rns/{}", instance_name);
        connect_local(&abstract_name).unwrap()
    }

    /// The bare `Connection refused` a failed connect produces says nothing
    /// about what was being connected to, which turns a missing or
    /// misnamed daemon into a mystery for every client of the shared
    /// instance. The message has to name the socket and the fix.
    #[tokio::test]
    async fn absent_daemon_error_names_the_socket_and_the_daemons() {
        let instance_name = format!("no_such_instance_{}", std::process::id());
        // InterfaceHandle is not Debug, so unwrap the Result by hand.
        let Err(err) = spawn_local_client(InterfaceId(1), &instance_name, 16) else {
            panic!("no daemon listens on this name, connecting must fail");
        };

        let message = err.to_string();
        assert!(
            message.contains(&format!("rns/{instance_name}")),
            "must name the socket, so a wrong instance_name is visible: {message}"
        );
        assert!(
            message.contains("lnsd") && message.contains("rnsd"),
            "must say which daemon to start: {message}"
        );
        // The underlying connect error differs by platform — Linux's abstract
        // namespace refuses an unbound name (ConnectionRefused), while the
        // filesystem-socket fallback on other Unixes fails to find the path
        // (NotFound). The invariant is that whichever kind the platform
        // produced survives the rewrap, not that every platform is Linux.
        #[cfg(target_os = "linux")]
        let expected_kind = io::ErrorKind::ConnectionRefused;
        #[cfg(not(target_os = "linux"))]
        let expected_kind = io::ErrorKind::NotFound;
        assert_eq!(
            err.kind(),
            expected_kind,
            "the error kind must survive the rewrap"
        );
    }

    #[tokio::test]
    async fn test_local_server_accepts_connection() {
        let next_id = Arc::new(AtomicUsize::new(100));
        let (tx, mut rx) = mpsc::channel::<InterfaceHandle>(4);

        // Use a unique instance name to avoid conflicts
        let instance_name = format!("test_{}", std::process::id());
        spawn_test_local_server(&instance_name, next_id.clone(), tx, 16).unwrap();

        // Connect as a local client
        let std_stream = test_connect(&instance_name);
        std_stream.set_nonblocking(true).unwrap();
        let _client = tokio::net::UnixStream::from_std(std_stream).unwrap();

        // Verify an InterfaceHandle arrives on the channel
        let handle = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for handle")
            .expect("channel closed");

        assert!(handle.info.name.starts_with("Local["));
        assert_eq!(handle.info.id, InterfaceId(100));
        assert!(handle.info.is_local_client);
        assert!(!handle.outgoing.is_closed());
    }

    #[tokio::test]
    async fn test_local_interface_hdlc_round_trip() {
        let next_id = Arc::new(AtomicUsize::new(200));
        let (tx, mut rx) = mpsc::channel::<InterfaceHandle>(4);

        let instance_name = format!("test_rt_{}", std::process::id());
        spawn_test_local_server(&instance_name, next_id.clone(), tx, 16).unwrap();

        // Connect
        let std_stream = test_connect(&instance_name);
        std_stream.set_nonblocking(true).unwrap();
        let mut client = tokio::net::UnixStream::from_std(std_stream).unwrap();

        let mut handle = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");

        // Client sends HDLC-framed packet to server
        let payload = b"hello-local";
        let mut frame_buf = Vec::new();
        leviculum_core::framing::hdlc::frame(payload, &mut frame_buf);
        client.write_all(&frame_buf).await.unwrap();

        // Verify packet arrives on incoming channel
        let pkt = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv())
            .await
            .expect("timeout waiting for packet")
            .expect("channel closed");
        assert_eq!(pkt.data, payload);

        // Server sends HDLC-framed packet to client
        let response = b"reply-local";
        handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: response.to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        // Read HDLC-framed response on client side
        let mut recv_buf = vec![0u8; 1024];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut recv_buf))
            .await
            .expect("timeout reading response")
            .unwrap();
        assert!(n > 0);

        // Deframe and verify
        let mut deframer = Deframer::new();
        let results = deframer.process(&recv_buf[..n]);
        let mut frames = Vec::new();
        for r in results {
            if let DeframeResult::Frame(data) = r {
                frames.push(data);
            }
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], response);
    }

    #[tokio::test]
    async fn test_local_client_disconnect_detected() {
        let next_id = Arc::new(AtomicUsize::new(300));
        let (tx, mut rx) = mpsc::channel::<InterfaceHandle>(4);

        let instance_name = format!("test_disc_{}", std::process::id());
        spawn_test_local_server(&instance_name, next_id.clone(), tx, 16).unwrap();

        // Connect and immediately drop
        let std_stream = test_connect(&instance_name);
        std_stream.set_nonblocking(true).unwrap();
        let client = tokio::net::UnixStream::from_std(std_stream).unwrap();

        let mut handle = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");

        // Drop the client connection
        drop(client);

        // incoming channel should close (recv returns None)
        let result = tokio::time::timeout(Duration::from_secs(2), handle.incoming.recv()).await;
        match result {
            Ok(None) => {} // expected: channel closed on disconnect
            Ok(Some(_)) => panic!("should not receive a packet after disconnect"),
            Err(_) => panic!("timeout — disconnect was not detected"),
        }
    }

    #[tokio::test]
    async fn test_local_server_multiple_clients() {
        let next_id = Arc::new(AtomicUsize::new(400));
        let (tx, mut rx) = mpsc::channel::<InterfaceHandle>(4);

        let instance_name = format!("test_multi_{}", std::process::id());
        spawn_test_local_server(&instance_name, next_id.clone(), tx, 16).unwrap();

        // Connect two clients
        let std1 = test_connect(&instance_name);
        std1.set_nonblocking(true).unwrap();
        let _client1 = tokio::net::UnixStream::from_std(std1).unwrap();

        let std2 = test_connect(&instance_name);
        std2.set_nonblocking(true).unwrap();
        let _client2 = tokio::net::UnixStream::from_std(std2).unwrap();

        // Both should produce handles
        let h1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");
        let h2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout")
            .expect("closed");

        assert_ne!(h1.info.id, h2.info.id);
        assert!(h1.info.is_local_client);
        assert!(h2.info.is_local_client);
    }

    #[tokio::test]
    async fn test_local_client_connects_and_communicates() {
        let next_id = Arc::new(AtomicUsize::new(500));
        let (tx, mut rx) = mpsc::channel::<InterfaceHandle>(4);

        let instance_name = format!("test_client_{}", std::process::id());
        spawn_test_local_server(&instance_name, next_id.clone(), tx, 16).unwrap();

        // Give server time to bind
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Connect via spawn_local_client
        let id = InterfaceId(42);
        let mut client_handle =
            spawn_local_client(id, &instance_name, 16).expect("client connect failed");

        // Verify client handle properties
        assert_eq!(client_handle.info.id, InterfaceId(42));
        assert!(!client_handle.info.is_local_client);
        assert!(client_handle.info.name.contains("LocalClient"));

        // Server should have received a new handle with is_local_client = true
        let mut server_handle = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout waiting for server handle")
            .expect("channel closed");
        assert!(server_handle.info.is_local_client);

        // Client → Server: send HDLC-framed data through client handle's outgoing
        client_handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: b"client-to-server".to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        let pkt = tokio::time::timeout(Duration::from_secs(2), server_handle.incoming.recv())
            .await
            .expect("timeout waiting for server packet")
            .expect("channel closed");
        assert_eq!(pkt.data, b"client-to-server");

        // Server → Client: send data through server handle's outgoing
        server_handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: b"server-to-client".to_vec(),
                high_priority: false,
            })
            .await
            .unwrap();

        let pkt = tokio::time::timeout(Duration::from_secs(2), client_handle.incoming.recv())
            .await
            .expect("timeout waiting for client packet")
            .expect("channel closed");
        assert_eq!(pkt.data, b"server-to-client");
    }

    #[test]
    fn test_resolve_loopback_port_defaults_and_overrides() {
        // Codeberg #112: the AF_INET bind port resolution. Unset (0) keeps the
        // Python defaults; a configured port wins for the matching channel.
        assert_eq!(resolve_loopback_port("rns/default", 0, 0), 37428);
        assert_eq!(resolve_loopback_port("rns/default/rpc", 0, 0), 37429);

        // Configured data port applies to the data channel, control to /rpc.
        assert_eq!(resolve_loopback_port("rns/default", 37500, 37501), 37500);
        assert_eq!(
            resolve_loopback_port("rns/default/rpc", 37500, 37501),
            37501
        );

        // The data override does not leak onto the RPC channel and vice versa.
        assert_eq!(resolve_loopback_port("rns/default/rpc", 37500, 0), 37429);
        assert_eq!(resolve_loopback_port("rns/default", 0, 37501), 37428);

        // A non-default instance name without an override derives a stable port.
        let a = resolve_loopback_port("rns/alpha", 0, 0);
        let b = resolve_loopback_port("rns/beta", 0, 0);
        assert_ne!(a, b);
        assert_eq!(a, resolve_loopback_port("rns/alpha", 0, 0), "stable");
        assert!((37430..=65534).contains(&a));
    }

    #[tokio::test]
    async fn test_two_instances_different_names_no_collision() {
        // Codeberg #112, functional: two shared-instance servers on one host
        // start without an AddrInUse collision when they use different instance
        // names. On Linux the shared instance is an abstract AF_UNIX socket
        // keyed by `instance_name` (`\0rns/{instance_name}`), so the instance
        // name -- not `shared_instance_port` -- is what separates two daemons,
        // matching Python's AF_UNIX behaviour (the port is only bound on the
        // AF_INET / `shared_instance_type = tcp` path).
        let next_id = Arc::new(AtomicUsize::new(600));
        let (tx1, _rx1) = mpsc::channel::<InterfaceHandle>(4);
        let (tx2, _rx2) = mpsc::channel::<InterfaceHandle>(4);

        let base = std::process::id();
        let name_a = format!("test_collide_a_{base}");
        let name_b = format!("test_collide_b_{base}");

        spawn_test_local_server(&name_a, next_id.clone(), tx1, 16).expect("first instance binds");
        // A second instance under a different name must not hit AddrInUse.
        spawn_test_local_server(&name_b, next_id.clone(), tx2, 16)
            .expect("second instance under a different name must bind");

        // Sanity: reusing the first name does collide (proves the bind is real).
        //
        // This holds where the shared-instance socket has kernel-enforced name
        // uniqueness: Linux's abstract AF_UNIX namespace (`\0rns/{name}`) and
        // the Windows TCP-loopback fallback both reject a duplicate bind. macOS
        // has no abstract namespace and falls back to a *filesystem* AF_UNIX
        // path, where a stale socket file is unlinked and re-bound instead of
        // colliding — so two same-named daemons can both bind. That is a real
        // shared-instance robustness gap on macOS (tracked for upstream), not a
        // property this test can assert there.
        #[cfg(not(target_os = "macos"))]
        {
            let (tx3, _rx3) = mpsc::channel::<InterfaceHandle>(4);
            let dup = spawn_test_local_server(&name_a, next_id, tx3, 16);
            assert!(
                dup.is_err(),
                "re-binding the same instance name must fail with AddrInUse"
            );
        }
        #[cfg(target_os = "macos")]
        let _ = next_id; // silence unused on the macOS path
    }

    #[tokio::test]
    async fn test_local_client_connect_failure() {
        let result = spawn_local_client(
            InterfaceId(99),
            "nonexistent_instance_that_does_not_exist",
            16,
        );
        assert!(
            result.is_err(),
            "connecting to nonexistent socket should fail"
        );
    }
}
