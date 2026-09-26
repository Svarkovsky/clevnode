//! I2PInterface: Reticulum over the I2P anonymous network via SAM v3.
//!
//! An `I2PInterface` carries HDLC-framed Reticulum packets over I2P STREAM
//! connections brokered by a local SAM bridge (i2pd / Java I2P on TCP
//! 127.0.0.1:7656). It mirrors Python Reticulum's `I2PInterface`: each
//! configured `peers` entry is an outbound client sub-interface that connects
//! to a remote `.b32.i2p` destination, and `connectable = yes` opens a local
//! endpoint that accepts inbound connections, spawning one sub-interface per
//! peer. On the stream, framing and semantics are byte-identical to Python, so
//! an lnsd I2P link interoperates with an rnsd one on the same I2P network.
//!
//! ## Interface isolation
//!
//! The carrier-medium quirk owned here is I2P tunnel setup: building an I2P
//! tunnel takes seconds to minutes, tunnels go stale, and the SAM session must
//! be held open for the lifetime of every stream it spawns. The interface
//! absorbs all of that behind the same channel contract every other interface
//! presents to the core: `IncomingPacket` in, `OutgoingPacket` out. The core,
//! transport, and daemon see a plain packet pipe.
//!
//! Unlike Python, we do not proxy the SAM stream through a second local TCP
//! socket (Python does, only because its bundled `i2plib` is a generic tunnel
//! library). We read and write the SAM stream socket directly. The bytes on
//! the I2P stream are identical either way, so this is a Priority-1 robustness
//! simplification with no wire-format cost.

pub(crate) mod sam;

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use leviculum_core::constants::MTU;
use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};
use leviculum_core::transport::InterfaceId;
use rand_core::RngCore;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::{
    IncomingPacket, InterfaceCounters, InterfaceHandle, InterfaceInfo, OutgoingPacket, ReadySignal,
};
use sam::{Destination, SamError};

/// Default channel buffer size for I2P sub-interfaces. Matches the TCP default;
/// large enough to absorb bursts while a tunnel is rebuilding.
pub(crate) const I2P_DEFAULT_BUFFER_SIZE: usize = 256;

/// I2P link MTU advertised for link-MTU negotiation. Matches Python
/// `I2PInterface.HW_MTU = 1064`, so MTU negotiation with an rnsd I2P peer
/// agrees.
const I2P_HW_MTU: u32 = 1064;

/// Wait between tunnel-rebuild attempts (Python `I2PInterfacePeer.RECONNECT_WAIT`).
pub(crate) const I2P_DEFAULT_RECONNECT_WAIT: Duration = Duration::from_secs(15);

/// Keepalive cadence. Python sends `FLAG FLAG` (an empty HDLC frame) when the
/// stream has been write-idle for `I2P_PROBE_AFTER` (10 s); the empty frame
/// keeps the I2P tunnel warm and surfaces a dead tunnel as a write error. We
/// emit it on a fixed 10 s cadence, which the deframer treats as a no-op.
const I2P_KEEPALIVE: Duration = Duration::from_secs(10);

/// Read buffer sizing, mirroring the TCP interface.
const READ_BUFFER_MULTIPLIER: usize = 4;
const FRAME_BUFFER_MULTIPLIER: usize = 2;

/// Generate a random SAM session id (Python i2plib `generate_session_id`:
/// `"reticulum-" + 6 random letters`). The nick must be unique per live
/// session on the bridge.
fn generate_session_id() -> String {
    let mut bytes = [0u8; 6];
    rand_core::OsRng.fill_bytes(&mut bytes);
    let suffix: String = bytes.iter().map(|b| (b'a' + (b % 26)) as char).collect();
    format!("reticulum-{suffix}")
}

/// Configuration for one outbound I2P client sub-interface (one `peers` entry).
pub(crate) struct I2pClientConfig {
    pub id: InterfaceId,
    pub name: String,
    /// SAM bridge address (`host:port`).
    pub sam_address: String,
    /// Remote destination: a `.b32.i2p` address (resolved via NAMING LOOKUP) or
    /// a raw base64 destination.
    pub peer: String,
    pub buffer_size: usize,
    pub reconnect_wait: Duration,
    pub ifac: Option<leviculum_core::ifac::IfacConfig>,
    pub reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
    /// Resolved `ingress_control` of the parent I2P interface entry, inherited
    /// by this sub-interface (Codeberg #189).
    pub ingress_control: bool,
    /// Applied to every TCP socket this sub-interface dials toward the SAM
    /// bridge, before it connects. See [`crate::socket_hook::OutboundSocketHook`].
    pub outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
}

/// Configuration for the `connectable` server endpoint of an I2P interface.
pub(crate) struct I2pServerConfig {
    pub sam_address: String,
    /// Path to the persistent private-key file for a stable `.b32.i2p` address.
    pub keyfile: PathBuf,
    pub buffer_size: usize,
    pub name_prefix: String,
    pub reconnect_wait: Duration,
    pub next_id: Arc<AtomicUsize>,
    pub new_interface_tx: mpsc::Sender<InterfaceHandle>,
    pub ifac: Option<leviculum_core::ifac::IfacConfig>,
    /// Resolved `ingress_control` of the parent I2P interface entry, inherited
    /// by every accepted connection (Codeberg #189).
    pub ingress_control: bool,
    /// Applied to every TCP socket the server endpoint dials toward the SAM
    /// bridge (control and accept sockets), before it connects.
    pub outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
}

/// Spawn an outbound I2P client sub-interface.
///
/// Returns an `InterfaceHandle` immediately; the SAM session and I2P stream are
/// established asynchronously in a background task that also owns tunnel
/// rebuilding. Outgoing packets buffer in the channel while a tunnel is being
/// built. Mirrors `spawn_tcp_client_with_reconnect`.
pub(crate) fn spawn_i2p_client(config: I2pClientConfig) -> InterfaceHandle {
    let (incoming_tx, incoming_rx) = mpsc::channel(config.buffer_size);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(config.buffer_size);
    let counters = Arc::new(InterfaceCounters::new());
    // Offline until the reconnect loop establishes the I2P stream (L-0020).
    counters.set_online(false);
    let ready = ReadySignal::new();

    let id = config.id;
    let task_ready = Arc::clone(&ready);
    let task_counters = Arc::clone(&counters);
    let name = config.name.clone();

    tokio::spawn(async move {
        i2p_client_task(
            id,
            config.sam_address,
            name,
            config.peer,
            incoming_tx,
            outgoing_rx,
            config.reconnect_wait,
            task_counters,
            config.reconnect_notify,
            task_ready,
            config.outbound_socket_hook,
        )
        .await;
    });

    InterfaceHandle {
        info: InterfaceInfo {
            id,
            name: config.name,
            hw_mtu: Some(I2P_HW_MTU),
            is_local_client: false,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac: config.ifac,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::I2p,
            // Codeberg #189: every sub-interface of an I2P entry inherits the
            // entry's resolved ingress control, as in the reference where the
            // spawned interface takes `self.ingress_control` from the one
            // parent interface (I2PInterface.py:951).
            ingress_control: Some(config.ingress_control),
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        ready,
    }
}

/// Client reconnect loop: (re)establish the SAM session + I2P stream, run I/O,
/// and rebuild the whole tunnel on loss. Python rebuilds the tunnel wholesale
/// on any stream failure; we do the same (drop the session control socket and
/// the stream, wait, redo the handshake). Session creation is cheap once the
/// router is warm, and a full rebuild is the robust path when a tunnel goes
/// stale.
#[allow(clippy::too_many_arguments)]
async fn i2p_client_task(
    id: InterfaceId,
    sam_address: String,
    name: String,
    peer: String,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    reconnect_wait: Duration,
    counters: Arc<InterfaceCounters>,
    reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
    ready: Arc<ReadySignal>,
    outbound_socket_hook: Option<crate::socket_hook::OutboundSocketHook>,
) {
    let nick = generate_session_id();
    let mut has_connected_before = false;

    loop {
        match establish_client_stream(&sam_address, &nick, &peer, outbound_socket_hook.as_ref())
            .await
        {
            Ok((ctrl, stream)) => {
                tracing::info!("{}: I2P stream established to {}", name, peer);
                ready.signal_ready();
                counters.set_online(true);
                let is_reconnect = has_connected_before;
                has_connected_before = true;
                if is_reconnect {
                    if let Some(ref notify) = reconnect_notify {
                        let _ = notify.try_send(id);
                    }
                }

                // Hold the session control socket open for the lifetime of the
                // stream: closing it would tear the SAM session (and thus the
                // stream) down.
                outgoing_rx = i2p_stream_task(
                    name.clone(),
                    stream,
                    incoming_tx.clone(),
                    outgoing_rx,
                    Arc::clone(&counters),
                )
                .await;
                drop(ctrl);
                counters.set_online(false);
                tracing::warn!("{}: I2P stream lost, rebuilding tunnel", name);
            }
            Err(e) => {
                tracing::warn!("{}: I2P tunnel setup to {} failed: {}", name, peer, e);
            }
        }

        if incoming_tx.is_closed() {
            tracing::debug!("{}: event loop shut down, stopping I2P client", name);
            return;
        }
        tokio::time::sleep(reconnect_wait).await;
    }
}

/// Establish a SAM STREAM session and connect to a remote destination.
///
/// Returns the control socket (which must stay open to keep the session alive)
/// and the stream socket (the raw bidirectional I2P stream carrying HDLC).
async fn establish_client_stream(
    sam_address: &str,
    nick: &str,
    peer: &str,
    hook: Option<&crate::socket_hook::OutboundSocketHook>,
) -> Result<(TcpStream, TcpStream), SamError> {
    // 1. Control socket: HELLO + SESSION CREATE (transient destination).
    let mut ctrl = crate::socket_hook::connect_hooked_host(sam_address, hook).await?;
    sam::handshake(&mut ctrl).await?;
    let reply = sam::command(
        &mut ctrl,
        &sam::session_create("STREAM", nick, sam::TRANSIENT_DESTINATION, ""),
    )
    .await?;
    if !reply.ok() {
        return Err(SamError::Result(reply.result().to_string()));
    }

    // 2. Resolve the peer to a full base64 destination.
    let dest_b64 = resolve_destination(sam_address, peer, hook).await?;

    // 3. Stream socket: HELLO + STREAM CONNECT.
    let mut stream = crate::socket_hook::connect_hooked_host(sam_address, hook).await?;
    sam::handshake(&mut stream).await?;
    let reply = sam::command(&mut stream, &sam::stream_connect(nick, &dest_b64, false)).await?;
    if !reply.ok() {
        return Err(SamError::Result(reply.result().to_string()));
    }

    Ok((ctrl, stream))
}

/// Resolve a peer address to a full base64 destination. A `.i2p` address is
/// looked up via `NAMING LOOKUP` on a throwaway SAM socket; a bare base64
/// destination is used as-is (matches i2plib `stream_connect`).
async fn resolve_destination(
    sam_address: &str,
    peer: &str,
    hook: Option<&crate::socket_hook::OutboundSocketHook>,
) -> Result<String, SamError> {
    if !peer.ends_with(".i2p") {
        return Ok(peer.to_string());
    }
    let mut lookup = crate::socket_hook::connect_hooked_host(sam_address, hook).await?;
    sam::handshake(&mut lookup).await?;
    let reply = sam::command(&mut lookup, &sam::naming_lookup(peer)).await?;
    if !reply.ok() {
        return Err(SamError::Result(reply.result().to_string()));
    }
    reply
        .get("VALUE")
        .map(|s| s.to_string())
        .ok_or_else(|| SamError::Protocol("NAMING REPLY missing VALUE".to_string()))
}

/// Spawn the `connectable` server task: create (or restore) a persistent SAM
/// session and accept inbound I2P connections, spawning one sub-interface per
/// peer via `new_interface_tx`. Returns immediately; all work is async.
pub(crate) fn spawn_i2p_server(config: I2pServerConfig) {
    tokio::spawn(async move {
        loop {
            match run_server_session(&config).await {
                Ok(()) => {
                    tracing::debug!(
                        "{}: I2P server session ended, restarting",
                        config.name_prefix
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "{}: I2P server session error: {} (check that i2pd is running with SAM enabled)",
                        config.name_prefix,
                        e
                    );
                }
            }
            if config.new_interface_tx.is_closed() {
                return;
            }
            tokio::time::sleep(config.reconnect_wait).await;
        }
    });
}

/// One server session lifecycle: create the persistent-destination session,
/// then loop accepting inbound streams. Returns `Ok(())` when the event loop
/// shuts down; returns `Err` when the session/bridge fails so the caller
/// rebuilds it.
async fn run_server_session(config: &I2pServerConfig) -> Result<(), SamError> {
    let nick = generate_session_id();

    // Control socket for the session; kept open for its whole lifetime.
    let mut ctrl = crate::socket_hook::connect_hooked_host(
        &config.sam_address,
        config.outbound_socket_hook.as_ref(),
    )
    .await?;
    sam::handshake(&mut ctrl).await?;

    // Persistent destination: reuse the stored private key if present, else ask
    // SAM for a fresh one (TRANSIENT) and persist what it generates so the
    // `.b32.i2p` address is stable across restarts.
    let stored_key = load_keyfile(&config.keyfile);
    let dest_arg = stored_key
        .clone()
        .unwrap_or_else(|| sam::TRANSIENT_DESTINATION.to_string());
    let reply = sam::command(
        &mut ctrl,
        &sam::session_create("STREAM", &nick, &dest_arg, ""),
    )
    .await?;
    if !reply.ok() {
        return Err(SamError::Result(reply.result().to_string()));
    }

    // Determine our private key: either the stored one, or the one SAM
    // generated for a TRANSIENT session (returned in DESTINATION).
    let priv_key = match stored_key {
        Some(k) => k,
        None => {
            let generated = reply.get("DESTINATION").ok_or_else(|| {
                SamError::Protocol("SESSION STATUS missing generated DESTINATION".to_string())
            })?;
            save_keyfile(&config.keyfile, generated);
            generated.to_string()
        }
    };

    let dest = Destination::from_private_base64(&priv_key)?;
    tracing::info!(
        "{}: I2P endpoint reachable at {}.b32.i2p",
        config.name_prefix,
        dest.base32()
    );

    // Accept loop. Each STREAM ACCEPT socket handles exactly one inbound
    // connection; we re-issue after each. With SILENT=false the first line the
    // socket delivers, once a peer connects, is that peer's full destination.
    loop {
        let mut accept_sock = crate::socket_hook::connect_hooked_host(
            &config.sam_address,
            config.outbound_socket_hook.as_ref(),
        )
        .await?;
        sam::handshake(&mut accept_sock).await?;
        let reply = sam::command(&mut accept_sock, &sam::stream_accept(&nick, false)).await?;
        if !reply.ok() {
            // e.g. the session died (INVALID_ID); let the caller rebuild it.
            return Err(SamError::Result(reply.result().to_string()));
        }

        // Block until a peer connects; the first line is its base64 destination.
        let peer_dest = sam::read_line(&mut accept_sock).await?;
        let peer_b32 = Destination::from_public_base64(peer_dest.trim())
            .map(|d| d.base32())
            .unwrap_or_else(|_| "unknown".to_string());
        let id = InterfaceId(config.next_id.fetch_add(1, Ordering::Relaxed));
        let name = format!("{}/{}.b32.i2p", config.name_prefix, peer_b32);
        tracing::info!("{}: accepted I2P connection ({})", config.name_prefix, id);

        let handle = spawn_i2p_accepted(
            id,
            name,
            accept_sock,
            config.buffer_size,
            config.ifac.clone(),
            config.ingress_control,
        );
        if config.new_interface_tx.send(handle).await.is_err() {
            return Ok(()); // event loop shut down
        }
    }
}

/// Build an `InterfaceHandle` around an already-accepted I2P stream socket. The
/// peer-destination line has already been consumed, so the socket now carries
/// pure HDLC. Pre-signaled ready, like a TCP-server-accepted connection.
fn spawn_i2p_accepted(
    id: InterfaceId,
    name: String,
    stream: TcpStream,
    buffer_size: usize,
    ifac: Option<leviculum_core::ifac::IfacConfig>,
    ingress_control: bool,
) -> InterfaceHandle {
    let (incoming_tx, incoming_rx) = mpsc::channel(buffer_size);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(buffer_size);
    let counters = Arc::new(InterfaceCounters::new());

    let task_name = name.clone();
    let task_counters = Arc::clone(&counters);
    tokio::spawn(async move {
        let _rx = i2p_stream_task(task_name, stream, incoming_tx, outgoing_rx, task_counters).await;
    });

    InterfaceHandle {
        info: InterfaceInfo {
            id,
            name,
            hw_mtu: Some(I2P_HW_MTU),
            is_local_client: false,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::I2p,
            // Codeberg #189, I2PInterface.py:951: inherited from the endpoint
            // that accepted this connection.
            ingress_control: Some(ingress_control),
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        ready: ReadySignal::ready_immediate(),
    }
}

/// Bidirectional HDLC I/O over a SAM stream socket.
///
/// Read path: drain the socket, HDLC-deframe, forward frames to `incoming_tx`.
/// Write path: HDLC-frame outgoing packets and write them to the socket.
/// Keepalive: on a 10 s idle cadence, emit an empty HDLC frame (`FLAG FLAG`) to
/// keep the tunnel warm and to detect a silently-dead tunnel as a write error.
///
/// Returns `outgoing_rx` on stream loss so the client reconnect loop can reuse
/// the channel with a freshly built tunnel.
async fn i2p_stream_task(
    name: String,
    stream: TcpStream,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
) -> mpsc::Receiver<OutgoingPacket> {
    let (reader, mut writer) = stream.into_split();

    let mut deframer = Deframer::with_max_frame(I2P_HW_MTU as usize);
    let mut read_buf = vec![0u8; MTU * READ_BUFFER_MULTIPLIER];
    let mut frame_buf = Vec::with_capacity(MTU * FRAME_BUFFER_MULTIPLIER);
    let mut keepalive = tokio::time::interval(I2P_KEEPALIVE);
    // The first tick fires immediately; skip it so we do not emit a keepalive
    // before any real traffic.
    keepalive.tick().await;

    loop {
        tokio::select! {
            result = reader.readable() => {
                match result {
                    Ok(()) => {
                        loop {
                            match reader.try_read(&mut read_buf) {
                                Ok(0) => {
                                    tracing::debug!("I2P interface {} disconnected (EOF)", name);
                                    return outgoing_rx;
                                }
                                Ok(n) => {
                                    counters.rx_bytes.fetch_add(n as usize, Ordering::Relaxed);
                                    for r in deframer.process(&read_buf[..n]) {
                                        // HW_MTU enforcement lives in the deframer now.
                                        if matches!(r, DeframeResult::Oversized) {
                                            tracing::trace!(
                                                "I2P {}: frame exceeds HW_MTU, discarded", name);
                                            continue;
                                        }
                                        if let DeframeResult::Frame(data) = r {
                                            if incoming_tx.send(IncomingPacket { data }).await.is_err() {
                                                return outgoing_rx;
                                            }
                                        }
                                    }
                                }
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                                Err(e) => {
                                    tracing::debug!("I2P interface {} read error: {}", name, e);
                                    return outgoing_rx;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::debug!("I2P interface {} readability error: {}", name, e);
                        return outgoing_rx;
                    }
                }
            }

            msg = outgoing_rx.recv() => {
                match msg {
                    Some(pkt) => {
                        frame(&pkt.data, &mut frame_buf);
                        if let Err(e) = writer.write_all(&frame_buf).await {
                            tracing::debug!("I2P interface {} write error: {}", name, e);
                            return outgoing_rx;
                        }
                        counters.tx_bytes.fetch_add(frame_buf.len() as usize, Ordering::Relaxed);
                    }
                    None => {
                        tracing::debug!("I2P interface {} outgoing channel closed", name);
                        return outgoing_rx;
                    }
                }
            }

            _ = keepalive.tick() => {
                // Empty HDLC frame: two FLAG bytes. The peer's deframer treats
                // it as a no-op; a write error means the tunnel is dead.
                if let Err(e) = writer.write_all(&[leviculum_core::framing::hdlc::FLAG, leviculum_core::framing::hdlc::FLAG]).await {
                    tracing::debug!("I2P interface {} keepalive write error: {}", name, e);
                    return outgoing_rx;
                }
            }
        }
    }
}

/// Load a persisted base64 private key, trimming whitespace. Returns `None` if
/// the file is absent or unreadable.
fn load_keyfile(path: &PathBuf) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Persist a base64 private key, creating the parent directory as needed. A
/// failure is logged but not fatal (the session still works this run; only the
/// stable-address property across restarts is lost).
fn save_keyfile(path: &PathBuf, private_key_base64: &str) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("could not create I2P key directory {:?}: {}", parent, e);
            return;
        }
    }
    if let Err(e) = std::fs::write(path, private_key_base64) {
        tracing::warn!("could not persist I2P key {:?}: {}", path, e);
    }
}

#[cfg(test)]
mod tests;

// Live tests against a real i2pd SAM bridge. `#[ignore]` by default: they need
// i2pd running with `sam.enabled = true` on 127.0.0.1:7656, and the full
// loopback builds real I2P tunnels (minutes). Run with
// `cargo test -p leviculum-std i2pd_live -- --ignored --nocapture`.
#[cfg(test)]
mod i2pd_live;
