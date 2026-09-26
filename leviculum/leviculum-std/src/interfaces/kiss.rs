use crate::tokio_serial;
// KISS interface. KISS-framed serial link to a TNC / modem.
//
// Implements Python Reticulum's `KISSInterface`
// (`RNS/Interfaces/KISSInterface.py`): open a serial port to a KISS TNC, send
// the TNC-configuration command frames on startup (preamble / txtail /
// persistence / slottime / flow-control ready), then frame outgoing packets as
// KISS data frames and deframe incoming KISS data frames.
//
// The frame/deframe primitives live in `leviculum_core::framing::kiss` so they
// are shared with the RNode interface and reusable by AX.25 framing (#97),
// which layers on the same KISS command set.
//
// # TNC vs host parameters
//
// - **TNC parameters** are pushed to the modem as KISS command frames at
//   startup and change how the TNC keys the radio: `preamble` (CMD_TXDELAY),
//   `txtail` (CMD_TXTAIL), `persistence` (CMD_P), `slottime` (CMD_SLOTTIME),
//   and the `CMD_READY` flow-control enable. They are write-only; the TNC does
//   not echo them.
// - **Host parameters** stay on our side of the wire: the serial line settings
//   (`port`, `speed`, `databits`, `parity`, `stopbits`), `flow_control` (which
//   gates our TX on the TNC's CMD_READY), and the beacon identification
//   (`id_interval` / `id_callsign`).

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use leviculum_core::constants::MTU;
use leviculum_core::framing::ax25::Ax25Addressing;
use leviculum_core::framing::kiss::{self, KissDeframeResult, KissDeframer};
use leviculum_core::transport::InterfaceId;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{IncomingPacket, InterfaceCounters, InterfaceHandle, InterfaceInfo, OutgoingPacket};

/// Python KISSInterface `HW_MTU` (KISSInterface.py:102).
pub(crate) const KISS_HW_MTU: u32 = 564;

/// Default channel buffer size for KISS interfaces.
pub(crate) const KISS_DEFAULT_BUFFER_SIZE: usize = 64;

/// Frame buffer multiplier (accounts for KISS escaping overhead).
const FRAME_BUFFER_MULTIPLIER: usize = 2;

/// Read buffer size for pulling bytes off the serial port.
const READ_BUF_SIZE: usize = 1024;

/// Incomplete-frame timeout. Matches Python KISSInterface.timeout = 100 (ms):
/// a partial frame with no fresh bytes for this long is discarded so line noise
/// cannot wedge the deframer.
const FRAME_TIMEOUT: Duration = Duration::from_millis(100);

/// Reconnect interval after serial port loss (Python `reconnect_port` sleeps 5s).
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

/// Flow-control unlock timeout. Python `flow_control_timeout = 5`: if the TNC
/// never sends CMD_READY after a locked TX, unlock anyway so a modem that does
/// not support flow control cannot deadlock the send path.
const FLOW_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

/// Max packets queued while flow control is engaged and the TNC is busy.
/// Python's queue is unbounded; bounded here (drop-oldest) because at TNC
/// bitrates a full queue holds minutes-old stale packets.
const FLOW_CONTROL_QUEUE_LIMIT: usize = 64;

// Python KISSInterface default TNC parameters (KISSInterface.py:129-132).
/// Default preamble in ms (CMD_TXDELAY, sent as ms/10).
pub(crate) const DEFAULT_PREAMBLE_MS: u32 = 350;
/// Default TX tail in ms (CMD_TXTAIL, sent as ms/10).
pub(crate) const DEFAULT_TXTAIL_MS: u32 = 20;
/// Default persistence (CMD_P, sent raw).
pub(crate) const DEFAULT_PERSISTENCE: u32 = 64;
/// Default slot time in ms (CMD_SLOTTIME, sent as ms/10).
pub(crate) const DEFAULT_SLOTTIME_MS: u32 = 20;

/// Configuration for a KISS interface.
pub(crate) struct KissInterfaceConfig {
    pub id: InterfaceId,
    pub name: String,
    pub port: String,
    pub speed: u32,
    pub data_bits: tokio_serial::DataBits,
    pub parity: tokio_serial::Parity,
    pub stop_bits: tokio_serial::StopBits,
    /// Preamble / TX delay in ms (sent to the TNC as ms/10).
    pub preamble_ms: u32,
    /// TX tail in ms (sent to the TNC as ms/10).
    pub txtail_ms: u32,
    /// Persistence parameter (sent to the TNC raw).
    pub persistence: u32,
    /// Slot time in ms (sent to the TNC as ms/10).
    pub slottime_ms: u32,
    /// Gate TX on the TNC's CMD_READY signal (Python `flow_control`).
    pub flow_control: bool,
    /// AX.25 UI-frame addressing (`AX25KISSInterface`). When `Some`, outgoing
    /// packets are wrapped in an AX.25 header before KISS framing and the header
    /// is stripped off incoming frames; when `None` this is a plain KISS link.
    pub ax25: Option<Ax25Addressing>,
    pub buffer_size: usize,
    pub reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
}

/// Convert a "milliseconds" TNC parameter to its KISS command value (ms/10),
/// clamped to a single byte. Matches Python `int(x_ms / 10)` then 0..=255 clamp
/// in `setPreamble` / `setTxTail` / `setSlotTime`.
fn ms_param_to_byte(ms: u32) -> u8 {
    (ms / 10).min(255) as u8
}

/// Clamp a raw TNC parameter (persistence) to a single byte, matching Python
/// `setPersistence`'s 0..=255 clamp.
fn raw_param_to_byte(value: u32) -> u8 {
    value.min(255) as u8
}

/// Build a 4-byte KISS command frame: `FEND cmd value FEND`.
///
/// The value byte is written raw, WITHOUT escaping, matching Python
/// (`bytes([FEND])+bytes([cmd])+bytes([value])+bytes([FEND])`). A value that
/// happens to equal FEND/FESC is therefore sent unescaped on purpose: this is
/// the exact byte sequence Python emits, so it stays wire-compatible with the
/// TNCs deployed against Python Reticulum.
fn build_kiss_command(cmd: u8, value: u8) -> [u8; 4] {
    [kiss::FEND, cmd, value, kiss::FEND]
}

/// Send the TNC-configuration command frames (Python `configure_device` →
/// setPreamble/setTxTail/setPersistence/setSlotTime/setFlowControl).
///
/// `setFlowControl` always emits CMD_READY(0x01) regardless of the host-side
/// `flow_control` flag (KISSInterface.py:237-244), so we do too.
async fn send_tnc_config<S>(port: &mut S, cfg: &KissInterfaceConfig) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let commands = [
        build_kiss_command(kiss::CMD_TXDELAY, ms_param_to_byte(cfg.preamble_ms)),
        build_kiss_command(kiss::CMD_TXTAIL, ms_param_to_byte(cfg.txtail_ms)),
        build_kiss_command(kiss::CMD_P, raw_param_to_byte(cfg.persistence)),
        build_kiss_command(kiss::CMD_SLOTTIME, ms_param_to_byte(cfg.slottime_ms)),
        build_kiss_command(kiss::CMD_READY, 0x01),
    ];
    for command in &commands {
        port.write_all(command).await?;
    }
    port.flush().await?;
    Ok(())
}

/// Frame a payload as a KISS data frame and write it to the port.
///
/// With `ax25` set, the payload is first wrapped in an AX.25 UI-frame header
/// (`AX25KISSInterface`), then the whole AX.25 frame is KISS-framed — matching
/// Python's `process_outgoing`, which prepends the address/control/PID header
/// and KISS-frames the result. Without it this is a plain KISS data frame.
async fn write_data_frame<S>(
    port: &mut S,
    payload: &[u8],
    ax25: Option<&Ax25Addressing>,
    frame_buf: &mut Vec<u8>,
    counters: &InterfaceCounters,
) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    match ax25 {
        Some(addr) => {
            let ax25_frame = addr.wrap(payload);
            kiss::frame(kiss::CMD_DATA, &ax25_frame, frame_buf);
        }
        None => kiss::frame(kiss::CMD_DATA, payload, frame_buf),
    }
    port.write_all(frame_buf).await?;
    port.flush().await?;
    // Count the packet bytes, not the AX.25/KISS overhead, matching Python's
    // `self.txb += datalen`.
    counters
        .tx_bytes
        .fetch_add(payload.len() as usize, Ordering::Relaxed);
    Ok(())
}

/// Run the KISS lifecycle over an already-open byte stream: push the TNC config,
/// then drive bidirectional I/O until the stream dies.
///
/// Carrier-agnostic (any `AsyncRead + AsyncWrite`) so the reconnect loop can run
/// it over a real serial port and the tests can run it over an in-process
/// duplex. Returns `outgoing_rx` on stream loss for reconnect reuse.
async fn run_kiss_over_stream<S>(
    cfg: &KissInterfaceConfig,
    mut port: S,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
) -> mpsc::Receiver<OutgoingPacket>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if let Err(e) = send_tnc_config(&mut port, cfg).await {
        tracing::warn!(
            "KISS interface {}: TNC config write failed: {}",
            cfg.name,
            e
        );
        return outgoing_rx;
    }
    tracing::info!("KISS interface {} configured on {}", cfg.name, cfg.port);
    kiss_io_task(
        &cfg.name,
        port,
        incoming_tx,
        outgoing_rx,
        counters,
        cfg.flow_control,
        cfg.ax25.as_ref(),
    )
    .await
}

/// Bidirectional KISS I/O loop.
///
/// Read path:  serial → KISS deframe → CMD_DATA payloads to incoming channel.
/// Write path: outgoing channel → KISS CMD_DATA frame → serial.
///
/// When `flow_control` is set, at most one frame is in flight: after a TX we
/// lock until the TNC returns CMD_READY (or `FLOW_CONTROL_TIMEOUT` elapses),
/// mirroring Python's `interface_ready` gating. With it clear (the default) the
/// queue drains immediately.
#[allow(clippy::too_many_arguments)]
async fn kiss_io_task<S>(
    name: &str,
    mut port: S,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
    flow_control: bool,
    ax25: Option<&Ax25Addressing>,
) -> mpsc::Receiver<OutgoingPacket>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // An AX.25 frame carries the 16-byte UI header on top of the packet, so the
    // KISS payload budget grows by the header size (Python bounds its RX buffer
    // at `HW_MTU + AX25.HEADER_SIZE`).
    let max_frame = KISS_HW_MTU as usize
        + ax25
            .map(|_| leviculum_core::framing::ax25::HEADER_SIZE)
            .unwrap_or(0);
    let mut deframer = KissDeframer::with_max_payload(max_frame);
    let mut read_buf = vec![0u8; READ_BUF_SIZE];
    let mut frame_buf = Vec::with_capacity(MTU * FRAME_BUFFER_MULTIPLIER);
    let mut last_read_at = Instant::now();

    // TX gating (flow control).
    let mut interface_ready = true;
    let mut send_queue: VecDeque<Vec<u8>> = VecDeque::new();
    let mut flow_control_locked_at = Instant::now();

    loop {
        // Flush as much of the queue as the flow-control gate allows.
        while !send_queue.is_empty() && (!flow_control || interface_ready) {
            let payload = send_queue.pop_front().expect("queue non-empty");
            if let Err(e) =
                write_data_frame(&mut port, &payload, ax25, &mut frame_buf, &counters).await
            {
                tracing::debug!("KISS interface {}: write error: {}", name, e);
                return outgoing_rx;
            }
            tracing::debug!("KISS interface {} TX {} bytes", name, payload.len());
            if flow_control {
                interface_ready = false;
                flow_control_locked_at = Instant::now();
                break;
            }
        }

        // Compute the next timer deadline: the sooner of the mid-frame timeout
        // and the flow-control unlock. Default to effectively infinite.
        let mut timeout = Duration::from_secs(3600);
        if deframer.is_in_frame() {
            let elapsed = last_read_at.elapsed();
            if elapsed >= FRAME_TIMEOUT {
                deframer.reset();
            } else {
                timeout = timeout.min(FRAME_TIMEOUT - elapsed);
            }
        }
        if flow_control && !interface_ready {
            let locked = flow_control_locked_at.elapsed();
            if locked >= FLOW_CONTROL_TIMEOUT {
                tracing::warn!(
                    "KISS interface {}: flow-control unlock on timeout (TNC missed CMD_READY?)",
                    name
                );
                interface_ready = true;
                continue;
            }
            timeout = timeout.min(FLOW_CONTROL_TIMEOUT - locked);
        }

        tokio::select! {
            // Read path
            result = port.read(&mut read_buf) => {
                match result {
                    Ok(0) => {
                        tracing::debug!("KISS interface {} EOF", name);
                        return outgoing_rx;
                    }
                    Ok(n) => {
                        last_read_at = Instant::now();
                        for r in deframer.process(&read_buf[..n]) {
                            if let KissDeframeResult::Frame { command, payload } = r {
                                match command {
                                    kiss::CMD_DATA => {
                                        // With AX.25 addressing, strip the 16-byte
                                        // UI-frame header before delivering (Python
                                        // `process_incoming`); a frame no longer than
                                        // the header carries no packet and is dropped.
                                        let data = match ax25 {
                                            Some(_) => {
                                                match leviculum_core::framing::ax25::strip_header(
                                                    &payload,
                                                ) {
                                                    Some(inner) => inner.to_vec(),
                                                    None => continue,
                                                }
                                            }
                                            None => payload,
                                        };
                                        counters.rx_bytes.fetch_add(
                                            data.len() as usize, Ordering::Relaxed);
                                        if incoming_tx
                                            .send(IncomingPacket { data })
                                            .await
                                            .is_err()
                                        {
                                            return outgoing_rx;
                                        }
                                    }
                                    kiss::CMD_READY => {
                                        // TNC is ready for the next frame.
                                        if flow_control {
                                            interface_ready = true;
                                        }
                                    }
                                    other => {
                                        tracing::trace!(
                                            "KISS interface {}: ignoring non-data cmd 0x{:02X}",
                                            name, other
                                        );
                                    }
                                }
                            }
                        }
                        // No HW_MTU check here: `KissDeframer::with_max_payload`
                        // already refuses to buffer past `max_frame`, so this
                        // condition was unreachable. One mechanism, not two
                        // (Codeberg #271).
                    }
                    Err(e) => {
                        tracing::debug!("KISS interface {} read error: {}", name, e);
                        return outgoing_rx;
                    }
                }
            }

            // Write path: enqueue; the flush at the top of the loop sends it.
            msg = outgoing_rx.recv() => {
                match msg {
                    Some(pkt) => {
                        if send_queue.len() >= FLOW_CONTROL_QUEUE_LIMIT {
                            tracing::warn!("KISS interface {}: send queue full, dropping oldest",
                                name);
                            send_queue.pop_front();
                        }
                        send_queue.push_back(pkt.data);
                    }
                    None => {
                        tracing::debug!("KISS interface {} outgoing channel closed", name);
                        return outgoing_rx;
                    }
                }
            }

            // Timer: frame timeout and/or flow-control unlock.
            _ = tokio::time::sleep(timeout) => {
                if deframer.is_in_frame() && last_read_at.elapsed() >= FRAME_TIMEOUT {
                    tracing::trace!("KISS interface {}: frame timeout, discarding partial", name);
                    deframer.reset();
                }
                // Flow-control unlock is handled by the deadline recompute at
                // the top of the loop.
            }
        }
    }
}

/// Spawn a KISS interface with automatic reconnection.
///
/// Mirrors the serial/TCP pattern: the channel pair is created once and a
/// reconnect task keeps the port alive across failures, so the returned
/// `InterfaceHandle` stays valid.
pub(crate) fn spawn_kiss_interface(config: KissInterfaceConfig) -> InterfaceHandle {
    let (incoming_tx, incoming_rx) = mpsc::channel(config.buffer_size);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(config.buffer_size);
    let counters = Arc::new(InterfaceCounters::new());

    let id = config.id;
    let handle_name = config.name.clone();
    let task_counters = Arc::clone(&counters);

    tokio::spawn(async move {
        kiss_reconnect_task(config, incoming_tx, outgoing_rx, task_counters).await;
    });

    InterfaceHandle {
        info: InterfaceInfo {
            id,
            name: handle_name,
            hw_mtu: Some(KISS_HW_MTU),
            is_local_client: false,
            bitrate: None,
            tx_jitter_max_ms: None,
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::Kiss,
            ingress_control: None,
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        // A KISS serial link carries no radio physics on our side (the TNC owns
        // the airtime), so no airtime budget — "always ready" like plain serial.
        credit: None,
        ready: super::ReadySignal::ready_immediate(),
    }
}

/// Reconnect wrapper: open the port, run the KISS lifecycle, retry on loss.
async fn kiss_reconnect_task(
    config: KissInterfaceConfig,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
) {
    let mut has_connected_before = false;
    loop {
        // Batch USB CDC-ACM writes into bulk transfers instead of byte-by-byte,
        // so KISS frames do not arrive one byte at a time (same fix serial.rs
        // applies via pyserial's set_low_latency_mode). Best-effort.
        let _ = std::process::Command::new("stty")
            .args(["-F", &config.port, "low_latency"])
            .output();

        let builder = tokio_serial::new(&config.port, config.speed)
            .data_bits(config.data_bits)
            .stop_bits(config.stop_bits)
            .parity(config.parity)
            .flow_control(tokio_serial::FlowControl::None);

        match tokio_serial::SerialStream::open(&builder) {
            Ok(port) => {
                let is_reconnect = has_connected_before;
                has_connected_before = true;
                tracing::info!("KISS interface {} online on {}", config.name, config.port);

                if is_reconnect {
                    if let Some(ref notify) = config.reconnect_notify {
                        let _ = notify.try_send(config.id);
                    }
                }

                outgoing_rx = run_kiss_over_stream(
                    &config,
                    port,
                    incoming_tx.clone(),
                    outgoing_rx,
                    Arc::clone(&counters),
                )
                .await;
                tracing::warn!("KISS interface {}: port lost, will reconnect", config.name);
            }
            Err(e) => {
                tracing::warn!(
                    "KISS interface {}: open {} failed: {}",
                    config.name,
                    config.port,
                    e
                );
            }
        }

        if incoming_tx.is_closed() {
            tracing::debug!("KISS interface {}: event loop shut down", config.name);
            return;
        }
        tracing::info!(
            "KISS interface {}: reconnecting in {}s",
            config.name,
            RECONNECT_INTERVAL.as_secs()
        );
        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_core::framing::kiss::{FEND, FESC, TFEND, TFESC};

    fn base_config(name: &str) -> KissInterfaceConfig {
        KissInterfaceConfig {
            id: InterfaceId(0),
            name: name.to_string(),
            port: "/dev/null-test".to_string(),
            speed: 9600,
            data_bits: tokio_serial::DataBits::Eight,
            parity: tokio_serial::Parity::None,
            stop_bits: tokio_serial::StopBits::One,
            preamble_ms: DEFAULT_PREAMBLE_MS,
            txtail_ms: DEFAULT_TXTAIL_MS,
            persistence: DEFAULT_PERSISTENCE,
            slottime_ms: DEFAULT_SLOTTIME_MS,
            flow_control: false,
            ax25: None,
            buffer_size: KISS_DEFAULT_BUFFER_SIZE,
            reconnect_notify: None,
        }
    }

    /// The TNC-parameter → command-byte conversion matches Python:
    /// preamble/txtail/slottime are ms/10, persistence is raw, all clamped to a
    /// byte.
    #[test]
    fn tnc_param_byte_conversion_matches_python() {
        // Python defaults: preamble 350 → 35, txtail 20 → 2, slottime 20 → 2.
        assert_eq!(ms_param_to_byte(DEFAULT_PREAMBLE_MS), 35);
        assert_eq!(ms_param_to_byte(DEFAULT_TXTAIL_MS), 2);
        assert_eq!(ms_param_to_byte(DEFAULT_SLOTTIME_MS), 2);
        // Persistence is sent raw.
        assert_eq!(raw_param_to_byte(DEFAULT_PERSISTENCE), 64);
        // Clamp: a huge ms value saturates at 255, not wraps.
        assert_eq!(ms_param_to_byte(10_000), 255);
        assert_eq!(raw_param_to_byte(1_000), 255);
    }

    /// The exact bytes Python's `configure_device` writes for the default TNC
    /// parameters, in order. This is the wire contract with a KISS TNC.
    #[tokio::test]
    async fn tnc_config_bytes_match_python() {
        let cfg = base_config("kiss-cfg");
        let mut sink: Vec<u8> = Vec::new();
        send_tnc_config(&mut sink, &cfg)
            .await
            .expect("config write");
        assert_eq!(
            sink,
            vec![
                FEND,
                kiss::CMD_TXDELAY,
                35,
                FEND, // preamble 350ms → 35
                FEND,
                kiss::CMD_TXTAIL,
                2,
                FEND, // txtail 20ms → 2
                FEND,
                kiss::CMD_P,
                64,
                FEND, // persistence 64
                FEND,
                kiss::CMD_SLOTTIME,
                2,
                FEND, // slottime 20ms → 2
                FEND,
                kiss::CMD_READY,
                0x01,
                FEND, // flow-control ready
            ]
        );
    }

    /// A command parameter that collides with FEND is written raw (unescaped),
    /// byte-for-byte as Python emits it — wire compatibility over cleanliness.
    #[test]
    fn command_value_is_not_escaped() {
        // persistence 192 == FEND (0xC0): Python sends it raw.
        assert_eq!(
            build_kiss_command(kiss::CMD_P, 192),
            [FEND, kiss::CMD_P, 0xC0, FEND]
        );
    }

    /// Drive `kiss_io_task` over an in-process duplex: a payload sent on the
    /// outgoing channel must come back KISS-deframed on the incoming channel of
    /// a peer task. Uses a FEND/FESC-laden payload so the escape round-trip is
    /// exercised on the real I/O path, not just the framer unit tests.
    #[tokio::test]
    async fn io_task_round_trips_escaped_payload() {
        let (a, b) = tokio::io::duplex(64 * 1024);

        let (a_in_tx, _a_in_rx) = mpsc::channel(8);
        let (a_out_tx, a_out_rx) = mpsc::channel(8);
        let a_counters = Arc::new(InterfaceCounters::new());
        tokio::spawn(async move {
            kiss_io_task("A", a, a_in_tx, a_out_rx, a_counters, false, None).await;
        });

        let (b_in_tx, mut b_in_rx) = mpsc::channel(8);
        let (_b_out_tx, b_out_rx) = mpsc::channel(8);
        let b_counters = Arc::new(InterfaceCounters::new());
        tokio::spawn(async move {
            kiss_io_task("B", b, b_in_tx, b_out_rx, b_counters, false, None).await;
        });

        let payload = vec![FEND, 0x11, FESC, TFEND, TFESC, 0x00, FEND, 0xDB, 0x42];
        a_out_tx
            .send(OutgoingPacket {
                peer: None,
                data: payload.clone(),
                high_priority: false,
            })
            .await
            .expect("send into A");

        let got = tokio::time::timeout(Duration::from_secs(5), b_in_rx.recv())
            .await
            .expect("B receives within timeout")
            .expect("channel open");
        assert_eq!(
            got.data, payload,
            "payload must survive the KISS round-trip"
        );
    }

    /// End-to-end: a genuine Reticulum announce crosses a KISS-framed link
    /// between two interface endpoints.
    ///
    /// Two `run_kiss_over_stream` endpoints are bridged by an in-process duplex
    /// (standing in for a wire between two hosts). Each endpoint first pushes its
    /// TNC-configuration command frames onto the wire; those are non-data KISS
    /// frames the peer must ignore, so their crossing also proves the config
    /// frames do not corrupt the data stream. A real announce built via
    /// `Destination::announce` is then sent from node A and must arrive on node
    /// B, unpack as an `Announce`, verify its destination hash and signature,
    /// and carry the app data intact.
    ///
    /// Gap vs. true cross-stack interop: the peer here is a second leviculum
    /// KISS endpoint, not a Python `rnsd` on a `KISSInterface` over a pty
    /// (socat is not available in this environment). The framing on the wire is
    /// byte-for-byte the KISS Python emits (locked down by
    /// `tnc_config_bytes_match_python` and the core `kiss` Python-vector tests),
    /// so this proves wire+semantic crossing; a live Python-KISS peer remains
    /// future work.
    #[tokio::test]
    async fn announce_crosses_kiss_link_between_two_nodes() {
        use leviculum_core::packet::{Packet, PacketType};
        use leviculum_core::{Destination, DestinationType, Direction, Identity};
        use rand_core::OsRng;

        // Bridge: A's stream <-> B's stream.
        let (a_stream, b_stream) = tokio::io::duplex(64 * 1024);

        // Endpoint A.
        let (a_in_tx, _a_in_rx) = mpsc::channel(8);
        let (a_out_tx, a_out_rx) = mpsc::channel(8);
        let a_cfg = base_config("kiss-A");
        let a_counters = Arc::new(InterfaceCounters::new());
        tokio::spawn(async move {
            run_kiss_over_stream(&a_cfg, a_stream, a_in_tx, a_out_rx, a_counters).await;
        });

        // Endpoint B.
        let (b_in_tx, mut b_in_rx) = mpsc::channel(8);
        let (_b_out_tx, b_out_rx) = mpsc::channel(8);
        let b_cfg = base_config("kiss-B");
        let b_counters = Arc::new(InterfaceCounters::new());
        tokio::spawn(async move {
            run_kiss_over_stream(&b_cfg, b_stream, b_in_tx, b_out_rx, b_counters).await;
        });

        // Build a real announce on node A's side.
        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "kissapp",
            &["announce", "test"],
        )
        .expect("destination");
        let dest_hash = *dest.hash();
        let announce_packet = dest
            .announce(Some(b"kiss-e2e"), &mut OsRng, 1_000, 1_000 / 1000)
            .expect("announce packet");
        let mut buf = [0u8; leviculum_core::constants::MTU];
        let len = announce_packet.pack(&mut buf).expect("pack announce");
        let announce_bytes = buf[..len].to_vec();

        a_out_tx
            .send(OutgoingPacket {
                peer: None,
                data: announce_bytes.clone(),
                high_priority: false,
            })
            .await
            .expect("send announce into A");

        // It must arrive on B, byte-identical, and parse as a verified announce.
        let got = tokio::time::timeout(Duration::from_secs(5), b_in_rx.recv())
            .await
            .expect("B receives the announce within timeout")
            .expect("channel open");
        assert_eq!(
            got.data, announce_bytes,
            "announce bytes must cross the KISS link intact"
        );

        let packet = Packet::unpack(&got.data).expect("unpack crossed packet");
        assert_eq!(
            packet.flags.packet_type,
            PacketType::Announce,
            "crossed packet must still be an announce"
        );
        assert_eq!(
            &packet.destination_hash,
            dest_hash.as_bytes(),
            "crossed announce must carry the original destination hash"
        );
    }

    // --- AX.25 (AX25KISSInterface) tests ---

    fn ax25_config(name: &str, callsign: &[u8], ssid: u8) -> KissInterfaceConfig {
        KissInterfaceConfig {
            ax25: Some(Ax25Addressing::new(callsign, ssid).expect("valid AX.25 addressing")),
            ..base_config(name)
        }
    }

    /// Byte-for-byte parity: the AX.25 TX path must emit exactly the KISS frame
    /// Python's `process_outgoing` writes — `FEND CMD_DATA <escaped(header +
    /// payload)> FEND` — for a fixed callsign/SSID and a payload with no special
    /// bytes.
    #[tokio::test]
    async fn ax25_write_frame_matches_python() {
        let cfg = ax25_config("ax25-kat", b"N0CALL", 0);
        let counters = InterfaceCounters::new();
        let mut frame_buf = Vec::new();
        let mut sink: Vec<u8> = Vec::new();
        let payload = [0x11u8, 0x22, 0x33];
        write_data_frame(
            &mut sink,
            &payload,
            cfg.ax25.as_ref(),
            &mut frame_buf,
            &counters,
        )
        .await
        .expect("write ax25 frame");

        // Expected: FEND, CMD_DATA, then the 16-byte AX.25 header (dst APZRNS-0,
        // src N0CALL-0, ctrl 0x03, PID 0xF0) followed by the payload, then FEND.
        // No byte in header/payload is FEND/FESC so nothing is escaped.
        let expected = vec![
            FEND,
            kiss::CMD_DATA,
            0x82,
            0xA0,
            0xB4,
            0xA4,
            0x9C,
            0xA6,
            0x60, // dst APZRNS-0
            0x9C,
            0x60,
            0x86,
            0x82,
            0x98,
            0x98,
            0x61, // src N0CALL-0
            0x03,
            0xF0, // ctrl UI, PID no-layer-3
            0x11,
            0x22,
            0x33, // payload
            FEND,
        ];
        assert_eq!(
            sink, expected,
            "AX.25 KISS frame must match Python byte-for-byte"
        );
    }

    /// End-to-end: a genuine Reticulum announce crosses an AX.25-over-KISS link
    /// between two nodes. Both endpoints wrap TX in an AX.25 UI header and strip
    /// it on RX; the delivered bytes must be the original packet (header gone)
    /// and unpack as a verified announce.
    #[tokio::test]
    async fn announce_crosses_ax25_kiss_link_between_two_nodes() {
        use leviculum_core::packet::{Packet, PacketType};
        use leviculum_core::{Destination, DestinationType, Direction, Identity};
        use rand_core::OsRng;

        let (a_stream, b_stream) = tokio::io::duplex(64 * 1024);

        let (a_in_tx, _a_in_rx) = mpsc::channel(8);
        let (a_out_tx, a_out_rx) = mpsc::channel(8);
        let a_cfg = ax25_config("ax25-A", b"N0CALL", 1);
        let a_counters = Arc::new(InterfaceCounters::new());
        tokio::spawn(async move {
            run_kiss_over_stream(&a_cfg, a_stream, a_in_tx, a_out_rx, a_counters).await;
        });

        let (b_in_tx, mut b_in_rx) = mpsc::channel(8);
        let (_b_out_tx, b_out_rx) = mpsc::channel(8);
        let b_cfg = ax25_config("ax25-B", b"N0CALL", 2);
        let b_counters = Arc::new(InterfaceCounters::new());
        tokio::spawn(async move {
            run_kiss_over_stream(&b_cfg, b_stream, b_in_tx, b_out_rx, b_counters).await;
        });

        let identity = Identity::generate(&mut OsRng);
        let mut dest = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            "ax25app",
            &["announce", "test"],
        )
        .expect("destination");
        let dest_hash = *dest.hash();
        let announce_packet = dest
            .announce(Some(b"ax25-e2e"), &mut OsRng, 1_000, 1_000 / 1000)
            .expect("announce packet");
        let mut buf = [0u8; leviculum_core::constants::MTU];
        let len = announce_packet.pack(&mut buf).expect("pack announce");
        let announce_bytes = buf[..len].to_vec();

        a_out_tx
            .send(OutgoingPacket {
                peer: None,
                data: announce_bytes.clone(),
                high_priority: false,
            })
            .await
            .expect("send announce into A");

        let got = tokio::time::timeout(Duration::from_secs(5), b_in_rx.recv())
            .await
            .expect("B receives the announce within timeout")
            .expect("channel open");
        assert_eq!(
            got.data, announce_bytes,
            "announce bytes must cross the AX.25/KISS link with the AX.25 header stripped"
        );

        let packet = Packet::unpack(&got.data).expect("unpack crossed packet");
        assert_eq!(packet.flags.packet_type, PacketType::Announce);
        assert_eq!(&packet.destination_hash, dest_hash.as_bytes());
    }

    /// The AX.25/KISS escape round-trip: a payload whose bytes (and whose
    /// resulting AX.25 frame) contain FEND/FESC must survive KISS escaping and
    /// AX.25 header strip intact.
    #[tokio::test]
    async fn ax25_round_trips_escaped_payload() {
        let (a, b) = tokio::io::duplex(64 * 1024);

        let (a_in_tx, _a_in_rx) = mpsc::channel(8);
        let (a_out_tx, a_out_rx) = mpsc::channel(8);
        let a_cfg = ax25_config("ax25-esc-A", b"AB1CD", 3);
        let a_counters = Arc::new(InterfaceCounters::new());
        tokio::spawn(async move {
            run_kiss_over_stream(&a_cfg, a, a_in_tx, a_out_rx, a_counters).await;
        });

        let (b_in_tx, mut b_in_rx) = mpsc::channel(8);
        let (_b_out_tx, b_out_rx) = mpsc::channel(8);
        let b_cfg = ax25_config("ax25-esc-B", b"AB1CD", 4);
        let b_counters = Arc::new(InterfaceCounters::new());
        tokio::spawn(async move {
            run_kiss_over_stream(&b_cfg, b, b_in_tx, b_out_rx, b_counters).await;
        });

        let payload = vec![FEND, 0x11, FESC, TFEND, TFESC, 0x00, FEND, 0xDB, 0x42];
        a_out_tx
            .send(OutgoingPacket {
                peer: None,
                data: payload.clone(),
                high_priority: false,
            })
            .await
            .expect("send into A");

        let got = tokio::time::timeout(Duration::from_secs(5), b_in_rx.recv())
            .await
            .expect("B receives within timeout")
            .expect("channel open");
        assert_eq!(
            got.data, payload,
            "escaped payload must survive the AX.25/KISS round-trip"
        );
    }
}
