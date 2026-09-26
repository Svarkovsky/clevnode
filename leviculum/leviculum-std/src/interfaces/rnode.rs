use crate::tokio_serial;
// RNode serial interface, detection, configuration, and data path
//
// Implements the full RNode lifecycle: detect → configure radio → validate →
// go online → bidirectional data → reconnect on failure → graceful shutdown.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use leviculum_core::framing::kiss::{self, KissDeframeResult, KissDeframer};
use leviculum_core::rnode;
use leviculum_core::transport::InterfaceId;
use rand_core::RngCore;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use super::{IncomingPacket, InterfaceCounters, InterfaceHandle, InterfaceInfo, OutgoingPacket};

// ---------------------------------------------------------------------------
// Named constants for serial protocol timing and buffers
// ---------------------------------------------------------------------------

/// Serial baud rate for all RNode devices
const SERIAL_BAUD_RATE: u32 = 115_200;
/// Device settle time after opening serial port
const DEVICE_SETTLE: Duration = Duration::from_secs(2);
/// Wait for device to process configuration
const CONFIG_PROCESS_WAIT: Duration = Duration::from_millis(250);
/// Final settle before starting I/O
const FINAL_SETTLE: Duration = Duration::from_millis(300);
/// Detection phase read timeout
const DETECT_TIMEOUT: Duration = Duration::from_millis(200);
/// Configuration validation read timeout
const VALIDATE_TIMEOUT: Duration = Duration::from_millis(2000);
/// Reconnect retry interval
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
/// Serial read buffer size (detection + validation phases)
const SERIAL_READ_BUF: usize = 256;
/// Serial read buffer size (I/O phase, larger for sustained throughput)
const IO_READ_BUF: usize = 1024;
/// Frequency confirmation tolerance (Hz)
const FREQ_TOLERANCE_HZ: u32 = 100;
/// Device reset notification marker
const DEVICE_RESET_MARKER: u8 = 0xF8;

/// Result of an RNode detection probe
#[derive(Debug)]
struct RNodeDetectResult {
    detected: bool,
    firmware_version: Option<(u8, u8)>,
    platform: Option<u8>,
    mcu: Option<u8>,
}

/// Errors from RNode serial operations
#[derive(Debug, thiserror::Error)]
pub(crate) enum RNodeError {
    #[error("serial port error: {0}")]
    SerialPort(String),
    #[error("device not detected")]
    NotDetected,
    #[error("firmware {0}.{1} below minimum {2}.{3}")]
    FirmwareTooOld(u8, u8, u8, u8),
    #[error("radio config mismatch: {0}")]
    RadioMismatch(String),
}

impl From<tokio_serial::Error> for RNodeError {
    fn from(e: tokio_serial::Error) -> Self {
        RNodeError::SerialPort(e.to_string())
    }
}

impl From<std::io::Error> for RNodeError {
    fn from(e: std::io::Error) -> Self {
        RNodeError::SerialPort(e.to_string())
    }
}

/// Radio parameters for RNode configuration
struct RadioParams {
    frequency: u32,
    bandwidth: u32,
    tx_power: u8,
    /// Whether `tx_power` came from the absent-key default
    /// (`rnode::resolve_tx_power`) rather than an explicit `txpower` line.
    ///
    /// The default asks for the board maximum — capped by the lawful ERP limit
    /// for the frequency (`rnode::lawful_erp_dbm`) — without probing what this
    /// board's maximum is, which the RNode firmware answers by clamping to its own
    /// ceiling and echoing the clamped value
    /// (`RNode_Firmware/RNode_Firmware.ino:861-879`) — 17 dBm on an SX127x
    /// board, `PA_MAX_OUTPUT` on an SX1262 with an external PA. Confirmation is
    /// otherwise an exact match, so without this flag every such board would
    /// fail startup with `RadioMismatch` instead of running at its maximum.
    /// An explicitly configured power keeps the strict check: it is a value the
    /// operator chose, and a board that cannot deliver it must say so.
    tx_power_derived: bool,
    sf: u8,
    cr: u8,
    st_alock: Option<u16>,
    lt_alock: Option<u16>,
}

/// Configuration for spawning an RNode interface
pub(crate) struct RNodeInterfaceConfig {
    pub id: InterfaceId,
    pub name: String,
    pub port_path: String,
    pub frequency: u32,
    pub bandwidth: u32,
    pub tx_power: u8,
    /// See [`RadioParams::tx_power_derived`].
    pub tx_power_derived: bool,
    pub sf: u8,
    pub cr: u8,
    pub st_alock: Option<u16>,
    pub lt_alock: Option<u16>,
    pub flow_control: bool,
    pub buffer_size: usize,
    pub reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
    /// TEST-ONLY range emulation: drop deframed hops=0 ingress frames
    /// (see [`super::test_drop_direct_ingress_frame`]).
    pub test_drop_direct_ingress: bool,
}

impl RNodeInterfaceConfig {
    fn radio_params(&self) -> RadioParams {
        RadioParams {
            frequency: self.frequency,
            bandwidth: self.bandwidth,
            tx_power: self.tx_power,
            tx_power_derived: self.tx_power_derived,
            sf: self.sf,
            cr: self.cr,
            st_alock: self.st_alock,
            lt_alock: self.lt_alock,
        }
    }
}

/// A framed packet queued for serial transmission
struct QueuedFrame {
    data: Vec<u8>,
    payload_len: u64,
    high_priority: bool,
}

/// Compute jitter ceiling from LoRa radio parameters.
///
/// The jitter window must exceed the maximum packet airtime so that two nodes
/// transmitting simultaneously have a chance to desynchronize. Uses 2x the
/// worst-case airtime (500-byte packet, CR=5), minimum 500ms for fast links.
/// No upper cap, slow links (SF10+) need wide jitter to avoid collisions
/// when airtime exceeds several seconds.
fn compute_jitter_max_ms(sf: u8, bandwidth_hz: u32) -> u64 {
    let bitrate = rnode::compute_bitrate(sf, 5, bandwidth_hz);
    if bitrate == 0 {
        return 500;
    }
    // 500 bytes * 8 bits = 4000 bits max Reticulum packet
    let airtime_ms = 4000u64 * 1000 / bitrate as u64;
    (airtime_ms * 2).max(500)
}

/// Default channel buffer size for RNode interfaces.
/// Smaller than TCP because LoRa bitrates are orders of magnitude lower.
pub(crate) const RNODE_DEFAULT_BUFFER_SIZE: usize = 64;

/// Maximum queued TX packets when flow control is active and device is busy.
/// Python uses an unbounded queue. Bounded to 64 here because at LoRa bitrates,
/// a full unbounded queue contains minutes-old stale packets. Drop oldest,
/// counted and named by `RNODE_TX_QUEUE_DROP` (drops are loud).
const FLOW_CONTROL_QUEUE_LIMIT: usize = 64;

/// How long the CMD_READY flow-control gate may hold queued frames before the
/// io task reports it. One firmware channel-stat cadence: the
/// RNode emits CMD_STAT_CHTM every ~2 s, while a READY normally follows a TX
/// within one packet airtime — sub-second at every supported rate. A gate
/// still closed after a full stat cadence is no longer ordinary airtime wait;
/// it is the duty-lock shape (no TX completion ⇒ no READY,
/// `RNode_Firmware.ino:1624`).
const TX_GATED_EVENT_AFTER: Duration = Duration::from_secs(2);

/// Repeat cadence for `RNODE_TX_GATED` while the gate stays closed. Bounded
/// so an hour-long duty lock produces hundreds of log lines, not one per
/// event-loop iteration.
const TX_GATED_EVENT_REPEAT: Duration = Duration::from_secs(10);

/// The CMD_READY queue-state query. The firmware dispatches its
/// `command == CMD_READY` branch only on a byte *following* the command
/// byte (`serial_callback`, `RNode_Firmware.ino:765ff`: the first in-frame
/// byte just latches `command`), so the query must carry one payload byte;
/// its value is ignored. The response echoes CMD_READY with 0x01 (queue
/// not full) or 0x00 (queue full) — `RNode_Firmware.ino:1003-1008`,
/// `Utilities.h:1157,1164`. Those two indicate functions have no other
/// call sites: CMD_READY is strictly a query/response exchange, never a
/// spontaneous signal (the flowval leg-B deadlock, 2026-08-21, came from
/// waiting for one).
const READY_QUERY_FRAME: [u8; 4] = [kiss::FEND, rnode::CMD_READY, 0x00, kiss::FEND];

/// Ceiling for the CMD_READY re-query backoff while the gate is closed.
/// Under a duty lock the firmware queue stays full for minutes, and the
/// poll shares the serial line the modem also uses to deliver RX frames —
/// so the cadence must flatten out. 2 s (one CHTM stat cadence) bounds the
/// chatter to a 4-byte frame every 2 s while capping worst-case
/// gate-release latency at the same interval the firmware already uses
/// for its own periodic reporting.
const READY_POLL_MAX: Duration = Duration::from_secs(2);

/// First re-query delay after a TX or an unanswered/negative CMD_READY
/// query: one full-size packet airtime at the configured PHY. The queue
/// state can only change when a TX completes, and completing a queued
/// full-size frame takes at least this long — polling faster cannot
/// observe a transition, it only spends serial bandwidth. Subsequent
/// re-queries double the delay up to [`READY_POLL_MAX`].
fn ready_poll_initial(sf: u8, cr: u8, bandwidth_hz: u32) -> Duration {
    let bitrate = rnode::compute_bitrate(sf, cr, bandwidth_hz);
    if bitrate == 0 {
        return READY_POLL_MAX;
    }
    let airtime_ms = (rnode::HW_MTU as u64 * 8 * 1000) / bitrate as u64;
    Duration::from_millis(airtime_ms.max(1)).min(READY_POLL_MAX)
}

// ---------------------------------------------------------------------------
// Configuration (includes detection)
// ---------------------------------------------------------------------------

/// Open + configure a serial RNode in one call. Retained for the hardware
/// smoke test; the production path uses [`open_serial_port`] +
/// [`configure_stream`] separately via the reconnect loop.
#[cfg(test)]
async fn configure_rnode(
    port_path: &str,
    radio: &RadioParams,
) -> Result<(tokio_serial::SerialStream, RNodeDetectResult), RNodeError> {
    let mut port = open_serial_port(port_path).await?;
    let detect_result = configure_stream(&mut port, radio, port_path).await?;
    Ok((port, detect_result))
}

/// Open the serial port (115200/8N1, no flow control) and wait for the device
/// to settle. Serial-specific: opening the port toggles DTR, which reboots many
/// RNode devices, hence the settle delay before any protocol I/O.
async fn open_serial_port(port_path: &str) -> Result<tokio_serial::SerialStream, RNodeError> {
    let builder = tokio_serial::new(port_path, SERIAL_BAUD_RATE)
        .data_bits(tokio_serial::DataBits::Eight)
        .stop_bits(tokio_serial::StopBits::One)
        .parity(tokio_serial::Parity::None)
        .flow_control(tokio_serial::FlowControl::None);

    let port = tokio_serial::SerialStream::open(&builder)?;

    // Wait for device to settle (reboot-on-open)
    tokio::time::sleep(DEVICE_SETTLE).await;

    Ok(port)
}

/// Detect the RNode, validate firmware, configure the radio, and confirm —
/// over an already-open, settled byte channel.
///
/// Carrier-agnostic: works on any `AsyncRead + AsyncWrite` stream, whether a
/// `tokio_serial::SerialStream` or a host-supplied channel (USB-CDC, BLE GATT,
/// mock pipe). The far end speaks RNode KISS regardless of substrate.
///
/// Sequence matches Python `RNodeInterface.configure_device()` from the detect
/// step onward (the port-open + device-settle step is the caller's, since it is
/// substrate-specific):
/// 1. Detect + validate firmware >= 1.52
/// 2. Send config commands: frequency, bandwidth, txpower, sf, cr, [st_alock], [lt_alock], radio ON
/// 3. Sleep 250ms, read confirmation frames
/// 4. Validate: frequency within 100 Hz, others exact match
/// 5. Sleep 300ms
async fn configure_stream<S>(
    port: &mut S,
    radio: &RadioParams,
    name: &str,
) -> Result<RNodeDetectResult, RNodeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // --- Detection phase ---
    let detect_result = detect_on_port(port).await?;

    // Validate firmware version
    if let Some((major, minor)) = detect_result.firmware_version {
        if !rnode::validate_firmware(major, minor) {
            return Err(RNodeError::FirmwareTooOld(
                major,
                minor,
                rnode::REQUIRED_FW_MAJ,
                rnode::REQUIRED_FW_MIN,
            ));
        }
    } else {
        return Err(RNodeError::NotDetected);
    }

    // --- Configuration phase ---
    rnode::validate_config(
        radio.frequency,
        radio.bandwidth,
        radio.tx_power,
        radio.sf,
        radio.cr,
    )
    .map_err(|e| RNodeError::RadioMismatch(e.to_string()))?;
    send_radio_config(port, radio).await?;

    // Wait for device to process configuration
    tokio::time::sleep(CONFIG_PROCESS_WAIT).await;

    // Read and validate confirmation frames
    validate_radio_config(port, radio, name).await?;

    // Final settle
    tokio::time::sleep(FINAL_SETTLE).await;

    Ok(detect_result)
}

/// Read KISS frames from the serial port until the deadline, calling `handler`
/// for each successfully deframed frame.
async fn read_frames_until_deadline<S>(
    port: &mut S,
    timeout: Duration,
    mut handler: impl FnMut(u8, &[u8]),
) -> Result<(), RNodeError>
where
    S: AsyncRead + Unpin,
{
    let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
    let mut buf = [0u8; SERIAL_READ_BUF];
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, port.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                tracing::info!("rnode read {} bytes: {:02x?}", n, &buf[..n.min(32)]);
                for frame in deframer.process(&buf[..n]) {
                    if let KissDeframeResult::Frame { command, payload } = frame {
                        tracing::debug!(
                            "rnode KISS frame: cmd=0x{:02x} len={}",
                            command,
                            payload.len()
                        );
                        handler(command, &payload);
                    }
                }
            }
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                tracing::debug!("rnode read timeout ({:?} remaining)", remaining);
                break;
            }
        }
    }
    Ok(())
}

/// Send detect query and parse response frames from an open port.
async fn detect_on_port<S>(port: &mut S) -> Result<RNodeDetectResult, RNodeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let query = rnode::build_detect_query();
    port.write_all(&query).await?;

    let mut result = RNodeDetectResult {
        detected: false,
        firmware_version: None,
        platform: None,
        mcu: None,
    };

    read_frames_until_deadline(port, DETECT_TIMEOUT, |command, payload| match command {
        rnode::CMD_DETECT if payload.first() == Some(&rnode::DETECT_RESP) => {
            result.detected = true;
        }
        rnode::CMD_FW_VERSION => {
            result.firmware_version = rnode::decode_firmware_version(payload);
        }
        rnode::CMD_PLATFORM => {
            result.platform = payload.first().copied();
        }
        rnode::CMD_MCU => {
            result.mcu = payload.first().copied();
        }
        _ => {}
    })
    .await?;

    if !result.detected {
        return Err(RNodeError::NotDetected);
    }

    Ok(result)
}

/// Send radio configuration commands to the RNode.
async fn send_radio_config<S>(port: &mut S, radio: &RadioParams) -> Result<(), RNodeError>
where
    S: AsyncWrite + Unpin,
{
    let mut config_bytes = Vec::with_capacity(64);
    // Idle the radio before touching modulation parameters. The firmware's
    // setters are no-ops while the modem is offline (`Utilities.h`
    // `setBandwidth`/`setFrequency`/`setSpreadingFactor`/... all guard on
    // `radio_online`); they only store `lora_*`, and `startRadio()` applies
    // the whole stored set in one go. Reconfiguring a *running* modem instead
    // pokes registers underneath an active receive, which most boards shrug
    // off and at least one does not — it comes up mute while every readback
    // still reports a healthy modem.
    config_bytes.extend_from_slice(&rnode::build_set_radio_state(rnode::RADIO_STATE_OFF));
    config_bytes.extend_from_slice(&rnode::build_set_frequency(radio.frequency));
    config_bytes.extend_from_slice(&rnode::build_set_bandwidth(radio.bandwidth));
    config_bytes.extend_from_slice(&rnode::build_set_txpower(radio.tx_power));
    config_bytes.extend_from_slice(&rnode::build_set_sf(radio.sf));
    config_bytes.extend_from_slice(&rnode::build_set_cr(radio.cr));
    if let Some(st) = radio.st_alock {
        config_bytes.extend_from_slice(&rnode::build_set_st_alock(st));
    }
    if let Some(lt) = radio.lt_alock {
        config_bytes.extend_from_slice(&rnode::build_set_lt_alock(lt));
    }
    config_bytes.extend_from_slice(&rnode::build_set_radio_state(rnode::RADIO_STATE_ON));

    tracing::info!("rnode: sending {} config bytes", config_bytes.len());
    port.write_all(&config_bytes).await?;
    port.flush().await?;
    tracing::info!("rnode: config sent and flushed");
    Ok(())
}

/// Read confirmation frames and validate they match the requested config.
async fn validate_radio_config<S>(
    port: &mut S,
    radio: &RadioParams,
    name: &str,
) -> Result<(), RNodeError>
where
    S: AsyncRead + Unpin,
{
    let mut confirmed_freq: Option<u32> = None;
    let mut confirmed_bw: Option<u32> = None;
    let mut confirmed_txp: Option<u8> = None;
    let mut confirmed_sf: Option<u8> = None;
    let mut confirmed_cr: Option<u8> = None;
    let mut confirmed_radio_state: Option<u8> = None;

    read_frames_until_deadline(port, VALIDATE_TIMEOUT, |command, payload| match command {
        rnode::CMD_FREQUENCY if payload.len() >= 4 => {
            confirmed_freq = Some(u32::from_be_bytes([
                payload[0], payload[1], payload[2], payload[3],
            ]));
        }
        rnode::CMD_BANDWIDTH if payload.len() >= 4 => {
            confirmed_bw = Some(u32::from_be_bytes([
                payload[0], payload[1], payload[2], payload[3],
            ]));
        }
        rnode::CMD_TXPOWER if !payload.is_empty() => {
            confirmed_txp = Some(payload[0]);
        }
        rnode::CMD_SF if !payload.is_empty() => {
            confirmed_sf = Some(payload[0]);
        }
        rnode::CMD_CR if !payload.is_empty() => {
            confirmed_cr = Some(payload[0]);
        }
        rnode::CMD_RADIO_STATE if !payload.is_empty() => {
            confirmed_radio_state = Some(payload[0]);
        }
        _ => {}
    })
    .await?;

    // Log warnings for missing confirmations to aid debugging
    if confirmed_freq.is_none() {
        tracing::debug!("{}: no frequency confirmation received", name);
    }
    if confirmed_bw.is_none() {
        tracing::debug!("{}: no bandwidth confirmation received", name);
    }
    if confirmed_txp.is_none() {
        tracing::debug!("{}: no tx_power confirmation received", name);
    }
    if confirmed_sf.is_none() {
        tracing::debug!("{}: no spreading factor confirmation received", name);
    }
    if confirmed_cr.is_none() {
        tracing::debug!("{}: no coding rate confirmation received", name);
    }
    if confirmed_radio_state.is_none() {
        tracing::debug!("{}: no radio_state confirmation received", name);
    }

    if let Some(cf) = confirmed_freq {
        if cf.abs_diff(radio.frequency) > FREQ_TOLERANCE_HZ {
            return Err(RNodeError::RadioMismatch(format!(
                "frequency: requested {} Hz, got {} Hz",
                radio.frequency, cf
            )));
        }
    }
    if let Some(cb) = confirmed_bw {
        if cb != radio.bandwidth {
            return Err(RNodeError::RadioMismatch(format!(
                "bandwidth: requested {} Hz, got {} Hz",
                radio.bandwidth, cb
            )));
        }
    }
    if let Some(ct) = confirmed_txp {
        // A derived board-maximum request is allowed to come back clamped
        // DOWN: that is the board reporting its own ceiling, which is what the
        // absent-key default asked for in the first place. Coming back HIGHER
        // than requested is a mismatch either way — nothing may transmit above
        // what was asked for.
        if ct < radio.tx_power && radio.tx_power_derived {
            tracing::info!(
                "{}: board maximum is {} dBm, below the {} dBm default request; using {} dBm",
                name,
                ct,
                radio.tx_power,
                ct
            );
        } else if ct != radio.tx_power {
            return Err(RNodeError::RadioMismatch(format!(
                "tx_power: requested {} dBm, got {} dBm",
                radio.tx_power, ct
            )));
        }
    }
    if let Some(cs) = confirmed_sf {
        if cs != radio.sf {
            return Err(RNodeError::RadioMismatch(format!(
                "sf: requested {}, got {}",
                radio.sf, cs
            )));
        }
    }
    if let Some(cc) = confirmed_cr {
        if cc != radio.cr {
            return Err(RNodeError::RadioMismatch(format!(
                "cr: requested {}, got {}",
                radio.cr, cc
            )));
        }
    }
    if confirmed_radio_state == Some(rnode::RADIO_STATE_OFF) {
        return Err(RNodeError::RadioMismatch(
            "radio did not turn on".to_string(),
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Radio stats (Codeberg #25)
// ---------------------------------------------------------------------------

/// Parse an RNode `CMD_STAT_*` frame and fold the values into the interface's
/// shared radio stats (Codeberg #25).
///
/// Decoding uses the shared `leviculum_core::rnode` decoders; the scaling and
/// clamping applied here match Python `RNodeInterface.process_incoming`
/// (RNodeInterface.py:878-1066) so the stored values carry the same units
/// Python exposes through `get_interface_stats`:
///
/// - RSSI: dBm (`raw - 157`), stored as `last_rssi`.
/// - SNR: dB (`signed raw * 0.25`), stored as `last_snr`.
/// - CHTM: airtime/channel-load are `raw_u16 / 100.0` percent; `noise_floor`
///   is dBm (only present on single-interface 11-byte frames).
/// - BAT: `(state, percent)`.
/// - TEMP: Celsius (`raw - 120`), clamped to `[-30, 90]`, else `None`.
///
/// Returns `true` if `command` was a recognised stat frame. Shared by the I/O
/// task and unit tests so the parse/state path is exercised without a serial
/// port.
fn apply_radio_stat(counters: &InterfaceCounters, command: u8, payload: &[u8]) -> bool {
    match command {
        rnode::CMD_STAT_RSSI => {
            if let Some(rssi) = rnode::decode_rssi(payload) {
                counters.update_radio(|r| r.last_rssi = Some(rssi));
            }
        }
        rnode::CMD_STAT_SNR => {
            if let Some(raw) = rnode::decode_snr(payload) {
                counters.update_radio(|r| r.last_snr = Some(raw as f64 * 0.25));
            }
        }
        rnode::CMD_STAT_CHTM => {
            if let Some(cs) = rnode::decode_channel_stats(payload) {
                counters.update_radio(|r| {
                    r.airtime_short = cs.airtime_short as f64 / 100.0;
                    r.airtime_long = cs.airtime_long as f64 / 100.0;
                    r.channel_load_short = cs.channel_load_short as f64 / 100.0;
                    r.channel_load_long = cs.channel_load_long as f64 / 100.0;
                    if let Some(nf) = cs.noise_floor {
                        r.noise_floor = Some(nf);
                    }
                });
            }
        }
        rnode::CMD_STAT_BAT => {
            if let Some((state, percent)) = rnode::decode_battery(payload) {
                counters.update_radio(|r| {
                    r.battery_state = state;
                    r.battery_percent = percent;
                });
            }
        }
        rnode::CMD_STAT_TEMP => {
            if let Some(temp) = rnode::decode_temperature(payload) {
                let clamped = (-30..=90).contains(&temp).then_some(temp);
                counters.update_radio(|r| r.cpu_temp = clamped);
            }
        }
        _ => return false,
    }
    true
}

/// The ` rssi=<dBm> snr=<dB>` suffix a data frame's RX log lines carry
/// (Codeberg #364: the air sniffer could not say whether a lost frame was
/// weaker).
///
/// The firmware indicates the frame's RSSI and SNR to the host BEFORE the
/// data frame itself, on every MCU variant (RNode_Firmware.ino:454-458 and
/// :495-499 AVR, :1668-1670 ESP32, :1689-1691 nRF52 — always
/// `kiss_indicate_stat_rssi(); kiss_indicate_stat_snr();
/// kiss_write_packet();`), so at `CMD_DATA` time the most recently stored
/// stat values are this frame's own signal report. That last-seen pairing is
/// the reference's: Python keeps `r_stat_rssi`/`r_stat_snr` from the
/// preceding stat frames (RNodeInterface.py:877-880, the SNR a signed byte
/// scaled by 0.25) and reads them for the frame that follows. Key names
/// match the firmware's `[LORA] RX` line. Empty until the first stat frame —
/// a stream that never carried stats (a mock, an older firmware) logs the
/// bare line rather than an invented value.
fn signal_suffix(counters: &InterfaceCounters) -> String {
    use std::fmt::Write as _;
    let mut suffix = String::new();
    if let Some(radio) = counters.radio_stats() {
        if let Some(rssi) = radio.last_rssi {
            let _ = write!(suffix, " rssi={rssi}");
        }
        if let Some(snr) = radio.last_snr {
            let _ = write!(suffix, " snr={snr}");
        }
    }
    suffix
}

// ---------------------------------------------------------------------------
// I/O task
// ---------------------------------------------------------------------------

/// Count and name the frames a return path leaves behind (Codeberg #316).
///
/// The io task's send queue is task-local: whatever it still holds when the
/// task returns is gone, while frames still sitting in the mpsc channel are
/// inherited by the reconnected task. Losing the held ones is a legitimate
/// deviation from Python's keep-across-reconnect `packet_queue`; losing them
/// *silently* is a black hole — under a long duty lock the queue is full
/// precisely when a port bounce is most likely.
///
/// One summary event per return path, never one per frame: a reconnect must
/// not be able to produce 64 log lines in one moment. `depth` is therefore
/// always 0 — nothing survives this call.
fn abandon_send_queue(
    name: &str,
    counters: &InterfaceCounters,
    send_queue: &mut VecDeque<QueuedFrame>,
    reason: &'static str,
) {
    let abandoned = send_queue.len();
    if abandoned == 0 {
        return;
    }
    let abandoned_bytes: u64 = send_queue.iter().map(|f| f.payload_len).sum();
    send_queue.clear();
    counters
        .tx_queue_drops
        .fetch_add(abandoned as usize, std::sync::atomic::Ordering::Relaxed);
    counters
        .tx_dropped_bytes
        .fetch_add(abandoned_bytes as usize, std::sync::atomic::Ordering::Relaxed);
    tracing::warn!(
        event = "RNODE_TX_QUEUE_DROP",
        iface = %name,
        frames = abandoned,
        depth = 0,
        reason = reason,
    );
}

/// [`abandon_send_queue`] for the multi-vport loop, whose shared queue is
/// just as task-local.
///
/// The frames are vport-tagged, so each abandoned frame is counted on its
/// owning vport's counters, while the single summary event names the
/// physical interface: the port that dropped is a property of the shared
/// line, not of any one vport.
fn abandon_multi_send_queue(
    name: &str,
    vports: &[VportRuntime],
    send_queue: &mut VecDeque<(usize, Vec<u8>)>,
    reason: &'static str,
) {
    let abandoned = send_queue.len();
    if abandoned == 0 {
        return;
    }
    for (subint, frame) in send_queue.drain(..) {
        vports[subint]
            .counters
            .tx_queue_drops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        vports[subint]
            .counters
            .tx_dropped_bytes
            .fetch_add(frame.len() as usize, std::sync::atomic::Ordering::Relaxed);
    }
    tracing::warn!(
        event = "RNODE_TX_QUEUE_DROP",
        iface = %name,
        frames = abandoned,
        depth = 0,
        reason = reason,
    );
}

/// Deregister one vport whose logical interface is gone.
///
/// The granularity is the point (#283): only this vport's queued frames are
/// abandoned and only this vport stops being routed to. The shared serial port
/// and every other vport on it keep running, so a single torn-down logical
/// interface no longer bounces the physical radio.
fn deregister_vport(
    name: &str,
    vports: &[VportRuntime],
    send_queue: &mut VecDeque<(usize, Vec<u8>)>,
    dead: &mut [bool],
    subint: usize,
) {
    if dead[subint] {
        return;
    }
    dead[subint] = true;

    let mut abandoned = 0usize;
    send_queue.retain(|(queued, frame)| {
        if *queued != subint {
            return true;
        }
        abandoned += 1;
        vports[subint]
            .counters
            .tx_queue_drops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        vports[subint]
            .counters
            .tx_dropped_bytes
            .fetch_add(frame.len() as usize, std::sync::atomic::Ordering::Relaxed);
        false
    });

    tracing::warn!(
        event = "RNODE_VPORT_DEREGISTERED",
        iface = %name,
        vport_iface = %vports[subint].name,
        vport = vports[subint].vport,
        frames = abandoned,
        reason = "incoming_closed",
    );
}

/// Bidirectional I/O loop for a configured RNode.
///
/// Returns the `outgoing_rx` on disconnect so the reconnect wrapper can
/// reuse the same channel (matching TCP interface pattern).
///
/// Send-side jitter: packets are not sent immediately. The first packet after
/// idle gets a random 0–500ms delay (desynchronizes rebroadcasts from multiple
/// nodes). Subsequent queued packets use a fixed 50ms spacing to avoid serial
/// buffer overrun. RNode firmware CSMA handles radio-level collision avoidance.
#[allow(clippy::too_many_arguments)]
async fn rnode_io_task<S>(
    name: String,
    mut port: S,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
    flow_control: bool,
    jitter_max_ms: u64,
    bandwidth_hz: u32,
    sf: u8,
    cr: u8,
    drop_direct_ingress: bool,
) -> mpsc::Receiver<OutgoingPacket>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    super::log_direct_ingress_filter_armed(drop_direct_ingress, &name);
    let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
    let mut buf = [0u8; IO_READ_BUF];
    // The gate starts open: before the first TX there is no queue state
    // worth asking about, and the firmware never volunteers CMD_READY
    // (RNode_Firmware.ino:1003-1008 answers only a host query), so a
    // closed initial gate could never open.
    let mut interface_ready = true;
    let mut send_queue: VecDeque<QueuedFrame> = VecDeque::new();
    let mut send_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let mut timer_ready = false;

    // Gate 2 reopen state: CMD_READY is a query protocol, not a courtesy.
    // After every TX (flow_control on) the gate closes and we ask the
    // firmware whether its queue has room; a 0x01 response reopens it. A
    // 0x00 response — or a response that never comes, e.g. lost on a noisy
    // line — leads to a re-query when `ready_query_timer` fires: the timer
    // is armed at query time, so a lost response degrades into the same
    // bounded re-poll instead of a stuck gate. The delay starts at one
    // packet airtime and doubles up to READY_POLL_MAX (see the constants
    // above for the justification).
    let ready_poll_start = ready_poll_initial(sf, cr, bandwidth_hz);
    let mut ready_poll = ready_poll_start;
    let mut ready_query_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;

    // Duty-lock visibility: when the CMD_READY gate (Gate 2 below)
    // holds queued frames, say so. The firmware's duty lock produces exactly
    // this shape — the queue stays full, every poll answers 0x00, the gate
    // stays closed — and it used
    // to be invisible from the host. `gate_blocked_since` starts when frames
    // are held and the gate is closed; `gate_event_timer` fires
    // RNODE_TX_GATED after TX_GATED_EVENT_AFTER, then every
    // TX_GATED_EVENT_REPEAT; `gate_announced` pairs the eventual
    // RNODE_TX_RELEASED with it.
    let mut gate_blocked_since: Option<tokio::time::Instant> = None;
    let mut gate_event_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let mut gate_announced = false;

    // Periodic heartbeat: send CMD_DETECT every 5 minutes to keep the
    // serial link alive and verify the RNode firmware is responsive.
    // This does NOT transmit over LoRa, it's a serial-only ping.
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);
    let mut heartbeat_timer = Box::pin(tokio::time::sleep(HEARTBEAT_INTERVAL));
    let mut heartbeat_pending = false;

    loop {
        tokio::select! {
            // Branch 1: Read from serial port
            result = port.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        tracing::warn!("{}: serial port EOF", name);
                        abandon_send_queue(&name, &counters, &mut send_queue, "serial_eof");
                        return outgoing_rx;
                    }
                    Ok(n) => {
                        let frames = deframer.process(&buf[..n]);
                        for frame in frames {
                            if let KissDeframeResult::Frame { command, payload } = frame {
                                match command {
                                    rnode::CMD_DATA => {
                                        let signal = signal_suffix(&counters);
                                        tracing::debug!(
                                            "{}: RX {} bytes from radio{}",
                                            name,
                                            payload.len(),
                                            signal
                                        );
                                        // TEST-ONLY range emulation: an
                                        // out-of-range frame was never heard,
                                        // so it is dropped before any counter
                                        // or trace event that the delivery
                                        // analysis reads.
                                        if super::test_drop_direct_ingress_frame(
                                            drop_direct_ingress, &name, &payload, &counters,
                                        ) {
                                            continue;
                                        }
                                        // Bug #25 capture-compare: structured
                                        // event at the RNode → host serial
                                        // boundary. Mirrors `LORA_TX` on the
                                        // send side; together they let the
                                        // analysis align TX on one node with
                                        // RX on the other.
                                        tracing::debug!(
                                            target: "leviculum_std::interfaces::rnode::rx_trace",
                                            "LORA_RX iface={name} len={}{signal}",
                                            payload.len()
                                        );
                                        counters.rx_bytes.fetch_add(
                                            payload.len() as usize,
                                            std::sync::atomic::Ordering::Relaxed,
                                        );
                                        let pkt = IncomingPacket { data: payload.to_vec() };
                                        if incoming_tx.send(pkt).await.is_err() {
                                            // Event loop shut down
                                            send_goodbye(&mut port, &name).await;
                                            abandon_send_queue(
                                                &name, &counters, &mut send_queue,
                                                "incoming_closed",
                                            );
                                            return outgoing_rx;
                                        }
                                    }
                                    rnode::CMD_READY => {
                                        // Response to our post-TX/re-poll query — the
                                        // firmware never sends CMD_READY unsolicited.
                                        // 0x01: queue not full, the gate reopens.
                                        // 0x00: still full; the timer armed with the
                                        // query re-asks on its bounded backoff.
                                        if flow_control && payload.first() == Some(&0x01) {
                                            tracing::debug!(
                                                "{}: CMD_READY answer: queue not full",
                                                name
                                            );
                                            interface_ready = true;
                                            ready_query_timer = None;
                                            ready_poll = ready_poll_start;
                                        } else if flow_control {
                                            tracing::debug!(
                                                "{}: CMD_READY answer: queue full",
                                                name
                                            );
                                        }
                                    }
                                    rnode::CMD_DETECT => {
                                        if payload.first() == Some(&rnode::DETECT_RESP)
                                            && heartbeat_pending
                                        {
                                            tracing::debug!("{}: heartbeat OK", name);
                                            heartbeat_pending = false;
                                        }
                                    }
                                    rnode::CMD_RESET => {
                                        if payload.first() == Some(&DEVICE_RESET_MARKER) {
                                            tracing::warn!("{}: device reset (0xF8)", name);
                                            abandon_send_queue(
                                                &name, &counters, &mut send_queue,
                                                "device_reset",
                                            );
                                            return outgoing_rx;
                                        }
                                    }
                                    rnode::CMD_ERROR => {
                                        let Some(code) = payload.first().copied() else {
                                            tracing::warn!("{}: CMD_ERROR with empty payload", name);
                                            continue;
                                        };
                                        match code {
                                            rnode::ERROR_INITRADIO => {
                                                tracing::error!("{}: radio init failed", name);
                                                abandon_send_queue(
                                                    &name, &counters, &mut send_queue,
                                                    "error_initradio",
                                                );
                                                return outgoing_rx;
                                            }
                                            rnode::ERROR_TXFAILED => {
                                                tracing::error!("{}: TX failed", name);
                                                abandon_send_queue(
                                                    &name, &counters, &mut send_queue,
                                                    "error_txfailed",
                                                );
                                                return outgoing_rx;
                                            }
                                            rnode::ERROR_EEPROM_LOCKED => {
                                                tracing::error!("{}: EEPROM locked", name);
                                            }
                                            rnode::ERROR_QUEUE_FULL => {
                                                tracing::warn!("{}: device TX queue full", name);
                                            }
                                            rnode::ERROR_MEMORY_LOW => {
                                                tracing::warn!("{}: device memory low", name);
                                            }
                                            rnode::ERROR_MODEM_TIMEOUT => {
                                                tracing::error!("{}: modem timeout", name);
                                                abandon_send_queue(
                                                    &name, &counters, &mut send_queue,
                                                    "error_modem_timeout",
                                                );
                                                return outgoing_rx;
                                            }
                                            _ => {
                                                tracing::warn!(
                                                    "{}: unknown device error 0x{:02X}",
                                                    name, code
                                                );
                                            }
                                        }
                                    }
                                    // Bug #25 investigation telemetry: explicit parsers
                                    // for the two CSMA-related stat frames the firmware
                                    // emits unsolicited. Structured events under the
                                    // `leviculum_std::interfaces::rnode::csma_probe`
                                    // tracing target let the debugger correlate
                                    // firmware CSMA state with on-air TX behaviour.
                                    // Measurement-only; no TX-path change.
                                    rnode::CMD_STAT_CSMA if payload.len() >= 3 => {
                                        let cw_band = payload[0];
                                        let cw_min = payload[1];
                                        let cw_max = payload[2];
                                        tracing::debug!(
                                            target: "leviculum_std::interfaces::rnode::csma_probe",
                                            "CSMA_STAT iface={name} cw_band={cw_band} \
                                             cw_min={cw_min} cw_max={cw_max}"
                                        );
                                    }
                                    rnode::CMD_STAT_PHYPRM => {
                                        tracing::debug!(
                                            target: "leviculum_std::interfaces::rnode::csma_probe",
                                            "CSMA_PHY_RAW iface={name} payload_len={} bytes={:?}",
                                            payload.len(), payload
                                        );
                                        if payload.len() >= 12 {
                                            let symbol_time_ms =
                                                u16::from_be_bytes([payload[0], payload[1]]) as f32
                                                    / 1000.0;
                                            let symbol_rate =
                                                u16::from_be_bytes([payload[2], payload[3]]);
                                            let preamble_symbols =
                                                u16::from_be_bytes([payload[4], payload[5]]);
                                            let preamble_time_ms =
                                                u16::from_be_bytes([payload[6], payload[7]]);
                                            let csma_slot_time_ms =
                                                u16::from_be_bytes([payload[8], payload[9]]);
                                            let csma_difs_ms =
                                                u16::from_be_bytes([payload[10], payload[11]]);
                                            tracing::debug!(
                                                target: "leviculum_std::interfaces::rnode::csma_probe",
                                                "CSMA_PHY iface={name} symbol_time_ms={symbol_time_ms:.3} \
                                                 symbol_rate={symbol_rate} preamble_symbols={preamble_symbols} \
                                                 preamble_time_ms={preamble_time_ms} \
                                                 csma_slot_time_ms={csma_slot_time_ms} \
                                                 csma_difs_ms={csma_difs_ms}"
                                            );
                                        }
                                    }
                                    // Radio statistics (Codeberg #25): parse and
                                    // store on the shared counters so `interface_stats`
                                    // surfaces the RNode radio rows (airtime, channel
                                    // load, noise floor, temperature, battery) to
                                    // rnstatus/lnstatus. Field names/units mirror
                                    // Python RNodeInterface's `r_*` attributes.
                                    cmd @ (rnode::CMD_STAT_RSSI
                                    | rnode::CMD_STAT_SNR
                                    | rnode::CMD_STAT_CHTM
                                    | rnode::CMD_STAT_BAT
                                    | rnode::CMD_STAT_TEMP) => {
                                        apply_radio_stat(&counters, cmd, &payload);
                                    }
                                    _ => {
                                        tracing::trace!(
                                            "{}: unhandled cmd 0x{:02X}",
                                            name, command
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("{}: serial read error: {}", name, e);
                        abandon_send_queue(&name, &counters, &mut send_queue, "serial_read_error");
                        return outgoing_rx;
                    }
                }
            }

            // Branch 2: Outgoing packet from driver → enqueue with jitter
            recv = outgoing_rx.recv() => {
                match recv {
                    Some(pkt) => {
                        let frame = rnode::build_data_frame(&pkt.data);
                        let high_priority = pkt.high_priority;
                        if send_queue.len() >= FLOW_CONTROL_QUEUE_LIMIT {
                            if let Some(dropped) = send_queue.pop_front() {
                                // A dropped frame must be loud —
                                // counted and catalogued, never only a log
                                // line. A silent drop is how lora_window_ab's
                                // third transfer vanished for six minutes.
                                counters
                                    .tx_queue_drops
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                counters.tx_dropped_bytes.fetch_add(
                                    dropped.payload_len as usize,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                tracing::warn!(
                                    event = "RNODE_TX_QUEUE_DROP",
                                    iface = %name,
                                    len = dropped.payload_len as usize,
                                    depth = send_queue.len(),
                                    reason = "queue_full",
                                );
                            }
                        }
                        let queued = QueuedFrame {
                            data: frame,
                            payload_len: pkt.data.len() as u64,
                            high_priority,
                        };
                        if high_priority {
                            // Insert before the first non-high-priority packet
                            let pos = send_queue
                                .iter()
                                .position(|f| !f.high_priority)
                                .unwrap_or(send_queue.len());
                            send_queue.insert(pos, queued);
                            tracing::debug!(
                                "{}: send queue: {} packets (priority insert at {})",
                                name, send_queue.len(), pos
                            );
                        } else {
                            send_queue.push_back(queued);
                        }
                        // High-priority packet at front of queue: bypass initial jitter
                        // ONLY if no CSMA spacing timer is active. The jitter timer
                        // desynchronizes announce rebroadcasts, directed traffic
                        // (proofs, link requests, data) should not wait for that.
                        // But the CSMA spacing timer (set after a TX) must NOT be
                        // bypassed, it ensures the firmware queue stays at depth 1
                        // so flush_queue() sends one frame per CSMA contest.
                        if high_priority
                            && send_queue.front().map(|f| f.high_priority).unwrap_or(false)
                            && send_timer.is_none()
                        {
                            timer_ready = true;
                            tracing::debug!(
                                "{}: send queue: {} packets (priority bypass jitter)",
                                name, send_queue.len()
                            );
                        } else if send_timer.is_none() && !timer_ready {
                            let delay = rand_core::OsRng.next_u64() % jitter_max_ms;
                            tracing::debug!(
                                "{}: send queue: {} packets, jitter {}ms",
                                name, send_queue.len(), delay
                            );
                            send_timer = Some(Box::pin(
                                tokio::time::sleep(Duration::from_millis(delay))
                            ));
                        }
                    }
                    None => {
                        // Event loop shut down
                        send_goodbye(&mut port, &name).await;
                        abandon_send_queue(&name, &counters, &mut send_queue, "outgoing_closed");
                        return outgoing_rx;
                    }
                }
            }

            // Branch 3: Send timer fires
            _ = async {
                if let Some(ref mut timer) = send_timer {
                    timer.await;
                }
            }, if send_timer.is_some() => {
                send_timer = None;
                timer_ready = true;
            }

            // Branch 3b: flow-control gate held past the reporting threshold
            // Warn level: an engaged duty lock is an operational
            // condition the operator must be able to see.
            _ = async {
                if let Some(ref mut timer) = gate_event_timer {
                    timer.await;
                }
            }, if gate_event_timer.is_some() => {
                let held_ms = gate_blocked_since
                    .map(|s| s.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                tracing::warn!(
                    event = "RNODE_TX_GATED",
                    iface = %name,
                    held_ms = held_ms,
                    depth = send_queue.len(),
                );
                gate_announced = true;
                gate_event_timer = Some(Box::pin(tokio::time::sleep(TX_GATED_EVENT_REPEAT)));
            }

            // Branch 3c: no queue-not-full answer within the backoff window
            // — ask again, with the next window doubled up to READY_POLL_MAX.
            _ = async {
                if let Some(ref mut timer) = ready_query_timer {
                    timer.await;
                }
            }, if ready_query_timer.is_some() => {
                if let Err(e) = port.write_all(&READY_QUERY_FRAME).await {
                    tracing::warn!("{}: ready query write error: {}", name, e);
                    abandon_send_queue(
                        &name, &counters, &mut send_queue, "ready_query_write_error",
                    );
                    return outgoing_rx;
                }
                if let Err(e) = port.flush().await {
                    tracing::warn!("{}: ready query flush error: {}", name, e);
                    abandon_send_queue(
                        &name, &counters, &mut send_queue, "ready_query_flush_error",
                    );
                    return outgoing_rx;
                }
                ready_query_timer = Some(Box::pin(tokio::time::sleep(ready_poll)));
                ready_poll = (ready_poll * 2).min(READY_POLL_MAX);
            }

            // Branch 4: Periodic heartbeat. CMD_DETECT ping to verify firmware
            _ = &mut heartbeat_timer => {
                let detect_frame = [kiss::FEND, rnode::CMD_DETECT, rnode::DETECT_REQ, kiss::FEND];
                if let Err(e) = port.write_all(&detect_frame).await {
                    tracing::warn!("{}: heartbeat write error: {}", name, e);
                    abandon_send_queue(
                        &name, &counters, &mut send_queue, "heartbeat_write_error",
                    );
                    return outgoing_rx;
                }
                heartbeat_pending = true;
                tracing::debug!("{}: heartbeat sent", name);
                heartbeat_timer = Box::pin(tokio::time::sleep(HEARTBEAT_INTERVAL));
            }
        }

        // Track the flow-control gate's hold state after every
        // iteration, before the send attempt below (a reopening gate must
        // emit its RNODE_TX_RELEASED with the frames still held, not after
        // the send block has already dispatched the first of them).
        // "Holding" means frames are queued and only the READY gate keeps
        // them there. Every TX re-enters this state on the next iteration
        // (the gate closes at TX and stays closed until a query answers
        // 0x01); events fire only if it persists past
        // TX_GATED_EVENT_AFTER, so ordinary airtime waits stay silent. The
        // queue cannot drain while the gate is closed, so leaving the hold
        // state means the gate reopened — if the hold was announced,
        // RNODE_TX_RELEASED closes the pair opened by RNODE_TX_GATED.
        let gate_holding = flow_control && !interface_ready && !send_queue.is_empty();
        match (gate_holding, gate_blocked_since) {
            (true, None) => {
                gate_blocked_since = Some(tokio::time::Instant::now());
                gate_event_timer = Some(Box::pin(tokio::time::sleep(TX_GATED_EVENT_AFTER)));
            }
            (false, Some(since)) => {
                if gate_announced {
                    tracing::warn!(
                        event = "RNODE_TX_RELEASED",
                        iface = %name,
                        held_ms = since.elapsed().as_millis() as u64,
                        depth = send_queue.len(),
                    );
                }
                gate_blocked_since = None;
                gate_event_timer = None;
                gate_announced = false;
            }
            _ => {}
        }

        // After any branch: try to send if all gates are open
        //   Gate 1: timer_ready (jitter/spacing delay elapsed)
        //   Gate 2: interface_ready || !flow_control
        if timer_ready && (interface_ready || !flow_control) {
            if let Some(queued) = send_queue.pop_front() {
                if let Err(e) = port.write_all(&queued.data).await {
                    tracing::warn!("{}: write error: {}", name, e);
                    // The frame in hand never made it out either — put it
                    // back so the count names every frame that is lost.
                    send_queue.push_front(queued);
                    abandon_send_queue(&name, &counters, &mut send_queue, "serial_write_error");
                    return outgoing_rx;
                }
                // tcdrain: block until firmware has received all bytes.
                // Without this, write_all() returns as soon as bytes enter
                // the OS serial buffer, multiple frames accumulate in the
                // firmware queue and flush_queue() sends them all in one
                // burst without CSMA between them.
                if let Err(e) = port.flush().await {
                    tracing::warn!("{}: flush error: {}", name, e);
                    // Bytes may have reached the OS buffer but not the
                    // firmware; count the frame as lost rather than as sent.
                    send_queue.push_front(queued);
                    abandon_send_queue(&name, &counters, &mut send_queue, "serial_flush_error");
                    return outgoing_rx;
                }
                counters
                    .tx_bytes
                    .fetch_add(queued.payload_len as usize, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!("{}: TX {} bytes to serial", name, queued.payload_len);
                // Bug #25 capture-compare: structured event at the host →
                // RNode serial boundary. Measurement-only; DEBUG-level under
                // the dedicated target so it can be filtered independently
                // of the rest of the rnode logs.
                tracing::debug!(
                    target: "leviculum_std::interfaces::rnode::tx_trace",
                    "LORA_TX iface={name} len={}",
                    queued.payload_len
                );

                timer_ready = false;
                if flow_control {
                    // Ask, don't wait: close the gate and query the queue
                    // state. The firmware answers 0x01/0x00 to this query;
                    // it never volunteers a READY.
                    interface_ready = false;
                    if let Err(e) = port.write_all(&READY_QUERY_FRAME).await {
                        tracing::warn!("{}: ready query write error: {}", name, e);
                        abandon_send_queue(
                            &name,
                            &counters,
                            &mut send_queue,
                            "ready_query_write_error",
                        );
                        return outgoing_rx;
                    }
                    if let Err(e) = port.flush().await {
                        tracing::warn!("{}: ready query flush error: {}", name, e);
                        abandon_send_queue(
                            &name,
                            &counters,
                            &mut send_queue,
                            "ready_query_flush_error",
                        );
                        return outgoing_rx;
                    }
                    ready_poll = ready_poll_start;
                    ready_query_timer = Some(Box::pin(tokio::time::sleep(ready_poll)));
                    ready_poll = (ready_poll * 2).min(READY_POLL_MAX);
                }

                // Schedule spacing timer after every TX. The flush() above
                // ensures the firmware has received this frame before we proceed.
                // MIN_SPACING_MS gives the firmware time to move the frame from
                // its serial buffer into the TX queue. The firmware's own CSMA
                // handles radio-level collision avoidance, we don't simulate
                // airtime in software.
                {
                    send_timer = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
                        rnode::MIN_SPACING_MS,
                    ))));
                }
            } else {
                timer_ready = false;
            }
        }
    }
}

/// Best-effort send radio-off + leave commands on shutdown
async fn send_goodbye<S>(port: &mut S, name: &str)
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let mut goodbye = rnode::build_set_radio_state(rnode::RADIO_STATE_OFF);
    goodbye.extend_from_slice(&rnode::build_leave());
    if let Err(e) = port.write_all(&goodbye).await {
        tracing::debug!("{name}: goodbye write failed (expected on disconnect): {e}");
    }
}

// ---------------------------------------------------------------------------
// Reconnect wrapper
// ---------------------------------------------------------------------------

/// Runtime parameters for the reconnect loop, independent of how the byte
/// channel is obtained (serial path vs. host-supplied channel factory).
struct RNodeReconnectCtx {
    id: InterfaceId,
    name: String,
    radio: RadioParams,
    flow_control: bool,
    reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
    jitter_max_ms: u64,
    /// TEST-ONLY range emulation: drop deframed hops=0 ingress frames
    /// (see [`super::test_drop_direct_ingress_frame`]).
    test_drop_direct_ingress: bool,
}

/// Reconnect loop: open channel → configure → I/O → on disconnect → wait → retry.
///
/// Carrier-agnostic: `connect` yields a fresh, opened (but unconfigured) byte
/// channel each attempt — a serial port for the path-based interface, or a
/// host-supplied duplex channel for [`spawn_rnode_channel_interface`]. The loop
/// runs detect/configure/validate on it via [`configure_stream`], so the
/// lifecycle is identical regardless of substrate.
async fn rnode_reconnect_task<S, C, Fut>(
    ctx: RNodeReconnectCtx,
    connect: C,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    mut outgoing_rx: mpsc::Receiver<OutgoingPacket>,
    counters: Arc<InterfaceCounters>,
    ready: Arc<super::ReadySignal>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Fn() -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<S, RNodeError>> + Send,
{
    let radio = &ctx.radio;
    let bitrate_bps = rnode::compute_bitrate(radio.sf, radio.cr, radio.bandwidth);
    tracing::debug!(
        "{}: bitrate={} bps, min_spacing={}ms, jitter_max={}ms (airtime-based)",
        ctx.name,
        bitrate_bps,
        rnode::MIN_SPACING_MS,
        ctx.jitter_max_ms,
    );
    let mut has_connected_before = false;

    loop {
        // Between here and a configured radio the carrier is down (L-0020).
        counters.set_online(false);
        // Open the channel, then configure it. Combined so either step's error
        // routes through the same reconnect-retry path.
        let opened = async {
            let mut port = connect().await?;
            let detect = configure_stream(&mut port, radio, &ctx.name).await?;
            Ok::<_, RNodeError>((port, detect))
        }
        .await;

        match opened {
            Ok((port, detect)) => {
                let is_reconnect = has_connected_before;
                has_connected_before = true;
                counters.set_online(true);
                // Readiness contract (L-0024): detect answered, firmware
                // validated, radio configured — only now may a caller that
                // waited on `ready` transmit. One-way, like the TCP client's
                // Option α signal: reconnects keep the signal set.
                ready.signal_ready();

                if let Some((major, minor)) = detect.firmware_version {
                    tracing::info!(
                        "{}: configured (FW {}.{}, freq={} Hz, bw={} Hz, sf={}, cr={}, txp={} dBm)",
                        ctx.name,
                        major,
                        minor,
                        radio.frequency,
                        radio.bandwidth,
                        radio.sf,
                        radio.cr,
                        radio.tx_power
                    );
                }

                // Notify driver about reconnection so it can re-announce
                if is_reconnect {
                    if let Some(ref notify) = ctx.reconnect_notify {
                        if let Err(e) = notify.try_send(ctx.id) {
                            tracing::warn!("{}: reconnect notify failed: {}", ctx.name, e);
                        }
                    }
                }

                outgoing_rx = rnode_io_task(
                    ctx.name.clone(),
                    port,
                    incoming_tx.clone(),
                    outgoing_rx,
                    Arc::clone(&counters),
                    ctx.flow_control,
                    ctx.jitter_max_ms,
                    radio.bandwidth,
                    radio.sf,
                    radio.cr,
                    ctx.test_drop_direct_ingress,
                )
                .await;

                counters.set_online(false);
                tracing::warn!("{}: disconnected", ctx.name);
            }
            Err(e) => {
                tracing::warn!("{}: configuration failed: {}", ctx.name, e);
            }
        }

        // Check if event loop shut down
        if incoming_tx.is_closed() {
            tracing::debug!("{}: event loop shut down, stopping reconnect", ctx.name);
            return;
        }

        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

// ---------------------------------------------------------------------------
// Custom byte-channel factory (phone-attached radios)
// ---------------------------------------------------------------------------

/// The two boxed halves of a duplex byte channel, as yielded by an
/// [`RNodeChannelFactory`]. Separate read/write halves rather than a single
/// `AsyncRead + AsyncWrite` object because a trait object can name only one
/// non-marker trait; the interface re-joins them with [`tokio::io::join`].
pub type RNodeChannelHalves = (
    Box<dyn AsyncRead + Send + Unpin>,
    Box<dyn AsyncWrite + Send + Unpin>,
);

/// The future returned by [`RNodeChannelFactory::open`]: resolves to a fresh
/// pair of channel halves, or a boxed error.
pub type RNodeChannelOpenFuture = Pin<
    Box<
        dyn std::future::Future<
                Output = Result<RNodeChannelHalves, Box<dyn std::error::Error + Send + Sync>>,
            > + Send,
    >,
>;

/// A factory the reconnect loop calls to obtain a fresh duplex byte channel to
/// the RNode firmware.
///
/// Lets a host application supply the radio I/O over any substrate — USB-CDC,
/// BLE GATT (notify characteristic for read + write characteristic for write),
/// BT-Classic SPP, or an in-process mock pipe — on platforms where leviculum
/// never sees `/dev/ttyACM*` and cannot `open()` a serial path (Android, iOS).
/// The far end still speaks RNode KISS; leviculum still drives
/// detection/configuration/lifecycle. `open` is called once per (re)connection
/// attempt and should return a freshly-established channel each time.
pub trait RNodeChannelFactory: Send + Sync + 'static {
    /// Open a fresh duplex byte channel to the radio.
    fn open(&self) -> RNodeChannelOpenFuture;
}

/// Configuration for spawning an RNode interface over a host-supplied byte
/// channel (see [`RNodeChannelFactory`]). Mirrors [`RNodeInterfaceConfig`] but
/// replaces `port_path` with a `channel_factory`.
pub(crate) struct RNodeChannelInterfaceConfig {
    pub id: InterfaceId,
    pub name: String,
    pub channel_factory: Arc<dyn RNodeChannelFactory>,
    pub frequency: u32,
    pub bandwidth: u32,
    pub tx_power: u8,
    pub sf: u8,
    pub cr: u8,
    pub st_alock: Option<u16>,
    pub lt_alock: Option<u16>,
    pub flow_control: bool,
    pub buffer_size: usize,
    pub reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
}

impl RNodeChannelInterfaceConfig {
    fn radio_params(&self) -> RadioParams {
        RadioParams {
            frequency: self.frequency,
            bandwidth: self.bandwidth,
            tx_power: self.tx_power,
            // The channel API spells `tx_power` as a plain `u8`: the caller
            // always states one, so it is never the absent-key default and the
            // strict confirmation check applies.
            tx_power_derived: false,
            sf: self.sf,
            cr: self.cr,
            st_alock: self.st_alock,
            lt_alock: self.lt_alock,
        }
    }
}

/// Radio + transport parameters for a channel-backed RNode interface.
///
/// Used two ways:
/// - construction-time, built from
///   [`ReticulumNodeBuilder::add_rnode_channel_interface`](crate::driver::ReticulumNodeBuilder::add_rnode_channel_interface);
/// - runtime, passed to
///   [`ReticulumNode::spawn_rnode_channel_interface`](crate::driver::ReticulumNode::spawn_rnode_channel_interface)
///   to hot-plug a radio after the node is running.
///
/// The node assigns the `InterfaceId` and name; this config carries only the
/// caller-relevant fields. `frequency`/`bandwidth` are Hz, `tx_power` is dBm.
pub struct RNodeChannelConfig {
    pub factory: Arc<dyn RNodeChannelFactory>,
    pub frequency: u32,
    pub bandwidth: u32,
    pub tx_power: u8,
    pub sf: u8,
    pub cr: u8,
    pub st_alock: Option<u16>,
    pub lt_alock: Option<u16>,
    pub flow_control: bool,
    pub buffer_size: usize,
}

/// Lifecycle handle for a runtime-attached channel-backed RNode interface,
/// returned by
/// [`ReticulumNode::spawn_rnode_channel_interface`](crate::driver::ReticulumNode::spawn_rnode_channel_interface).
///
/// **Hold it to keep the radio attached; drop it to detach.** Dropping (or
/// calling [`detach`](Self::detach)) signals the interface task to stop, which
/// closes its channel and makes the node's event loop tear the interface down
/// and remove it from routing — cleanly, without rebuilding the node.
///
/// (The interface's I/O channels live inside the node's event loop, which is
/// why this is a small control handle rather than the internal
/// `InterfaceHandle`: the latter's channels must be owned by the loop for the
/// interface to route at all.)
pub struct RNodeChannelHandle {
    id: InterfaceId,
    // Dropping this Sender resolves the task's shutdown receiver, which exits
    // the reconnect loop -> closes the incoming channel -> event loop detaches.
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl RNodeChannelHandle {
    pub(crate) fn new(id: InterfaceId, shutdown: tokio::sync::oneshot::Sender<()>) -> Self {
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

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Wire up channels + counters, spawn the reconnect task with the given
/// connector, and return an `InterfaceHandle`. Shared by both the serial
/// (`spawn_rnode_interface`) and channel (`spawn_rnode_channel_interface`)
/// entry points.
fn spawn_rnode_with_connector<S, C, Fut>(
    ctx: RNodeReconnectCtx,
    buffer_size: usize,
    connect: C,
    shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> InterfaceHandle
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<S, RNodeError>> + Send,
{
    let (incoming_tx, incoming_rx) = mpsc::channel(buffer_size);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(buffer_size);
    let counters = Arc::new(InterfaceCounters::new());
    // Codeberg #25: RNode reports radio stats via CMD_STAT_*; mark the counters
    // radio-capable so interface_stats always emits the radio keys (with their
    // defaults) even before the first frame arrives.
    counters.enable_radio_stats();
    // Offline until the reconnect loop has detected and configured the radio
    // (L-0020); covers the window between registration and first configure.
    counters.set_online(false);
    let ready = super::ReadySignal::new();

    let id = ctx.id;
    let name = ctx.name.clone();
    let task_counters = Arc::clone(&counters);
    let task_ready = Arc::clone(&ready);
    let bitrate = rnode::compute_bitrate(ctx.radio.sf, ctx.radio.cr, ctx.radio.bandwidth);
    // Copied out before `ctx` moves into the task: the handle reports the
    // same pre-TX jitter ceiling the TX loop actually draws against, rather
    // than recomputing it and risking the two drifting apart.
    let tx_jitter_max_ms = ctx.jitter_max_ms;

    tokio::spawn(async move {
        let run = rnode_reconnect_task(
            ctx,
            connect,
            incoming_tx,
            outgoing_rx,
            task_counters,
            task_ready,
        );
        match shutdown {
            // Runtime-attached interface: stop promptly when the caller drops
            // its RNodeChannelHandle (the Sender drops, this resolves). The
            // select drops `run`, cancelling the in-flight configure/IO and the
            // reconnect sleeps; the task then ends and its incoming channel
            // closes, which the event loop turns into a detach.
            Some(sd) => {
                tokio::select! {
                    _ = sd => {}
                    _ = run => {}
                }
            }
            // Construction-time interface: lives until the event loop drops the
            // handle (closing the incoming channel ends the reconnect loop).
            None => run.await,
        }
    });

    InterfaceHandle {
        info: InterfaceInfo {
            id,
            name,
            hw_mtu: Some(rnode::HW_MTU as u32),
            is_local_client: false,
            bitrate: Some(bitrate),
            tx_jitter_max_ms: Some(tx_jitter_max_ms),
            ifac: None,
            mode: leviculum_core::traits::InterfaceMode::default(),
            kind: leviculum_core::traits::InterfaceKind::Rnode,
            ingress_control: None,
        },
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        counters,
        credit: None,
        // RNode readiness is async: the reconnect task signals once the
        // channel is open, the firmware probe answered, and the radio is
        // configured (L-0024).
        ready,
    }
}

fn reconnect_ctx_from_radio(
    id: InterfaceId,
    name: String,
    radio: RadioParams,
    flow_control: bool,
    reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
    test_drop_direct_ingress: bool,
) -> RNodeReconnectCtx {
    let jitter_max_ms = compute_jitter_max_ms(radio.sf, radio.bandwidth);
    RNodeReconnectCtx {
        id,
        name,
        radio,
        flow_control,
        reconnect_notify,
        jitter_max_ms,
        test_drop_direct_ingress,
    }
}

/// Spawn a complete RNode interface over a serial port, with reconnection.
///
/// Creates channels + counters, spawns the reconnect task, and returns an
/// `InterfaceHandle` for the event loop. Each (re)connection opens the serial
/// port fresh via [`open_serial_port`].
pub(crate) fn spawn_rnode_interface(config: RNodeInterfaceConfig) -> InterfaceHandle {
    let ctx = reconnect_ctx_from_radio(
        config.id,
        config.name.clone(),
        config.radio_params(),
        config.flow_control,
        config.reconnect_notify,
        config.test_drop_direct_ingress,
    );
    let buffer_size = config.buffer_size;
    let port_path = config.port_path;
    spawn_rnode_with_connector(
        ctx,
        buffer_size,
        move || {
            let path = port_path.clone();
            async move { open_serial_port(&path).await }
        },
        None,
    )
}

/// Spawn a complete RNode interface over a host-supplied byte channel, with
/// reconnection. The lifecycle (detect → configure → online → I/O →
/// reconnect-on-drop) is identical to [`spawn_rnode_interface`]; only the
/// transport differs. Each (re)connection calls
/// [`RNodeChannelFactory::open`] for a fresh duplex channel.
pub(crate) fn spawn_rnode_channel_interface(
    config: RNodeChannelInterfaceConfig,
    shutdown: Option<tokio::sync::oneshot::Receiver<()>>,
) -> InterfaceHandle {
    let ctx = reconnect_ctx_from_radio(
        config.id,
        config.name.clone(),
        config.radio_params(),
        config.flow_control,
        config.reconnect_notify,
        // Range emulation is a rig affordance; the phone-attached channel
        // path never needs it.
        false,
    );
    let buffer_size = config.buffer_size;
    let factory = config.channel_factory;
    spawn_rnode_with_connector(
        ctx,
        buffer_size,
        move || {
            let factory = Arc::clone(&factory);
            async move {
                let (read_half, write_half) = factory
                    .open()
                    .await
                    .map_err(|e| RNodeError::SerialPort(e.to_string()))?;
                // Re-join the two boxed halves into one AsyncRead + AsyncWrite
                // stream for the carrier-agnostic configure/IO path.
                Ok(tokio::io::join(read_half, write_half))
            }
        },
        shutdown,
    )
}

// ---------------------------------------------------------------------------
// Multi-interface (multi-vport RNode)
// ---------------------------------------------------------------------------
//
// An `RNodeMultiInterface` drives a single RNode that carries several LoRa
// transceivers, each exposed as a virtual port (vport). One serial link is
// shared; every per-vport command is prefixed with a CMD_SEL_INT frame
// (see `leviculum_core::rnode::build_vport_command`). Each vport is registered
// with the transport as its own logical interface, so announces and paths work
// per band exactly as if the radios were separate devices.
//
// Layering (matches the single-RNode interface): the carrier-medium specifics
// (serial framing, vport multiplexing, per-vport radio config push) live here;
// the transport sees N ordinary interfaces and stays vport-agnostic.

/// Radio + routing parameters for one vport subinterface.
pub(crate) struct RNodeSubinterfaceParams {
    /// Transport interface id assigned to this vport.
    pub id: InterfaceId,
    /// Display name (`<multi name>[<sub name>]`).
    pub name: String,
    /// Virtual port index on the device.
    pub vport: u8,
    pub frequency: u32,
    pub bandwidth: u32,
    pub tx_power: u8,
    /// See [`RadioParams::tx_power_derived`].
    pub tx_power_derived: bool,
    pub sf: u8,
    pub cr: u8,
    pub st_alock: Option<u16>,
    pub lt_alock: Option<u16>,
    /// Whether this subinterface may transmit (Python `interface.OUT`).
    pub outgoing: bool,
}

impl RNodeSubinterfaceParams {
    fn radio_params(&self) -> RadioParams {
        RadioParams {
            frequency: self.frequency,
            bandwidth: self.bandwidth,
            tx_power: self.tx_power,
            tx_power_derived: self.tx_power_derived,
            sf: self.sf,
            cr: self.cr,
            st_alock: self.st_alock,
            lt_alock: self.lt_alock,
        }
    }
}

/// Configuration for spawning a multi-vport RNode interface over a serial port.
pub(crate) struct RNodeMultiInterfaceConfig {
    /// Name of the parent multi interface.
    pub name: String,
    /// Serial port shared by all vports.
    pub port_path: String,
    /// One entry per enabled subinterface (vport).
    pub subinterfaces: Vec<RNodeSubinterfaceParams>,
    pub flow_control: bool,
    pub buffer_size: usize,
    pub reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
}

/// Per-vport runtime state the shared hub task holds: the radio params to push,
/// the routing tag, and the channel to deliver received packets to this vport's
/// logical interface.
struct VportRuntime {
    id: InterfaceId,
    name: String,
    vport: u8,
    radio: RadioParams,
    outgoing: bool,
    incoming_tx: mpsc::Sender<IncomingPacket>,
    counters: Arc<InterfaceCounters>,
}

/// A TX packet after merging all vports' outgoing channels, tagged with the
/// index into the hub's `VportRuntime` list it came from.
struct TaggedOutgoing {
    subint: usize,
    packet: OutgoingPacket,
}

/// Detect a multi-vport RNode and read its reported per-vport chip types.
///
/// Sends [`build_detect_query_multi`](rnode::build_detect_query_multi) (detect +
/// firmware + platform + MCU + interfaces) and collects the responses. The
/// returned `Vec<u8>` holds one chip-type byte per vport in vport order (empty
/// if the firmware did not report — older single-radio firmware, or a mock).
async fn detect_multi_on_port<S>(port: &mut S) -> Result<(RNodeDetectResult, Vec<u8>), RNodeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let query = rnode::build_detect_query_multi();
    port.write_all(&query).await?;

    let mut result = RNodeDetectResult {
        detected: false,
        firmware_version: None,
        platform: None,
        mcu: None,
    };
    let mut chip_types: Vec<u8> = Vec::new();

    read_frames_until_deadline(port, DETECT_TIMEOUT, |command, payload| match command {
        rnode::CMD_DETECT if payload.first() == Some(&rnode::DETECT_RESP) => {
            result.detected = true;
        }
        rnode::CMD_FW_VERSION => {
            result.firmware_version = rnode::decode_firmware_version(payload);
        }
        rnode::CMD_PLATFORM => {
            result.platform = payload.first().copied();
        }
        rnode::CMD_MCU => {
            result.mcu = payload.first().copied();
        }
        rnode::CMD_INTERFACES => {
            // One frame can carry several 2-byte records; a device may also
            // emit multiple frames. Append in arrival (vport) order.
            chip_types.extend(rnode::decode_interfaces(payload));
        }
        _ => {}
    })
    .await?;

    if !result.detected {
        return Err(RNodeError::NotDetected);
    }

    Ok((result, chip_types))
}

/// Push per-vport radio configuration: for each vport, a CMD_SEL_INT frame
/// followed by frequency/bandwidth/txpower/sf/cr/[alock]/radio-on, mirroring
/// Python `RNodeSubInterface.initRadio` driving the parent's `set*` methods.
async fn send_multi_radio_config<S>(port: &mut S, vports: &[VportRuntime]) -> Result<(), RNodeError>
where
    S: AsyncWrite + Unpin,
{
    let mut bytes = Vec::with_capacity(64 * vports.len());
    for v in vports {
        let r = &v.radio;
        bytes.extend_from_slice(&rnode::build_vport_command(
            v.vport,
            &rnode::build_set_frequency(r.frequency),
        ));
        bytes.extend_from_slice(&rnode::build_vport_command(
            v.vport,
            &rnode::build_set_bandwidth(r.bandwidth),
        ));
        bytes.extend_from_slice(&rnode::build_vport_command(
            v.vport,
            &rnode::build_set_txpower(r.tx_power),
        ));
        bytes.extend_from_slice(&rnode::build_vport_command(
            v.vport,
            &rnode::build_set_sf(r.sf),
        ));
        bytes.extend_from_slice(&rnode::build_vport_command(
            v.vport,
            &rnode::build_set_cr(r.cr),
        ));
        if let Some(st) = r.st_alock {
            bytes.extend_from_slice(&rnode::build_vport_command(
                v.vport,
                &rnode::build_set_st_alock(st),
            ));
        }
        if let Some(lt) = r.lt_alock {
            bytes.extend_from_slice(&rnode::build_vport_command(
                v.vport,
                &rnode::build_set_lt_alock(lt),
            ));
        }
        bytes.extend_from_slice(&rnode::build_vport_command(
            v.vport,
            &rnode::build_set_radio_state(rnode::RADIO_STATE_ON),
        ));
    }
    port.write_all(&bytes).await?;
    port.flush().await?;
    Ok(())
}

/// Detect + validate firmware + validate vports + push per-vport config over an
/// already-open, settled channel. The single-RNode analogue is
/// [`configure_stream`]; this adds the vport dimension.
async fn configure_multi_stream<S>(
    port: &mut S,
    vports: &[VportRuntime],
    name: &str,
) -> Result<RNodeDetectResult, RNodeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (detect, chip_types) = detect_multi_on_port(port).await?;

    match detect.firmware_version {
        Some((maj, min)) if rnode::validate_firmware(maj, min) => {}
        Some((maj, min)) => {
            return Err(RNodeError::FirmwareTooOld(
                maj,
                min,
                rnode::REQUIRED_FW_MAJ,
                rnode::REQUIRED_FW_MIN,
            ));
        }
        None => return Err(RNodeError::NotDetected),
    }

    // Validate each vport index against the device's report. When the device
    // reported types (`!chip_types.is_empty()`), a configured vport must exist;
    // this is Python's hard check. When it reported none (mock or firmware that
    // does not answer CMD_INTERFACES), proceed best-effort -- the SEL_INT frames
    // are still correct, and refusing to start would strand a usable radio.
    for v in vports {
        rnode::validate_config(
            v.radio.frequency,
            v.radio.bandwidth,
            v.radio.tx_power,
            v.radio.sf,
            v.radio.cr,
        )
        .map_err(|e| RNodeError::RadioMismatch(format!("{}: {}", v.name, e)))?;
        if !chip_types.is_empty() && (v.vport as usize) >= chip_types.len() {
            return Err(RNodeError::RadioMismatch(format!(
                "vport {} for {} does not exist on device ({} vports reported)",
                v.vport,
                v.name,
                chip_types.len()
            )));
        }
    }

    send_multi_radio_config(port, vports).await?;
    tokio::time::sleep(CONFIG_PROCESS_WAIT).await;

    // Drain confirmation frames for the settle window. Per-vport strict
    // validation is deferred (HW gap): unlike the single interface we do not
    // fail startup on a missing echo, because a partial multi-band device
    // should still bring up the vports it can. Config correctness is covered by
    // the byte-level KAT and the mock exchange.
    let _ = read_frames_until_deadline(port, CONFIG_PROCESS_WAIT, |_, _| {}).await;

    tracing::info!(
        "{}: multi-vport configured ({} vports, device reported {} chip types)",
        name,
        vports.len(),
        chip_types.len()
    );
    Ok(detect)
}

/// Shared serial I/O loop for a multi-vport RNode.
///
/// RX: tracks the selected vport from CMD_SEL_INT frames and routes each
/// following CMD_DATA frame to that vport's logical interface. TX: pulls
/// vport-tagged packets from `merged_rx`, prefixes each with its vport's
/// CMD_SEL_INT, and paces writes with the serial-level spacing floor.
///
/// Returns when the channel fails so the reconnect wrapper can retry.
async fn rnode_multi_io_task<S>(
    name: &str,
    port: &mut S,
    vports: &[VportRuntime],
    vport_to_subint: &HashMap<u8, usize>,
    merged_rx: &mut mpsc::Receiver<TaggedOutgoing>,
    flow_control: bool,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
    let mut buf = [0u8; IO_READ_BUF];
    let mut selected_vport: u8 = 0;
    // Vports whose logical interface has been torn down. A dead vport is
    // deregistered from routing, not a reason to drop the shared radio: the
    // other vports on the same serial port are still carrying traffic (#283).
    // Seeded from the channel state so a vport that died during a disconnect
    // is not rediscovered one lost frame at a time.
    let mut dead: Vec<bool> = vports.iter().map(|v| v.incoming_tx.is_closed()).collect();
    if dead.iter().all(|&d| d) {
        tracing::debug!("{}: no live vport left, io task not starting", name);
        return;
    }
    let mut interface_ready = true;
    let mut send_queue: VecDeque<(usize, Vec<u8>)> = VecDeque::new();
    let mut send_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;
    let mut timer_ready = true;

    // CMD_READY reopen state, same query protocol as the single-radio io
    // task (the firmware only ever answers a host query). The vports share
    // one firmware queue, so one gate and one poll cadence: seeded from the
    // slowest vport's packet airtime — the conservative bound on how fast
    // the shared queue can drain.
    let ready_poll_start = vports
        .iter()
        .map(|v| ready_poll_initial(v.radio.sf, v.radio.cr, v.radio.bandwidth))
        .max()
        .unwrap_or(READY_POLL_MAX);
    let mut ready_poll = ready_poll_start;
    let mut ready_query_timer: Option<Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        tokio::select! {
            result = port.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        tracing::warn!("{}: serial port EOF", name);
                        abandon_multi_send_queue(name, vports, &mut send_queue, "serial_eof");
                        return;
                    }
                    Ok(n) => {
                        for frame in deframer.process(&buf[..n]) {
                            let KissDeframeResult::Frame { command, payload } = frame else { continue; };
                            match command {
                                rnode::CMD_SEL_INT => {
                                    if let Some(vp) = rnode::decode_select_interface(&payload) {
                                        selected_vport = vp;
                                    }
                                }
                                rnode::CMD_DATA => {
                                    match vport_to_subint.get(&selected_vport) {
                                        Some(&idx) if !dead[idx] => {
                                            let v = &vports[idx];
                                            v.counters.rx_bytes.fetch_add(
                                                payload.len() as usize,
                                                std::sync::atomic::Ordering::Relaxed,
                                            );
                                            tracing::debug!(
                                                "{}: RX {} bytes on vport {} -> {}",
                                                name, payload.len(), selected_vport, v.name
                                            );
                                            if v.incoming_tx
                                                .send(IncomingPacket { data: payload.to_vec() })
                                                .await
                                                .is_err()
                                            {
                                                // This vport's event loop is gone. Deregister
                                                // it and keep serving the others: returning
                                                // here tore down the shared radio, and the
                                                // reconnect loop then bounced it every five
                                                // seconds forever, because the next frame for
                                                // the same dead vport ended it again (#283).
                                                deregister_vport(
                                                    name, vports, &mut send_queue, &mut dead, idx,
                                                );
                                                if dead.iter().all(|&d| d) {
                                                    tracing::debug!(
                                                        "{}: last vport shut down, ending io task",
                                                        name
                                                    );
                                                    return;
                                                }
                                            }
                                        }
                                        // A vport whose interface is gone: its frames are
                                        // dropped where they arrive, without touching the
                                        // radio the live vports share.
                                        Some(&idx) => {
                                            tracing::trace!(
                                                "{}: RX for deregistered vport {} -> {} (dropped)",
                                                name, selected_vport, vports[idx].name
                                            );
                                        }
                                        None => {
                                            tracing::warn!(
                                                "{}: RX data for unknown vport {} (dropped)",
                                                name, selected_vport
                                            );
                                        }
                                    }
                                }
                                rnode::CMD_READY if flow_control => {
                                    // Answer to our queue-state query: 0x01 reopens
                                    // the gate, 0x00 leaves the armed timer polling.
                                    if payload.first() == Some(&0x01) {
                                        interface_ready = true;
                                        ready_query_timer = None;
                                        ready_poll = ready_poll_start;
                                    }
                                }
                                rnode::CMD_ERROR => {
                                    match payload.first().copied() {
                                        Some(rnode::ERROR_INITRADIO) => {
                                            tracing::error!("{}: radio init failed", name);
                                            abandon_multi_send_queue(
                                                name, vports, &mut send_queue, "error_initradio",
                                            );
                                            return;
                                        }
                                        Some(rnode::ERROR_TXFAILED) => {
                                            tracing::error!("{}: TX failed", name);
                                            abandon_multi_send_queue(
                                                name, vports, &mut send_queue, "error_txfailed",
                                            );
                                            return;
                                        }
                                        Some(code) => {
                                            tracing::warn!("{}: device error 0x{:02X}", name, code);
                                        }
                                        None => {}
                                    }
                                }
                                rnode::CMD_RESET
                                    if payload.first() == Some(&DEVICE_RESET_MARKER) =>
                                {
                                    tracing::warn!("{}: device reset (0xF8)", name);
                                    abandon_multi_send_queue(
                                        name, vports, &mut send_queue, "device_reset",
                                    );
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("{}: serial read error: {}", name, e);
                        abandon_multi_send_queue(
                            name, vports, &mut send_queue, "serial_read_error",
                        );
                        return;
                    }
                }
            }

            recv = merged_rx.recv() => {
                match recv {
                    Some(tagged) => {
                        // A subinterface with outgoing = false must never transmit
                        // (Python `interface.OUT = False`). Drop silently.
                        if !vports[tagged.subint].outgoing {
                            tracing::debug!(
                                "{}: dropping TX on non-outgoing vport {}",
                                name, vports[tagged.subint].vport
                            );
                            continue;
                        }
                        // A deregistered vport keeps its hands off the shared
                        // radio in both directions (#283).
                        if dead[tagged.subint] {
                            tracing::debug!(
                                "{}: dropping TX on deregistered vport {}",
                                name, vports[tagged.subint].vport
                            );
                            continue;
                        }
                        if send_queue.len() >= FLOW_CONTROL_QUEUE_LIMIT {
                            if let Some((shed_subint, shed)) = send_queue.pop_front() {
                                // A dropped frame must be loud, same as the
                                // single-radio twin. The count lands on the
                                // vport that owned the shed frame; the event
                                // names the physical interface — the port
                                // that dropped is a property of the shared
                                // line, not of any one vport.
                                vports[shed_subint]
                                    .counters
                                    .tx_queue_drops
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                vports[shed_subint].counters.tx_dropped_bytes.fetch_add(
                                    shed.len() as usize,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                tracing::warn!(
                                    event = "RNODE_TX_QUEUE_DROP",
                                    iface = %name,
                                    len = shed.len(),
                                    depth = send_queue.len(),
                                    reason = "queue_full",
                                );
                            }
                        }
                        send_queue.push_back((tagged.subint, tagged.packet.data));
                    }
                    None => {
                        // All vport senders dropped: interface tearing down.
                        abandon_multi_send_queue(
                            name, vports, &mut send_queue, "outgoing_closed",
                        );
                        return;
                    }
                }
            }

            _ = async {
                if let Some(ref mut timer) = send_timer {
                    timer.await;
                }
            }, if send_timer.is_some() => {
                send_timer = None;
                timer_ready = true;
            }

            // No queue-not-full answer within the backoff window — re-query.
            _ = async {
                if let Some(ref mut timer) = ready_query_timer {
                    timer.await;
                }
            }, if ready_query_timer.is_some() => {
                if let Err(e) = port.write_all(&READY_QUERY_FRAME).await {
                    tracing::warn!("{}: ready query write error: {}", name, e);
                    abandon_multi_send_queue(
                        name, vports, &mut send_queue, "ready_query_write_error",
                    );
                    return;
                }
                if let Err(e) = port.flush().await {
                    tracing::warn!("{}: ready query flush error: {}", name, e);
                    abandon_multi_send_queue(
                        name, vports, &mut send_queue, "ready_query_flush_error",
                    );
                    return;
                }
                ready_query_timer = Some(Box::pin(tokio::time::sleep(ready_poll)));
                ready_poll = (ready_poll * 2).min(READY_POLL_MAX);
            }
        }

        // Send if the spacing gate and the flow-control gate are both open.
        if timer_ready && (interface_ready || !flow_control) {
            if let Some((subint, data)) = send_queue.pop_front() {
                let v = &vports[subint];
                let frame = rnode::build_vport_data_frame(v.vport, &data);
                if let Err(e) = port.write_all(&frame).await {
                    tracing::warn!("{}: write error: {}", name, e);
                    // The frame in hand is lost with the rest; count it.
                    send_queue.push_front((subint, data));
                    abandon_multi_send_queue(name, vports, &mut send_queue, "serial_write_error");
                    return;
                }
                if let Err(e) = port.flush().await {
                    tracing::warn!("{}: flush error: {}", name, e);
                    send_queue.push_front((subint, data));
                    abandon_multi_send_queue(name, vports, &mut send_queue, "serial_flush_error");
                    return;
                }
                v.counters
                    .tx_bytes
                    .fetch_add(data.len() as usize, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!(
                    "{}: TX {} bytes on vport {} ({})",
                    name,
                    data.len(),
                    v.vport,
                    v.name
                );
                timer_ready = false;
                if flow_control {
                    // Same query protocol as the single-radio task: close
                    // the gate, ask the shared queue for room.
                    interface_ready = false;
                    if let Err(e) = port.write_all(&READY_QUERY_FRAME).await {
                        tracing::warn!("{}: ready query write error: {}", name, e);
                        abandon_multi_send_queue(
                            name,
                            vports,
                            &mut send_queue,
                            "ready_query_write_error",
                        );
                        return;
                    }
                    if let Err(e) = port.flush().await {
                        tracing::warn!("{}: ready query flush error: {}", name, e);
                        abandon_multi_send_queue(
                            name,
                            vports,
                            &mut send_queue,
                            "ready_query_flush_error",
                        );
                        return;
                    }
                    ready_poll = ready_poll_start;
                    ready_query_timer = Some(Box::pin(tokio::time::sleep(ready_poll)));
                    ready_poll = (ready_poll * 2).min(READY_POLL_MAX);
                }
                send_timer = Some(Box::pin(tokio::time::sleep(Duration::from_millis(
                    rnode::MIN_SPACING_MS,
                ))));
            }
            // Nothing queued: leave `timer_ready` set so the next packet ships
            // immediately without waiting on a fresh spacing timer.
        }
    }
}

/// Reconnect loop for a multi-vport RNode: open channel -> configure all vports
/// -> shared I/O -> on disconnect notify each vport, wait, retry.
async fn rnode_multi_reconnect_task<S, C, Fut>(
    name: String,
    connect: C,
    vports: Vec<VportRuntime>,
    mut merged_rx: mpsc::Receiver<TaggedOutgoing>,
    flow_control: bool,
    reconnect_notify: Option<mpsc::Sender<InterfaceId>>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<S, RNodeError>>,
{
    let vport_to_subint: HashMap<u8, usize> = vports
        .iter()
        .enumerate()
        .map(|(i, v)| (v.vport, i))
        .collect();
    let mut has_connected_before = false;

    loop {
        let opened = async {
            let mut port = connect().await?;
            configure_multi_stream(&mut port, &vports, &name).await?;
            Ok::<_, RNodeError>(port)
        }
        .await;

        match opened {
            Ok(mut port) => {
                let is_reconnect = has_connected_before;
                has_connected_before = true;
                if is_reconnect {
                    if let Some(ref notify) = reconnect_notify {
                        for v in &vports {
                            if let Err(e) = notify.try_send(v.id) {
                                tracing::warn!("{}: reconnect notify failed: {}", name, e);
                            }
                        }
                    }
                }

                rnode_multi_io_task(
                    &name,
                    &mut port,
                    &vports,
                    &vport_to_subint,
                    &mut merged_rx,
                    flow_control,
                )
                .await;

                tracing::warn!("{}: disconnected", name);
            }
            Err(e) => {
                tracing::warn!("{}: configuration failed: {}", name, e);
            }
        }

        // Stop if every vport's logical interface has been torn down.
        if vports.iter().all(|v| v.incoming_tx.is_closed()) {
            tracing::debug!("{}: all vports shut down, stopping reconnect", name);
            return;
        }

        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

/// Build the per-vport `InterfaceHandle`s and spawn the shared hub task.
///
/// Returns one `InterfaceHandle` per subinterface, each registered with the
/// transport as an independent logical interface. All share the single serial
/// port via the hub task; a per-vport relay forwards that vport's outgoing
/// channel into the hub's merged TX channel, tagged so the hub knows which
/// vport (and CMD_SEL_INT) to emit.
pub(crate) fn spawn_rnode_multi_interface(
    config: RNodeMultiInterfaceConfig,
) -> Vec<InterfaceHandle> {
    let RNodeMultiInterfaceConfig {
        name,
        port_path,
        subinterfaces,
        flow_control,
        buffer_size,
        reconnect_notify,
    } = config;

    let (merged_tx, merged_rx) = mpsc::channel::<TaggedOutgoing>(buffer_size.max(1) * 2);

    let mut handles: Vec<InterfaceHandle> = Vec::with_capacity(subinterfaces.len());
    let mut runtimes: Vec<VportRuntime> = Vec::with_capacity(subinterfaces.len());

    for (subint_idx, sub) in subinterfaces.iter().enumerate() {
        let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingPacket>(buffer_size);
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingPacket>(buffer_size);
        let counters = Arc::new(InterfaceCounters::new());
        let bitrate = rnode::compute_bitrate(sub.sf, sub.cr, sub.bandwidth);

        handles.push(InterfaceHandle {
            info: InterfaceInfo {
                id: sub.id,
                name: sub.name.clone(),
                hw_mtu: Some(rnode::HW_MTU as u32),
                is_local_client: false,
                bitrate: Some(bitrate),
                tx_jitter_max_ms: Some(compute_jitter_max_ms(sub.sf, sub.bandwidth)),
                ifac: None,
                mode: leviculum_core::traits::InterfaceMode::default(),
                kind: leviculum_core::traits::InterfaceKind::Rnode,
                ingress_control: None,
            },
            incoming: incoming_rx,
            outgoing: outgoing_tx,
            counters: Arc::clone(&counters),
            credit: None,
            ready: super::ReadySignal::ready_immediate(),
        });

        // Relay this vport's outgoing packets into the shared merged channel,
        // tagged with its index. Lives until the handle's outgoing sender drops.
        let relay_tx = merged_tx.clone();
        let subint = subint_idx;
        tokio::spawn(async move {
            while let Some(pkt) = outgoing_rx.recv().await {
                if relay_tx
                    .send(TaggedOutgoing {
                        subint,
                        packet: pkt,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        runtimes.push(VportRuntime {
            id: sub.id,
            name: sub.name.clone(),
            vport: sub.vport,
            radio: sub.radio_params(),
            outgoing: sub.outgoing,
            incoming_tx,
            counters,
        });
    }
    // Drop the hub's own clone so the merged channel closes once every relay
    // (i.e. every vport handle) is gone.
    drop(merged_tx);

    tokio::spawn(async move {
        rnode_multi_reconnect_task(
            name,
            move || {
                let path = port_path.clone();
                async move { open_serial_port(&path).await }
            },
            runtimes,
            merged_rx,
            flow_control,
            reconnect_notify,
        )
        .await;
    });

    handles
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// In-process RNode firmware stub over one half of a `tokio::io::duplex`
    /// pair. Answers the detect probe (so firmware validation passes) and echoes
    /// each radio-config command back as its confirmation. When it receives the
    /// first outbound `CMD_DATA` from the interface (proving the I/O phase is
    /// live) it records the payload and injects one inbound data frame in reply
    /// — injecting earlier would have it swallowed by the config-validation
    /// read window, which ignores data frames.
    async fn rnode_firmware_stub(
        mut peer: tokio::io::DuplexStream,
        inbound: Vec<u8>,
        got_outbound: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) {
        let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
        let mut buf = [0u8; 512];
        let mut injected = false;
        // kiss::frame clears its output each call, so accumulate via a scratch.
        let push = |reply: &mut Vec<u8>, cmd: u8, payload: &[u8]| {
            let mut one = Vec::new();
            kiss::frame(cmd, payload, &mut one);
            reply.extend_from_slice(&one);
        };
        loop {
            let n = match peer.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut reply: Vec<u8> = Vec::new();
            for f in deframer.process(&buf[..n]) {
                if let KissDeframeResult::Frame { command, payload } = f {
                    match command {
                        rnode::CMD_DETECT => {
                            // Answer the probe: detected + firmware >= required.
                            push(&mut reply, rnode::CMD_DETECT, &[rnode::DETECT_RESP]);
                            push(
                                &mut reply,
                                rnode::CMD_FW_VERSION,
                                &[rnode::REQUIRED_FW_MAJ, rnode::REQUIRED_FW_MIN],
                            );
                            push(&mut reply, rnode::CMD_PLATFORM, &[rnode::PLATFORM_ESP32]);
                            push(&mut reply, rnode::CMD_MCU, &[0x00]);
                        }
                        rnode::CMD_FREQUENCY
                        | rnode::CMD_BANDWIDTH
                        | rnode::CMD_TXPOWER
                        | rnode::CMD_SF
                        | rnode::CMD_CR
                        | rnode::CMD_RADIO_STATE => {
                            // Echo the requested value back as confirmation.
                            push(&mut reply, command, &payload);
                        }
                        rnode::CMD_DATA => {
                            let _ = got_outbound.try_send(payload.to_vec());
                            if !injected {
                                injected = true;
                                push(&mut reply, rnode::CMD_DATA, &inbound);
                            }
                        }
                        _ => {}
                    }
                }
            }
            if !reply.is_empty() && peer.write_all(&reply).await.is_err() {
                return;
            }
        }
    }

    // #19: a host-supplied byte channel drives the full RNode lifecycle —
    // detect → configure → online → outbound frame → inbound frame — with no
    // serial port, via spawn_rnode_channel_interface + RNodeChannelFactory.
    #[tokio::test]
    async fn test_rnode_channel_interface_lifecycle() {
        // The factory hands leviculum one (split) half of an in-memory duplex;
        // the firmware stub owns the other half.
        let (port, peer) = tokio::io::duplex(64 * 1024);
        let (read_half, write_half) = tokio::io::split(port);
        let halves: std::sync::Mutex<Option<RNodeChannelHalves>> = std::sync::Mutex::new(Some((
            Box::new(read_half) as Box<dyn AsyncRead + Send + Unpin>,
            Box::new(write_half) as Box<dyn AsyncWrite + Send + Unpin>,
        )));

        struct MockFactory(std::sync::Mutex<Option<RNodeChannelHalves>>);
        impl RNodeChannelFactory for MockFactory {
            fn open(&self) -> RNodeChannelOpenFuture {
                let taken = self.0.lock().unwrap().take();
                Box::pin(async move { taken.ok_or_else(|| "channel already opened".into()) })
            }
        }

        let (got_tx, mut got_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let stub = tokio::spawn(async move {
            rnode_firmware_stub(peer, b"inbound-over-channel".to_vec(), got_tx).await;
        });

        let mut handle = spawn_rnode_channel_interface(
            RNodeChannelInterfaceConfig {
                id: InterfaceId(0),
                name: "rnode_channel_test".to_string(),
                channel_factory: Arc::new(MockFactory(halves)),
                frequency: 868_000_000,
                bandwidth: 125_000,
                tx_power: 17,
                sf: 7,
                cr: 5,
                st_alock: None,
                lt_alock: None,
                flow_control: false,
                buffer_size: RNODE_DEFAULT_BUFFER_SIZE,
                reconnect_notify: None,
            },
            None,
        );

        // Bitrate is computed at spawn from the radio params.
        assert!(handle.info.bitrate.is_some());

        // Queue an outbound packet. Once detect+configure complete and the I/O
        // phase begins, it must traverse the channel to the firmware stub.
        handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: b"outbound-over-channel".to_vec(),
                high_priority: false,
            })
            .await
            .expect("send to interface");

        // The stub records the outbound payload (proves detect → configure →
        // online completed over the channel) and injects an inbound frame in
        // reply, which must surface on the interface's incoming channel.
        let outbound = tokio::time::timeout(Duration::from_secs(8), got_rx.recv())
            .await
            .expect("outbound CMD_DATA must reach the stub within 8s (lifecycle reached online)")
            .expect("got_outbound channel open");
        assert_eq!(outbound, b"outbound-over-channel");

        let incoming = tokio::time::timeout(Duration::from_secs(8), handle.incoming.recv())
            .await
            .expect("inbound packet must arrive within 8s")
            .expect("incoming channel open");
        assert_eq!(incoming.data, b"inbound-over-channel");

        stub.abort();
    }

    // L-0024: `ready` must reflect the RNode lifecycle (channel open →
    // firmware probe → radio config), not handle construction. A caller
    // that sends after a construction-time `ready` sends into a port that
    // may not even answer the detect probe yet.
    #[tokio::test]
    async fn test_rnode_channel_ready_waits_for_configuration() {
        struct MockFactory(std::sync::Mutex<Option<RNodeChannelHalves>>);
        impl RNodeChannelFactory for MockFactory {
            fn open(&self) -> RNodeChannelOpenFuture {
                let taken = self.0.lock().unwrap().take();
                Box::pin(async move { taken.ok_or_else(|| "channel already opened".into()) })
            }
        }
        let config =
            |factory: Arc<dyn RNodeChannelFactory>, name: &str| RNodeChannelInterfaceConfig {
                id: InterfaceId(0),
                name: name.to_string(),
                channel_factory: factory,
                frequency: 868_000_000,
                bandwidth: 125_000,
                tx_power: 17,
                sf: 7,
                cr: 5,
                st_alock: None,
                lt_alock: None,
                flow_control: false,
                buffer_size: RNODE_DEFAULT_BUFFER_SIZE,
                reconnect_notify: None,
            };
        let halves_of = |port: tokio::io::DuplexStream| {
            let (read_half, write_half) = tokio::io::split(port);
            std::sync::Mutex::new(Some((
                Box::new(read_half) as Box<dyn AsyncRead + Send + Unpin>,
                Box::new(write_half) as Box<dyn AsyncWrite + Send + Unpin>,
            )))
        };

        // (a) A channel whose far end never answers the detect probe: the
        // interface must NOT report ready.
        let (port, silent_peer) = tokio::io::duplex(64 * 1024);
        let handle = spawn_rnode_channel_interface(
            config(Arc::new(MockFactory(halves_of(port))), "rnode_ready_silent"),
            None,
        );
        assert!(
            handle.ready.wait(Duration::from_millis(300)).await.is_err(),
            "L-0024: ready fired although the firmware probe never answered"
        );
        drop(silent_peer);
        drop(handle);

        // (b) Positive control: with a firmware stub that answers detect and
        // confirms the radio config, ready fires.
        let (port, peer) = tokio::io::duplex(64 * 1024);
        let (got_tx, _got_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let stub = tokio::spawn(async move {
            rnode_firmware_stub(peer, Vec::new(), got_tx).await;
        });
        let handle = spawn_rnode_channel_interface(
            config(Arc::new(MockFactory(halves_of(port))), "rnode_ready_probed"),
            None,
        );
        handle
            .ready
            .wait(Duration::from_secs(8))
            .await
            .expect("ready must fire once detect+configure completed");
        stub.abort();
    }

    /// Minimal firmware stub that answers detect + echoes config confirmations,
    /// signals once the radio is switched on (`configured`), and signals again
    /// when the channel closes (`closed`) — i.e. when the interface task is torn
    /// down. Used by the runtime attach/detach test.
    async fn rnode_firmware_stub_signals(
        mut peer: tokio::io::DuplexStream,
        configured: tokio::sync::mpsc::Sender<()>,
        closed: tokio::sync::mpsc::Sender<()>,
    ) {
        let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
        let mut buf = [0u8; 512];
        let push = |reply: &mut Vec<u8>, cmd: u8, payload: &[u8]| {
            let mut one = Vec::new();
            kiss::frame(cmd, payload, &mut one);
            reply.extend_from_slice(&one);
        };
        loop {
            let n = match peer.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let _ = closed.try_send(());
                    return;
                }
                Ok(n) => n,
            };
            let mut reply: Vec<u8> = Vec::new();
            for f in deframer.process(&buf[..n]) {
                if let KissDeframeResult::Frame { command, payload } = f {
                    match command {
                        rnode::CMD_DETECT => {
                            push(&mut reply, rnode::CMD_DETECT, &[rnode::DETECT_RESP]);
                            push(
                                &mut reply,
                                rnode::CMD_FW_VERSION,
                                &[rnode::REQUIRED_FW_MAJ, rnode::REQUIRED_FW_MIN],
                            );
                            push(&mut reply, rnode::CMD_PLATFORM, &[rnode::PLATFORM_ESP32]);
                            push(&mut reply, rnode::CMD_MCU, &[0x00]);
                        }
                        rnode::CMD_FREQUENCY
                        | rnode::CMD_BANDWIDTH
                        | rnode::CMD_TXPOWER
                        | rnode::CMD_SF
                        | rnode::CMD_CR => push(&mut reply, command, &payload),
                        rnode::CMD_RADIO_STATE => {
                            push(&mut reply, command, &payload);
                            let _ = configured.try_send(());
                        }
                        _ => {}
                    }
                }
            }
            if !reply.is_empty() && peer.write_all(&reply).await.is_err() {
                let _ = closed.try_send(());
                return;
            }
        }
    }

    // #19 follow-up: attach a channel-backed RNode interface to a RUNNING node
    // at runtime (hot-plug), then detach it by dropping the handle.
    #[tokio::test]
    async fn test_runtime_attach_detach_rnode_channel() {
        use crate::driver::ReticulumNodeBuilder;

        let td = tempfile::tempdir().expect("tempdir");
        let mut node = ReticulumNodeBuilder::new()
            .enable_transport(true)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync");
        node.start().await.expect("start");

        // Wire a mock radio over an in-memory duplex.
        let (port, peer) = tokio::io::duplex(64 * 1024);
        let (read_half, write_half) = tokio::io::split(port);
        let halves: std::sync::Mutex<Option<RNodeChannelHalves>> = std::sync::Mutex::new(Some((
            Box::new(read_half) as Box<dyn AsyncRead + Send + Unpin>,
            Box::new(write_half) as Box<dyn AsyncWrite + Send + Unpin>,
        )));
        struct MockFactory(std::sync::Mutex<Option<RNodeChannelHalves>>);
        impl RNodeChannelFactory for MockFactory {
            fn open(&self) -> RNodeChannelOpenFuture {
                let taken = self.0.lock().unwrap().take();
                Box::pin(async move { taken.ok_or_else(|| "channel already opened".into()) })
            }
        }

        let (cfg_tx, mut cfg_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (closed_tx, mut closed_rx) = tokio::sync::mpsc::channel::<()>(1);
        let stub = tokio::spawn(rnode_firmware_stub_signals(peer, cfg_tx, closed_tx));

        // Hot-plug: attach the radio to the already-running node.
        let handle = node
            .spawn_rnode_channel_interface(RNodeChannelConfig {
                factory: Arc::new(MockFactory(halves)),
                frequency: 868_000_000,
                bandwidth: 125_000,
                tx_power: 17,
                sf: 7,
                cr: 5,
                st_alock: None,
                lt_alock: None,
                flow_control: false,
                buffer_size: RNODE_DEFAULT_BUFFER_SIZE,
            })
            .expect("attach must succeed on a running node");

        // The interface ran its detect → configure lifecycle over the channel.
        tokio::time::timeout(Duration::from_secs(8), cfg_rx.recv())
            .await
            .expect("interface must reach configured within 8s (runtime attach worked)")
            .expect("cfg channel open");

        // Detach by dropping the handle: the task stops and the channel closes.
        handle.detach();
        tokio::time::timeout(Duration::from_secs(8), closed_rx.recv())
            .await
            .expect("channel must close within 8s of dropping the handle (detach worked)")
            .expect("closed channel open");

        stub.abort();
        node.stop().await.expect("stop");
    }

    // Codeberg #136 follow-up: `InterfaceStatusSnapshot.interface_id` exists so
    // a caller holding a runtime handle can pair it with a snapshot without
    // matching on the name. That only holds if the id in the snapshot is the
    // same id the handle reports, which is what this asserts — together with
    // the `kind` from #140, since both fields are populated from the same
    // registry entry and a hot-plugged interface is the case that exercises
    // the dynamic-registration path rather than the config path.
    #[tokio::test]
    async fn test_interface_stats_id_pairs_with_runtime_handle() {
        use crate::driver::ReticulumNodeBuilder;

        let td = tempfile::tempdir().expect("tempdir");
        let mut node = ReticulumNodeBuilder::new()
            .enable_transport(true)
            .storage_path(td.path().to_path_buf())
            .build_sync()
            .expect("build_sync");
        node.start().await.expect("start");

        let (port, peer) = tokio::io::duplex(64 * 1024);
        let (read_half, write_half) = tokio::io::split(port);
        let halves: std::sync::Mutex<Option<RNodeChannelHalves>> = std::sync::Mutex::new(Some((
            Box::new(read_half) as Box<dyn AsyncRead + Send + Unpin>,
            Box::new(write_half) as Box<dyn AsyncWrite + Send + Unpin>,
        )));
        struct MockFactory(std::sync::Mutex<Option<RNodeChannelHalves>>);
        impl RNodeChannelFactory for MockFactory {
            fn open(&self) -> RNodeChannelOpenFuture {
                let taken = self.0.lock().unwrap().take();
                Box::pin(async move { taken.ok_or_else(|| "channel already opened".into()) })
            }
        }

        let (cfg_tx, mut cfg_rx) = tokio::sync::mpsc::channel::<()>(1);
        let (closed_tx, _closed_rx) = tokio::sync::mpsc::channel::<()>(1);
        let stub = tokio::spawn(rnode_firmware_stub_signals(peer, cfg_tx, closed_tx));

        let handle = node
            .spawn_rnode_channel_interface(RNodeChannelConfig {
                factory: Arc::new(MockFactory(halves)),
                frequency: 868_000_000,
                bandwidth: 125_000,
                tx_power: 17,
                sf: 7,
                cr: 5,
                st_alock: None,
                lt_alock: None,
                flow_control: false,
                buffer_size: RNODE_DEFAULT_BUFFER_SIZE,
            })
            .expect("attach must succeed on a running node");
        tokio::time::timeout(Duration::from_secs(8), cfg_rx.recv())
            .await
            .expect("interface must reach configured within 8s")
            .expect("cfg channel open");

        // The snapshot list must contain exactly the handle's id, and that
        // entry must be the radio (not some other interface that happened to
        // land on the same index).
        let mut entry = None;
        for _ in 0..80 {
            entry = node
                .interface_stats()
                .into_iter()
                .find(|e| e.interface_id == handle.id());
            if entry.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let entry = entry.unwrap_or_else(|| {
            panic!(
                "no snapshot carried the handle's id {:?}; snapshots = {:?}",
                handle.id(),
                node.interface_stats()
                    .iter()
                    .map(|e| (e.interface_id, e.name.clone()))
                    .collect::<Vec<_>>()
            )
        });
        assert_eq!(
            entry.kind,
            leviculum_core::traits::InterfaceKind::Rnode,
            "the entry paired by id must be the hot-plugged radio"
        );
        assert!(
            entry.name.starts_with("rnode_channel_"),
            "paired entry name should be the radio's, got {:?}",
            entry.name
        );

        handle.detach();
        stub.abort();
        node.stop().await.expect("stop");
    }

    #[tokio::test]
    #[ignore] // Requires RNode hardware at /dev/ttyACM0
    async fn test_configure_real_rnode() {
        let radio = RadioParams {
            frequency: 868_000_000,
            bandwidth: 125_000,
            tx_power: 17,
            tx_power_derived: false,
            sf: 7,
            cr: 5,
            st_alock: None,
            lt_alock: None,
        };
        let result = configure_rnode("/dev/ttyACM0", &radio).await;

        match result {
            Ok((mut port, detect)) => {
                println!("RNode configured successfully!");
                if let Some((major, minor)) = detect.firmware_version {
                    println!("  Firmware: {major}.{minor}");
                }
                if let Some(platform) = detect.platform {
                    let name = match platform {
                        rnode::PLATFORM_ESP32 => "ESP32",
                        rnode::PLATFORM_NRF52 => "nRF52",
                        rnode::PLATFORM_AVR => "AVR",
                        _ => "Unknown",
                    };
                    println!("  Platform: {name} (0x{platform:02X})");
                }
                if let Some(mcu) = detect.mcu {
                    println!("  MCU: 0x{mcu:02X}");
                }
                // Turn radio off and send leave
                send_goodbye(&mut port, "test_rnode").await;
                println!("  Radio off, leave sent");
            }
            Err(e) => {
                panic!("Configuration failed: {e}");
            }
        }
    }

    #[tokio::test]
    #[ignore] // Requires RNode hardware at /dev/ttyACM0
    async fn test_rnode_interface_lifecycle() {
        let config = RNodeInterfaceConfig {
            id: InterfaceId(0),
            name: "test_rnode".to_string(),
            port_path: "/dev/ttyACM0".to_string(),
            frequency: 868_000_000,
            bandwidth: 125_000,
            tx_power: 17,
            tx_power_derived: false,
            sf: 7,
            cr: 5,
            st_alock: None,
            lt_alock: None,
            flow_control: true,
            buffer_size: RNODE_DEFAULT_BUFFER_SIZE,
            reconnect_notify: None,
            test_drop_direct_ingress: false,
        };

        let mut handle = spawn_rnode_interface(config);

        // Wait for the interface to come online (~5s for detect + configure)
        println!("Waiting for RNode to come online...");
        tokio::time::sleep(Duration::from_secs(6)).await;

        assert!(
            handle.info.bitrate.is_some(),
            "bitrate should be computed at spawn"
        );
        println!("Bitrate: {} bps", handle.info.bitrate.unwrap());

        // Send a test packet via outgoing channel
        let test_data = b"Hello from Rust RNode test";
        handle
            .outgoing
            .send(OutgoingPacket {
                peer: None,
                data: test_data.to_vec(),
                high_priority: false,
            })
            .await
            .expect("send should succeed");
        println!("Sent test packet ({} bytes)", test_data.len());

        // Brief wait, then drop the handle to trigger shutdown
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Drop outgoing sender to signal shutdown
        drop(handle.outgoing);

        // Read remaining incoming (drain)
        while let Ok(pkt) = handle.incoming.try_recv() {
            println!("Received: {} bytes", pkt.data.len());
        }

        println!("Interface lifecycle test complete");
    }

    /// Reproduce the flow-control startup deadlock without hardware.
    ///
    /// With `flow_control = true`, the io task historically initialised
    /// `interface_ready = false` and only flipped it on receipt of a
    /// `CMD_READY` (0x0F) frame. The RNode firmware sends `CMD_READY`
    /// **after** each TX as a "I can accept the next frame" signal — never
    /// spontaneously after init. That produced a chicken-and-egg stall: no
    /// TX ⇒ no `CMD_READY` ⇒ no TX, ever. Observed on miauhaus 2026-04-29
    /// as 0 bytes TX in 24 minutes uptime while the send queue spammed
    /// "send queue full, dropping oldest".
    ///
    /// Test fails (timeout) before the fix, passes after.
    #[tokio::test]
    async fn test_flow_control_initial_ready_no_stall() {
        // tokio::io::duplex gives us a pair of in-memory streams: `port` is
        // what the io task drives; `peer` simulates the RNode-side serial
        // endpoint that we read TX bytes from. The peer never sends
        // CMD_READY — the test's whole point is that the first TX must go
        // through *without* one.
        let (port, mut peer) = tokio::io::duplex(8192);

        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(16);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingPacket>(16);
        let counters = Arc::new(InterfaceCounters::new());

        // jitter_max_ms = 1 so the random pre-TX jitter is effectively zero
        // and the test does not depend on a long tail.
        let task_counters = Arc::clone(&counters);
        let task = tokio::spawn(async move {
            rnode_io_task(
                "test_rnode".to_string(),
                port,
                incoming_tx,
                outgoing_rx,
                task_counters,
                /* flow_control = */ true,
                /* jitter_max_ms = */ 1,
                125_000,
                7,
                5,
                /* drop_direct_ingress = */ false,
            )
            .await;
        });

        // Submit one outgoing packet. With flow_control = true and the
        // fixed initial-ready bug, this must reach `peer` within a short
        // timeout. With the bug, the io task stalls waiting for
        // CMD_READY and `peer.read` times out.
        let payload = b"hello";
        outgoing_tx
            .send(OutgoingPacket {
                peer: None,
                data: payload.to_vec(),
                high_priority: false,
            })
            .await
            .expect("send to io task");

        let mut buf = [0u8; 64];
        let n = tokio::time::timeout(Duration::from_secs(1), peer.read(&mut buf))
            .await
            .expect("io task must send first frame within 1s (would deadlock if interface_ready stayed false)")
            .expect("read from duplex");

        assert!(n >= 3, "expected at least a 3-byte KISS frame, got {n}");
        // KISS data frame: FEND CMD_DATA payload FEND
        assert_eq!(buf[0], kiss::FEND, "first byte should be KISS FEND");
        assert_eq!(buf[1], rnode::CMD_DATA, "second byte should be CMD_DATA");

        let tx_bytes = counters.tx_bytes.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            tx_bytes,
            payload.len() as usize,
            "tx_bytes counter must reflect the dispatched payload"
        );

        drop(outgoing_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
    }

    /// Read KISS frames from `peer` until `timeout` elapses or no more bytes
    /// arrive. Returns every successfully deframed `(command, payload)` pair
    /// in arrival order. Used by the multi-frame send tests below.
    async fn drain_kiss_frames<S: tokio::io::AsyncReadExt + Unpin>(
        peer: &mut S,
        timeout: Duration,
    ) -> Vec<(u8, Vec<u8>)> {
        let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
        let mut frames: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut buf = [0u8; 256];
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, peer.read(&mut buf)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    for f in deframer.process(&buf[..n]) {
                        if let KissDeframeResult::Frame { command, payload } = f {
                            frames.push((command, payload.to_vec()));
                        }
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        frames
    }

    /// The TEST-ONLY `test_drop_direct_ingress` knob at the interface level
    /// (deframe → filter, no daemon): a deframed CMD_DATA frame whose wire
    /// hops byte (`raw[1]`) is 0 must be dropped before the transport
    /// channel, a relayed copy (hops >= 1) must pass, and with the knob off
    /// (the default) the hops-0 frame passes too.
    #[tokio::test]
    async fn test_drop_direct_ingress_filters_hops0_at_the_rnode_boundary() {
        async fn rx_through_io_task(
            drop_direct: bool,
            frames: &[Vec<u8>],
        ) -> (Vec<Vec<u8>>, Arc<InterfaceCounters>) {
            let (port, mut peer) = tokio::io::duplex(8192);
            let (incoming_tx, mut incoming_rx) = mpsc::channel::<IncomingPacket>(16);
            let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingPacket>(16);
            let counters = Arc::new(InterfaceCounters::new());
            let task_counters = Arc::clone(&counters);
            let task = tokio::spawn(async move {
                rnode_io_task(
                    "test_rnode_deaf".to_string(),
                    port,
                    incoming_tx,
                    outgoing_rx,
                    task_counters,
                    /* flow_control = */ false,
                    /* jitter_max_ms = */ 1,
                    125_000,
                    7,
                    5,
                    drop_direct,
                )
                .await;
            });
            for f in frames {
                let mut out = Vec::new();
                kiss::frame(rnode::CMD_DATA, f, &mut out);
                peer.write_all(&out).await.expect("write frame");
            }
            let mut got = Vec::new();
            while let Ok(Some(pkt)) =
                tokio::time::timeout(Duration::from_millis(300), incoming_rx.recv()).await
            {
                got.push(pkt.data);
            }
            drop(outgoing_tx);
            drop(peer);
            let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
            (got, counters)
        }

        // flags(1) hops(1) tail — the filter reads only raw[1]. A frame
        // transmitted by its originator carries wire hops 0; a copy relayed
        // by one transport hop carries 1.
        let direct = vec![0x00, 0x00, 0xAA, 0xBB];
        let relayed = vec![0x00, 0x01, 0xAA, 0xBB];

        // Knob on: the direct frame vanishes before the transport channel
        // and is counted; the relayed copy arrives.
        let (got, counters) = rx_through_io_task(true, &[direct.clone(), relayed.clone()]).await;
        assert_eq!(got, vec![relayed.clone()], "only the relayed copy passes");
        assert_eq!(
            counters
                .test_direct_ingress_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            counters.rx_bytes.load(std::sync::atomic::Ordering::Relaxed),
            relayed.len() as u64,
            "a dropped frame was never heard, so it must not count as RX"
        );

        // Default off: both frames arrive, nothing is counted as dropped.
        let (got, counters) = rx_through_io_task(false, &[direct.clone(), relayed.clone()]).await;
        assert_eq!(got, vec![direct, relayed]);
        assert_eq!(
            counters
                .test_direct_ingress_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    /// The filter announces itself (Codeberg #223): when
    /// `test_drop_direct_ingress` is on, the io task emits exactly the
    /// armed line the periculum cells assert on — from the same task that
    /// holds the flag the drop filter reads, so the line proves the
    /// production drop path is live and not merely that a config key
    /// parsed. Off (the default), the line must be absent.
    #[tokio::test]
    async fn test_drop_direct_ingress_announces_arming_at_the_rnode_boundary() {
        async fn logs_from_io_task(drop_direct: bool) -> String {
            let (buf, _guard) = capture_logs();
            let (port, peer) = tokio::io::duplex(8192);
            let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(16);
            let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingPacket>(16);
            let counters = Arc::new(InterfaceCounters::new());
            let task = tokio::spawn(async move {
                rnode_io_task(
                    "test_rnode_armed".to_string(),
                    port,
                    incoming_tx,
                    outgoing_rx,
                    counters,
                    /* flow_control = */ false,
                    /* jitter_max_ms = */ 1,
                    125_000,
                    7,
                    5,
                    drop_direct,
                )
                .await;
            });
            // Let the io task start and emit (or not emit) the armed line,
            // then shut it down by closing its peer and channels.
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(outgoing_tx);
            drop(peer);
            let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
            let captured = buf.lock().unwrap();
            String::from_utf8_lossy(&captured).into_owned()
        }

        let logs = logs_from_io_task(true).await;
        assert!(
            logs.contains("DIRECT_INGRESS_FILTER armed iface=test_rnode_armed"),
            "armed line missing with the knob on; logs:\n{logs}"
        );

        let logs = logs_from_io_task(false).await;
        assert!(
            !logs.contains("DIRECT_INGRESS_FILTER"),
            "armed line must be absent with the knob off; logs:\n{logs}"
        );
    }

    /// A data frame's RX lines carry the RSSI/SNR the firmware indicated
    /// immediately before it (Codeberg #364). The mocked KISS stream
    /// reproduces the firmware's order on every MCU variant — CMD_STAT_RSSI,
    /// CMD_STAT_SNR, then the CMD_DATA frame (RNode_Firmware.ino:1689-1691)
    /// — and the pairing is the reference's last-seen one
    /// (RNodeInterface.py:877-880): the stats stored when CMD_DATA arrives
    /// are that frame's own report. A frame no stat frame preceded logs the
    /// bare line, not an invented value.
    #[tokio::test]
    async fn test_rx_lines_carry_the_preceding_stat_frames_rssi_and_snr() {
        let (buf, _guard) = capture_logs();
        let (port, mut peer) = tokio::io::duplex(8192);
        let (incoming_tx, mut incoming_rx) = mpsc::channel::<IncomingPacket>(16);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingPacket>(16);
        let counters = Arc::new(InterfaceCounters::new());
        let task = tokio::spawn(async move {
            rnode_io_task(
                "test_rnode_signal".to_string(),
                port,
                incoming_tx,
                outgoing_rx,
                counters,
                /* flow_control = */ false,
                /* jitter_max_ms = */ 1,
                125_000,
                7,
                5,
                /* drop_direct_ingress = */ false,
            )
            .await;
        });

        // The mocked KISS stream, in the firmware's order: a data frame no
        // stat frame preceded (a mock, an older firmware), then a real
        // reception's RSSI stat, SNR stat, data frame. Raw 100 is 100-157 =
        // -57 dBm; raw 21 is 21*0.25 = 5.25 dB, proving the scaled value
        // (not the raw byte) is logged. `kiss::frame` clears its output, so
        // each frame is built and written on its own.
        let mut wire = Vec::new();
        for (command, payload) in [
            (rnode::CMD_DATA, &[0x00, 0x00, 0x01][..]),
            (rnode::CMD_STAT_RSSI, &[100][..]),
            (rnode::CMD_STAT_SNR, &[21][..]),
            (rnode::CMD_DATA, &[0x00, 0x00, 0x02, 0x03][..]),
        ] {
            kiss::frame(command, payload, &mut wire);
            peer.write_all(&wire)
                .await
                .expect("write mocked KISS stream");
        }

        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(2), incoming_rx.recv())
                .await
                .expect("data frame within 2s")
                .expect("incoming channel open");
        }
        drop(outgoing_tx);
        drop(peer);
        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;

        let captured = buf.lock().unwrap();
        let logs = String::from_utf8_lossy(&captured);
        let rx_events: Vec<&str> = logs.lines().filter(|l| l.contains("LORA_RX")).collect();
        assert_eq!(rx_events.len(), 2, "two LORA_RX events; logs:\n{logs}");
        assert!(
            rx_events[0].contains("len=3")
                && !rx_events[0].contains("rssi=")
                && !rx_events[0].contains("snr="),
            "the un-preceded frame carries no signal keys: {}",
            rx_events[0]
        );
        assert!(
            rx_events[1].contains("len=4 rssi=-57 snr=5.25"),
            "the preceded frame carries the paired stats: {}",
            rx_events[1]
        );
        assert!(
            logs.contains("RX 4 bytes from radio rssi=-57 snr=5.25"),
            "the human RX line carries the same pair; logs:\n{logs}"
        );
    }

    /// Steady-state throughput sanity for the default `flow_control = false`
    /// path: pushed packets must all reach `peer` without anyone feeding
    /// CMD_READY back. This is the configuration that lnsd (and Python-RNS)
    /// actually defaults to and that miauhaus runs after the 2026-04-30
    /// flow_control flip. Guards against a regression where someone
    /// reintroduces a hidden CMD_READY-gate on the non-flow-control path.
    #[tokio::test]
    async fn test_no_flow_control_multi_frame_throughput() {
        let (port, mut peer) = tokio::io::duplex(8192);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(16);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingPacket>(16);
        let counters = Arc::new(InterfaceCounters::new());

        let task_counters = Arc::clone(&counters);
        let task = tokio::spawn(async move {
            rnode_io_task(
                "test_rnode".to_string(),
                port,
                incoming_tx,
                outgoing_rx,
                task_counters,
                /* flow_control = */ false,
                /* jitter_max_ms = */ 1,
                125_000,
                7,
                5,
                /* drop_direct_ingress = */ false,
            )
            .await;
        });

        let payloads: [&[u8]; 3] = [b"alpha", b"bravo", b"charlie"];
        for p in payloads.iter() {
            outgoing_tx
                .send(OutgoingPacket {
                    peer: None,
                    data: p.to_vec(),
                    high_priority: false,
                })
                .await
                .expect("send to io task");
        }

        // 3 × MIN_SPACING_MS (50ms) + jitter (~1ms) + serial latency.
        // 1s is generous; the io task should drain all three within ~150ms.
        let frames = drain_kiss_frames(&mut peer, Duration::from_secs(1)).await;
        let data_frames: Vec<&Vec<u8>> = frames
            .iter()
            .filter(|(c, _)| *c == rnode::CMD_DATA)
            .map(|(_, p)| p)
            .collect();

        assert_eq!(
            data_frames.len(),
            3,
            "all three frames must reach the peer; got {} CMD_DATA frames out of {} total",
            data_frames.len(),
            frames.len()
        );
        for (i, p) in payloads.iter().enumerate() {
            assert_eq!(
                data_frames[i].as_slice(),
                *p,
                "frame {} payload mismatch",
                i
            );
        }

        let tx_bytes = counters.tx_bytes.load(std::sync::atomic::Ordering::Relaxed);
        let total_payload: usize = payloads.iter().map(|p| p.len()).sum();
        assert_eq!(
            tx_bytes, total_payload as u64,
            "tx_bytes counter must sum payloads"
        );

        drop(outgoing_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
    }

    // The former `test_flow_control_with_cmd_ready_multi_frame_throughput`
    // encoded the refuted protocol model — an unsolicited CMD_READY after
    // every TX. The firmware never sends one (`RNode_Firmware.ino:1003-1008`
    // answers only a host query; `kiss_indicate_ready`, `Utilities.h:1157`,
    // has no other call site), so the round-trip it guarded cannot occur on
    // hardware. The corrected query/response round-trip is covered by the
    // scripted-stub suite below.

    // -----------------------------------------------------------------------
    // Multi-vport (RNodeMultiInterface) tests
    // -----------------------------------------------------------------------

    /// In-process multi-vport RNode firmware stub over one half of a duplex.
    ///
    /// Answers the multi detect probe (detected + firmware + platform + MCU +
    /// CMD_INTERFACES chip-type report), tracks the selected vport from
    /// CMD_SEL_INT, records the frequency configured for each vport, and for
    /// every CMD_DATA it receives records `(vport, payload)` and echoes the same
    /// payload back tagged with that vport. This lets a test prove both that TX
    /// frames carry the right vport and that RX frames route to the right
    /// logical interface.
    async fn rnode_multi_firmware_stub(
        mut peer: tokio::io::DuplexStream,
        chip_types: Vec<u8>,
        freq_report: tokio::sync::mpsc::Sender<(u8, u32)>,
        data_report: tokio::sync::mpsc::Sender<(u8, Vec<u8>)>,
    ) {
        let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
        let mut buf = [0u8; 1024];
        let mut selected: u8 = 0;
        let push = |reply: &mut Vec<u8>, cmd: u8, payload: &[u8]| {
            let mut one = Vec::new();
            kiss::frame(cmd, payload, &mut one);
            reply.extend_from_slice(&one);
        };
        loop {
            let n = match peer.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut reply: Vec<u8> = Vec::new();
            for f in deframer.process(&buf[..n]) {
                let KissDeframeResult::Frame { command, payload } = f else {
                    continue;
                };
                match command {
                    rnode::CMD_DETECT => {
                        push(&mut reply, rnode::CMD_DETECT, &[rnode::DETECT_RESP]);
                        push(
                            &mut reply,
                            rnode::CMD_FW_VERSION,
                            &[rnode::REQUIRED_FW_MAJ, rnode::REQUIRED_FW_MIN],
                        );
                        push(&mut reply, rnode::CMD_PLATFORM, &[rnode::PLATFORM_NRF52]);
                        push(&mut reply, rnode::CMD_MCU, &[0x00]);
                    }
                    rnode::CMD_INTERFACES => {
                        // Report one 2-byte record per vport: [0x00, chip_type].
                        let mut rep = Vec::with_capacity(chip_types.len() * 2);
                        for &t in &chip_types {
                            rep.push(0x00);
                            rep.push(t);
                        }
                        push(&mut reply, rnode::CMD_INTERFACES, &rep);
                    }
                    rnode::CMD_SEL_INT => {
                        if let Some(v) = rnode::decode_select_interface(&payload) {
                            selected = v;
                        }
                    }
                    rnode::CMD_FREQUENCY if payload.len() >= 4 => {
                        let hz =
                            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                        let _ = freq_report.try_send((selected, hz));
                    }
                    rnode::CMD_DATA => {
                        let _ = data_report.try_send((selected, payload.to_vec()));
                        // Echo the payload back tagged with the same vport, using
                        // the same SEL_INT + CMD_DATA framing the host uses.
                        reply.extend_from_slice(&rnode::build_vport_data_frame(selected, &payload));
                    }
                    // Drain the rest of the per-vport config commands silently.
                    _ => {}
                }
            }
            if !reply.is_empty() && peer.write_all(&reply).await.is_err() {
                return;
            }
        }
    }

    /// Drive the full multi-vport exchange against the mock firmware: configure
    /// two vports over one shared serial link, then prove that (a) each vport's
    /// radio config is pushed under the correct CMD_SEL_INT, (b) a packet sent on
    /// a vport's logical interface reaches the firmware tagged with that vport,
    /// and (c) a frame the firmware emits tagged with a vport routes back to that
    /// vport's logical interface and no other.
    #[tokio::test]
    async fn test_multi_vport_config_and_routing() {
        let (port, peer) = tokio::io::duplex(64 * 1024);

        let (freq_tx, mut freq_rx) = tokio::sync::mpsc::channel::<(u8, u32)>(8);
        let (data_tx, mut data_rx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(8);
        // Two vports: vport 0 = SX127X (sub-GHz), vport 1 = SX128X (2.4 GHz).
        let stub = tokio::spawn(rnode_multi_firmware_stub(
            peer,
            vec![rnode::CHIP_SX127X, rnode::CHIP_SX128X],
            freq_tx,
            data_tx,
        ));

        // Build the two vport runtimes with their own incoming channels.
        let (in0_tx, mut in0_rx) = mpsc::channel::<IncomingPacket>(16);
        let (in1_tx, mut in1_rx) = mpsc::channel::<IncomingPacket>(16);
        let vports = vec![
            VportRuntime {
                id: InterfaceId(10),
                name: "multi[low]".to_string(),
                vport: 0,
                radio: RadioParams {
                    frequency: 865_600_000,
                    bandwidth: 125_000,
                    tx_power: 0,
                    tx_power_derived: false,
                    sf: 7,
                    cr: 5,
                    st_alock: None,
                    lt_alock: None,
                },
                outgoing: true,
                incoming_tx: in0_tx,
                counters: Arc::new(InterfaceCounters::new()),
            },
            VportRuntime {
                id: InterfaceId(11),
                name: "multi[high]".to_string(),
                vport: 1,
                radio: RadioParams {
                    frequency: 2_400_000_000,
                    bandwidth: 500_000,
                    tx_power: 0,
                    tx_power_derived: false,
                    sf: 5,
                    cr: 5,
                    st_alock: None,
                    lt_alock: None,
                },
                outgoing: true,
                incoming_tx: in1_tx,
                counters: Arc::new(InterfaceCounters::new()),
            },
        ];

        // A connector that yields the (single) duplex port on first call.
        let port_holder = std::sync::Mutex::new(Some(port));
        let connect = move || {
            let taken = port_holder.lock().unwrap().take();
            async move { taken.ok_or(RNodeError::NotDetected) }
        };

        let (merged_tx, merged_rx) = mpsc::channel::<TaggedOutgoing>(16);
        let hub = tokio::spawn(async move {
            rnode_multi_reconnect_task(
                "multi".to_string(),
                connect,
                vports,
                merged_rx,
                /* flow_control = */ false,
                None,
            )
            .await;
        });

        // (a) Both vports' frequencies were pushed under their own SEL_INT.
        let mut freqs = std::collections::HashMap::new();
        for _ in 0..2 {
            let (vport, hz) = tokio::time::timeout(Duration::from_secs(5), freq_rx.recv())
                .await
                .expect("frequency config must be pushed within 5s")
                .expect("freq channel open");
            freqs.insert(vport, hz);
        }
        assert_eq!(freqs.get(&0), Some(&865_600_000));
        assert_eq!(freqs.get(&1), Some(&2_400_000_000));

        // (b) A packet on vport 0's logical interface reaches the firmware
        // tagged vport 0, and (c) echoes back onto vport 0's incoming channel.
        merged_tx
            .send(TaggedOutgoing {
                subint: 0,
                packet: OutgoingPacket {
                    peer: None,
                    data: b"ping0".to_vec(),
                    high_priority: false,
                },
            })
            .await
            .expect("send to hub");

        let (rx_vport0, rx_data0) = tokio::time::timeout(Duration::from_secs(5), data_rx.recv())
            .await
            .expect("firmware must receive vport-0 frame within 5s")
            .expect("data channel open");
        assert_eq!(rx_vport0, 0, "TX frame must be tagged vport 0");
        assert_eq!(rx_data0, b"ping0");

        let echoed0 = tokio::time::timeout(Duration::from_secs(5), in0_rx.recv())
            .await
            .expect("echo must route to vport-0 interface within 5s")
            .expect("in0 channel open");
        assert_eq!(echoed0.data, b"ping0");

        // vport 1 must NOT have received vport 0's echo.
        assert!(
            in1_rx.try_recv().is_err(),
            "vport-0 echo must not leak into vport-1's interface"
        );

        // (b/c) repeat for vport 1 to prove the routing is per-vport, not fixed.
        merged_tx
            .send(TaggedOutgoing {
                subint: 1,
                packet: OutgoingPacket {
                    peer: None,
                    data: b"ping1".to_vec(),
                    high_priority: false,
                },
            })
            .await
            .expect("send to hub");

        let (rx_vport1, rx_data1) = tokio::time::timeout(Duration::from_secs(5), data_rx.recv())
            .await
            .expect("firmware must receive vport-1 frame within 5s")
            .expect("data channel open");
        assert_eq!(rx_vport1, 1, "TX frame must be tagged vport 1");
        assert_eq!(rx_data1, b"ping1");

        let echoed1 = tokio::time::timeout(Duration::from_secs(5), in1_rx.recv())
            .await
            .expect("echo must route to vport-1 interface within 5s")
            .expect("in1 channel open");
        assert_eq!(echoed1.data, b"ping1");
        assert!(
            in0_rx.try_recv().is_err(),
            "vport-1 echo must not leak into vport-0's interface"
        );

        hub.abort();
        stub.abort();
    }

    /// A subinterface with `outgoing = false` must never transmit: a packet
    /// queued on it is dropped at the hub, never reaching the firmware.
    #[tokio::test]
    async fn test_multi_vport_non_outgoing_drops_tx() {
        let (port, peer) = tokio::io::duplex(64 * 1024);
        let (freq_tx, mut freq_rx) = tokio::sync::mpsc::channel::<(u8, u32)>(8);
        let (data_tx, mut data_rx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(8);
        let stub = tokio::spawn(rnode_multi_firmware_stub(
            peer,
            vec![rnode::CHIP_SX127X],
            freq_tx,
            data_tx,
        ));

        let (in0_tx, _in0_rx) = mpsc::channel::<IncomingPacket>(16);
        let vports = vec![VportRuntime {
            id: InterfaceId(20),
            name: "multi[rxonly]".to_string(),
            vport: 0,
            radio: RadioParams {
                frequency: 868_000_000,
                bandwidth: 125_000,
                tx_power: 0,
                tx_power_derived: false,
                sf: 7,
                cr: 5,
                st_alock: None,
                lt_alock: None,
            },
            outgoing: false,
            incoming_tx: in0_tx,
            counters: Arc::new(InterfaceCounters::new()),
        }];

        let port_holder = std::sync::Mutex::new(Some(port));
        let connect = move || {
            let taken = port_holder.lock().unwrap().take();
            async move { taken.ok_or(RNodeError::NotDetected) }
        };
        let (merged_tx, merged_rx) = mpsc::channel::<TaggedOutgoing>(16);
        let hub = tokio::spawn(async move {
            rnode_multi_reconnect_task(
                "multi".to_string(),
                connect,
                vports,
                merged_rx,
                false,
                None,
            )
            .await;
        });

        // Wait until configured (frequency pushed) so the io loop is running.
        tokio::time::timeout(Duration::from_secs(5), freq_rx.recv())
            .await
            .expect("configure within 5s")
            .expect("freq channel");

        merged_tx
            .send(TaggedOutgoing {
                subint: 0,
                packet: OutgoingPacket {
                    peer: None,
                    data: b"nope".to_vec(),
                    high_priority: false,
                },
            })
            .await
            .expect("send to hub");

        // The non-outgoing vport must drop it: no CMD_DATA ever reaches the stub.
        let got = tokio::time::timeout(Duration::from_millis(500), data_rx.recv()).await;
        assert!(
            got.is_err(),
            "outgoing=false subinterface must not transmit, but firmware saw data"
        );

        hub.abort();
        stub.abort();
    }

    /// One vport's logical interface going away must not take the shared
    /// physical radio with it (Codeberg #283).
    ///
    /// Before the fix, the send failure on the dead vport's incoming channel
    /// returned from the whole io task. The reconnect loop then reopened the
    /// port (its stop condition needs *every* vport closed), the next frame for
    /// the same dead vport ended it again, and the surviving vports lost the
    /// radio every five seconds forever.
    ///
    /// The connector hands out the duplex port exactly once, so any reconnect
    /// leaves the surviving vport with a dead radio and the round trip below
    /// times out — the bounce cannot pass this test quietly.
    #[tokio::test]
    async fn test_multi_vport_one_dead_vport_does_not_bounce_the_radio() {
        let (port, peer) = tokio::io::duplex(64 * 1024);
        let (freq_tx, _freq_rx) = tokio::sync::mpsc::channel::<(u8, u32)>(8);
        let (data_tx, mut data_rx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(64);
        let stub = tokio::spawn(rnode_multi_firmware_stub(
            peer,
            vec![rnode::CHIP_SX127X, rnode::CHIP_SX128X],
            freq_tx,
            data_tx,
        ));

        let (in0_tx, mut in0_rx) = mpsc::channel::<IncomingPacket>(16);
        let (in1_tx, mut in1_rx) = mpsc::channel::<IncomingPacket>(16);
        let vports = vec![
            VportRuntime {
                id: InterfaceId(40),
                name: "multi[low]".to_string(),
                vport: 0,
                radio: RadioParams {
                    frequency: 865_600_000,
                    bandwidth: 125_000,
                    tx_power: 0,
                    tx_power_derived: false,
                    sf: 7,
                    cr: 5,
                    st_alock: None,
                    lt_alock: None,
                },
                outgoing: true,
                incoming_tx: in0_tx,
                counters: Arc::new(InterfaceCounters::new()),
            },
            VportRuntime {
                id: InterfaceId(41),
                name: "multi[high]".to_string(),
                vport: 1,
                radio: RadioParams {
                    frequency: 2_400_000_000,
                    bandwidth: 500_000,
                    tx_power: 0,
                    tx_power_derived: false,
                    sf: 5,
                    cr: 5,
                    st_alock: None,
                    lt_alock: None,
                },
                outgoing: true,
                incoming_tx: in1_tx,
                counters: Arc::new(InterfaceCounters::new()),
            },
        ];

        let opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let opens_in_task = opens.clone();
        let port_holder = std::sync::Mutex::new(Some(port));
        let connect = move || {
            opens_in_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let taken = port_holder.lock().unwrap().take();
            async move { taken.ok_or(RNodeError::NotDetected) }
        };

        let (merged_tx, merged_rx) = mpsc::channel::<TaggedOutgoing>(16);
        let hub = tokio::spawn(async move {
            rnode_multi_reconnect_task(
                "multi".to_string(),
                connect,
                vports,
                merged_rx,
                false,
                None,
            )
            .await;
        });

        // A full round trip on vport 0 proves the io loop is running before
        // its receiver is dropped — the deregistration must come from the send
        // failure, not from the state the loop was seeded with.
        let ping = |subint: usize, data: &'static [u8]| {
            let merged_tx = merged_tx.clone();
            async move {
                merged_tx
                    .send(TaggedOutgoing {
                        subint,
                        packet: OutgoingPacket {
                            peer: None,
                            data: data.to_vec(),
                            high_priority: false,
                        },
                    })
                    .await
                    .expect("send to hub");
            }
        };
        ping(0, b"warmup").await;
        let echoed = tokio::time::timeout(Duration::from_secs(5), in0_rx.recv())
            .await
            .expect("vport-0 echo within 5s")
            .expect("in0 open");
        assert_eq!(echoed.data, b"warmup");

        // vport 0's logical interface is torn down.
        drop(in0_rx);

        // A frame for the now-dead vport: the send fails and the vport is
        // deregistered. Before the fix, this returned from the io task.
        ping(0, b"to-the-dead").await;
        let (vp, _) = tokio::time::timeout(Duration::from_secs(5), data_rx.recv())
            .await
            .expect("firmware sees the vport-0 frame within 5s")
            .expect("data channel open");
        assert_eq!(vp, 0);

        // The surviving vport still has the radio. This is the assertion the
        // bug fails: after a bounce the connector has no port left to hand out.
        ping(1, b"still-here").await;
        let alive = tokio::time::timeout(Duration::from_secs(5), in1_rx.recv())
            .await
            .expect("vport-1 must keep the radio after vport-0 died")
            .expect("in1 open");
        assert_eq!(alive.data, b"still-here");

        // A second frame for the dead vport must be dropped where it arrives,
        // not rediscovered as a fresh teardown.
        ping(0, b"still-dead").await;
        ping(1, b"and-still-here").await;
        let alive = tokio::time::timeout(Duration::from_secs(5), in1_rx.recv())
            .await
            .expect("vport-1 must survive a second frame for the dead vport")
            .expect("in1 open");
        assert_eq!(alive.data, b"and-still-here");

        assert_eq!(
            opens.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the shared radio must be opened once; a reconnect is the #283 bounce"
        );

        // When the LAST vport goes too, the hub stops on its own rather than
        // reconnecting forever: the all-closed exit stays reachable.
        drop(in1_rx);
        ping(1, b"final").await;
        tokio::time::timeout(Duration::from_secs(5), hub)
            .await
            .expect("hub must stop once every vport is gone")
            .expect("hub task must not panic");

        stub.abort();
    }

    /// The multi-vport twin of behaviour 3 (Codeberg #319): the shared
    /// hub's host-side queue overflow must be exactly as loud as the
    /// single-radio one — every shed frame counted, and counted on the
    /// vport that OWNED the frame (the queue is vport-tagged), while the
    /// `RNODE_TX_QUEUE_DROP` event names the physical interface, because
    /// the port that dropped is a property of the shared line. The gate
    /// closes after the first TX and stays closed because the stub never
    /// answers a CMD_READY query; the queue is then filled past the cap
    /// with vport-1 frames and overflowed by vport-0 pushes, so the shed
    /// oldest frames all belong to vport 1 — pinning the attribution.
    #[tokio::test(start_paused = true)]
    async fn test_multi_vport_host_queue_overflow_is_loud() {
        let (logs, guard) = capture_logs();
        let (port, peer) = tokio::io::duplex(64 * 1024);
        let (freq_tx, mut freq_rx) = tokio::sync::mpsc::channel::<(u8, u32)>(8);
        let (data_tx, mut data_rx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(256);
        let stub = tokio::spawn(rnode_multi_firmware_stub(
            peer,
            vec![rnode::CHIP_SX127X, rnode::CHIP_SX128X],
            freq_tx,
            data_tx,
        ));

        // Keep both incoming receivers alive: the stub echoes CMD_DATA, and
        // a dropped receiver would end the io task via `incoming_closed`.
        let (in0_tx, _in0_rx) = mpsc::channel::<IncomingPacket>(256);
        let (in1_tx, _in1_rx) = mpsc::channel::<IncomingPacket>(256);
        let counters0 = Arc::new(InterfaceCounters::new());
        let counters1 = Arc::new(InterfaceCounters::new());
        let vports = vec![
            VportRuntime {
                id: InterfaceId(30),
                name: "multi[low]".to_string(),
                vport: 0,
                radio: RadioParams {
                    frequency: 865_600_000,
                    bandwidth: 125_000,
                    tx_power: 0,
                    tx_power_derived: false,
                    sf: 7,
                    cr: 5,
                    st_alock: None,
                    lt_alock: None,
                },
                outgoing: true,
                incoming_tx: in0_tx,
                counters: Arc::clone(&counters0),
            },
            VportRuntime {
                id: InterfaceId(31),
                name: "multi[high]".to_string(),
                vport: 1,
                radio: RadioParams {
                    frequency: 2_400_000_000,
                    bandwidth: 500_000,
                    tx_power: 0,
                    tx_power_derived: false,
                    sf: 5,
                    cr: 5,
                    st_alock: None,
                    lt_alock: None,
                },
                outgoing: true,
                incoming_tx: in1_tx,
                counters: Arc::clone(&counters1),
            },
        ];

        let port_holder = std::sync::Mutex::new(Some(port));
        let connect = move || {
            let taken = port_holder.lock().unwrap().take();
            async move { taken.ok_or(RNodeError::NotDetected) }
        };
        let (merged_tx, merged_rx) = mpsc::channel::<TaggedOutgoing>(16);
        let hub = tokio::spawn(async move {
            rnode_multi_reconnect_task(
                "multi".to_string(),
                connect,
                vports,
                merged_rx,
                /* flow_control = */ true,
                None,
            )
            .await;
        });

        // Wait until both vports are configured so the io loop is running.
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(5), freq_rx.recv())
                .await
                .expect("frequency config must be pushed within 5s")
                .expect("freq channel open");
        }

        // The bootstrap frame ships (the gate starts open); its post-TX
        // CMD_READY query goes unanswered, so the gate stays closed for good.
        merged_tx
            .send(TaggedOutgoing {
                subint: 0,
                packet: OutgoingPacket {
                    peer: None,
                    data: b"bootstrap".to_vec(),
                    high_priority: false,
                },
            })
            .await
            .expect("send to hub");
        let (_, boot) = tokio::time::timeout(Duration::from_secs(5), data_rx.recv())
            .await
            .expect("bootstrap frame ships ungated")
            .expect("data channel open");
        assert_eq!(boot, b"bootstrap");

        // Fill the held queue to the cap with vport-1 frames, then overflow
        // it with vport-0 pushes: each sheds the oldest held frame, all of
        // which are vport 1's.
        const EXCESS: usize = 5;
        for i in 0..FLOW_CONTROL_QUEUE_LIMIT {
            merged_tx
                .send(TaggedOutgoing {
                    subint: 1,
                    packet: OutgoingPacket {
                        peer: None,
                        data: format!("v1-{i:03}").into_bytes(),
                        high_priority: false,
                    },
                })
                .await
                .expect("send to hub");
        }
        for i in 0..EXCESS {
            merged_tx
                .send(TaggedOutgoing {
                    subint: 0,
                    packet: OutgoingPacket {
                        peer: None,
                        data: format!("v0-{i:03}").into_bytes(),
                        high_priority: false,
                    },
                })
                .await
                .expect("send to hub");
        }
        // Let the hub drain the merged channel into its send queue.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(
            counters1
                .tx_queue_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            EXCESS as u64,
            "every shed frame must be counted on the vport that owned it"
        );
        assert_eq!(
            counters0
                .tx_queue_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the vport whose push caused the shed lost nothing of its own"
        );

        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert_eq!(
            count_event(&logs, "RNODE_TX_QUEUE_DROP"),
            EXCESS,
            "each shed frame must be named by a structured event\n--- logs ---\n{logs}"
        );
        let line = logs
            .lines()
            .find(|l| l.contains("event=\"RNODE_TX_QUEUE_DROP\""))
            .expect("checked non-zero above");
        for key in ["iface=multi", "len=", "depth=", "reason=\"queue_full\""] {
            assert!(
                line.contains(key),
                "drop event must carry {key}; line: {line}"
            );
        }

        hub.abort();
        stub.abort();
    }

    /// A modem that answers nothing keeps the gate closed: with
    /// `flow_control = true` against a peer that never responds to the
    /// CMD_READY queries (dead serial, wedged firmware), the io task ships
    /// exactly one frame and then holds the rest host-side. That is the
    /// safe contract — a silent modem must not be flooded on hope; the
    /// re-query poll keeps asking at a bounded cadence and the reconnect
    /// path owns recovery from a truly dead device.
    #[tokio::test]
    async fn test_flow_control_without_cmd_ready_stalls_after_first_frame() {
        let (port, mut peer) = tokio::io::duplex(8192);
        let (incoming_tx, _incoming_rx) = mpsc::channel::<IncomingPacket>(16);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingPacket>(16);
        let counters = Arc::new(InterfaceCounters::new());

        let task_counters = Arc::clone(&counters);
        let task = tokio::spawn(async move {
            rnode_io_task(
                "test_rnode".to_string(),
                port,
                incoming_tx,
                outgoing_rx,
                task_counters,
                /* flow_control = */ true,
                /* jitter_max_ms = */ 1,
                125_000,
                7,
                5,
                /* drop_direct_ingress = */ false,
            )
            .await;
        });

        let payloads: [&[u8]; 3] = [b"alpha", b"bravo", b"charlie"];
        for p in payloads.iter() {
            outgoing_tx
                .send(OutgoingPacket {
                    peer: None,
                    data: p.to_vec(),
                    high_priority: false,
                })
                .await
                .expect("send to io task");
        }

        // Generous read window. The first frame must arrive; any later
        // frame would mean the gate opened without a queue-not-full answer.
        // The CMD_READY queries the io task sends land in `frames` too and
        // are filtered out below — only CMD_DATA counts.
        let frames = drain_kiss_frames(&mut peer, Duration::from_millis(500)).await;
        let data_frames: Vec<&Vec<u8>> = frames
            .iter()
            .filter(|(c, _)| *c == rnode::CMD_DATA)
            .map(|(_, p)| p)
            .collect();

        assert_eq!(
            data_frames.len(),
            1,
            "exactly one frame must reach the wire; the others must remain queued \
             waiting for CMD_READY (got {} CMD_DATA frames)",
            data_frames.len()
        );
        assert_eq!(
            data_frames[0].as_slice(),
            payloads[0],
            "the single TX must be the first queued payload"
        );

        let tx_bytes = counters.tx_bytes.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            tx_bytes,
            payloads[0].len() as u64,
            "tx_bytes must reflect exactly the first payload"
        );

        drop(outgoing_tx);
        let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
    }

    // -----------------------------------------------------------------------
    // CMD_READY flow control under a firmware duty lock
    // -----------------------------------------------------------------------

    /// Scripted firmware stub at the io-task boundary for the duty-lock
    /// flow-control tests.
    ///
    /// Reads KISS frames from its half of the duplex and records every
    /// `CMD_DATA` payload on `data_tx`. TX model: a one-deep firmware queue
    /// behind a scripted airtime budget — a frame received while
    /// `air_budget > 0` airs at once (budget decrements), a frame received
    /// at budget zero stays in the queue, which is the duty-lock shape
    /// (serial still drains into the queue, but no TX completes,
    /// `RNode_Firmware.ino:1624`). A budget increment on `resume_rx` models
    /// the lock releasing: queued frames air against the refreshed budget —
    /// silently.
    ///
    /// The stub speaks `CMD_READY` **only as the response to a host
    /// CMD_READY query**, exactly like the firmware
    /// (`RNode_Firmware.ino:1003-1008`): 0x01 while its queue has room,
    /// 0x00 while it is full. `kiss_indicate_ready`/`_not_ready`
    /// (`Utilities.h:1157,1164`) have no other call sites, so a spontaneous
    /// READY — after a TX, on queue drain, ever — would be dishonest. The
    /// previous stub's READY-follows-each-TX script was exactly the wrong
    /// protocol model that let six green tests hide the flowval leg-B
    /// deadlock (2026-08-21): an implementation that waits for an
    /// unsolicited READY starves against this stub, which is the point.
    ///
    /// `end_rx` scripts the disconnect: the stub either drops its half of
    /// the duplex (the io task reads `Ok(0)`, a port that went away) or
    /// announces a firmware reset — the two return paths the held-frame
    /// tests drive (Codeberg #316).
    async fn rnode_firmware_stub_scripted(
        mut peer: tokio::io::DuplexStream,
        mut air_budget: usize,
        mut resume_rx: mpsc::Receiver<usize>,
        mut end_rx: mpsc::Receiver<StubEnd>,
        data_tx: mpsc::Sender<Vec<u8>>,
        ready_queries: Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
        let mut buf = [0u8; 1024];
        // Frames sitting in the firmware queue while the lock holds TX.
        // One-deep model: any held frame means the queue is full.
        let mut queued: usize = 0;
        loop {
            tokio::select! {
                read = peer.read(&mut buf) => {
                    let n = match read {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    for f in deframer.process(&buf[..n]) {
                        if let KissDeframeResult::Frame { command, payload } = f {
                            match command {
                                rnode::CMD_DATA => {
                                    if data_tx.send(payload.to_vec()).await.is_err() {
                                        return;
                                    }
                                    if air_budget > 0 {
                                        air_budget -= 1;
                                    } else {
                                        queued += 1;
                                    }
                                    // No READY here: the firmware does not
                                    // announce TX completion.
                                }
                                rnode::CMD_READY => {
                                    ready_queries.fetch_add(
                                        1,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    let mut resp = Vec::new();
                                    kiss::frame(
                                        rnode::CMD_READY,
                                        &[if queued > 0 { 0x00 } else { 0x01 }],
                                        &mut resp,
                                    );
                                    if peer.write_all(&resp).await.is_err() {
                                        return;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                resumed = resume_rx.recv() => {
                    match resumed {
                        Some(n) => {
                            // Lock released: the queue drains against the
                            // refreshed budget. Nothing is announced — only
                            // the next query learns about the room.
                            air_budget += n;
                            while queued > 0 && air_budget > 0 {
                                queued -= 1;
                                air_budget -= 1;
                            }
                        }
                        None => return,
                    }
                }
                end = end_rx.recv() => {
                    match end {
                        // Returning drops `peer`, so the io task's next read
                        // yields `Ok(0)` — the port went away mid-gate.
                        Some(StubEnd::Eof) | None => return,
                        Some(StubEnd::DeviceReset) => {
                            let mut resp = Vec::new();
                            kiss::frame(rnode::CMD_RESET, &[DEVICE_RESET_MARKER], &mut resp);
                            if peer.write_all(&resp).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    }

    /// Everything a duty-lock test needs: `rnode_io_task` under test, wired
    /// to [`rnode_firmware_stub_scripted`] over an in-memory duplex. All
    /// timing is driven by `start_paused` tokio time, so the 2 s gated
    /// threshold and multi-second hold windows cost no wall clock.
    /// How a test ends the scripted stub's session.
    #[derive(Clone, Copy, Debug)]
    enum StubEnd {
        /// The port goes away: the stub drops the duplex, the io task reads
        /// `Ok(0)`.
        Eof,
        /// The firmware announces a reset (`CMD_RESET` 0xF8) on an otherwise
        /// healthy port.
        DeviceReset,
    }

    struct DutyLockHarness {
        outgoing_tx: mpsc::Sender<OutgoingPacket>,
        counters: Arc<InterfaceCounters>,
        resume_tx: mpsc::Sender<usize>,
        /// Scripts the disconnect (see [`StubEnd`]).
        end_tx: mpsc::Sender<StubEnd>,
        data_rx: mpsc::Receiver<Vec<u8>>,
        /// CMD_READY queries the stub has answered — proves the host asks.
        ready_queries: Arc<std::sync::atomic::AtomicUsize>,
        /// Held so the io task's `incoming_tx.send` never fails mid-test.
        _incoming_rx: mpsc::Receiver<IncomingPacket>,
    }

    fn spawn_duty_lock_harness(flow_control: bool, air_budget: usize) -> DutyLockHarness {
        let (port, peer) = tokio::io::duplex(64 * 1024);
        let (incoming_tx, incoming_rx) = mpsc::channel::<IncomingPacket>(16);
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<OutgoingPacket>(16);
        let (resume_tx, resume_rx) = mpsc::channel::<usize>(16);
        let (end_tx, end_rx) = mpsc::channel::<StubEnd>(4);
        let (data_tx, data_rx) = mpsc::channel::<Vec<u8>>(256);
        let counters = Arc::new(InterfaceCounters::new());
        let ready_queries = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        tokio::spawn(rnode_firmware_stub_scripted(
            peer,
            air_budget,
            resume_rx,
            end_rx,
            data_tx,
            Arc::clone(&ready_queries),
        ));
        let task_counters = Arc::clone(&counters);
        tokio::spawn(async move {
            rnode_io_task(
                "test_rnode_duty".to_string(),
                port,
                incoming_tx,
                outgoing_rx,
                task_counters,
                flow_control,
                /* jitter_max_ms = */ 1,
                125_000,
                7,
                5,
                /* drop_direct_ingress = */ false,
            )
            .await;
        });

        DutyLockHarness {
            outgoing_tx,
            counters,
            resume_tx,
            end_tx,
            data_rx,
            ready_queries,
            _incoming_rx: incoming_rx,
        }
    }

    impl DutyLockHarness {
        async fn push(&self, payload: &[u8]) {
            self.outgoing_tx
                .send(OutgoingPacket {
                    peer: None,
                    data: payload.to_vec(),
                    high_priority: false,
                })
                .await
                .expect("io task alive");
        }

        async fn expect_frame(&mut self, want: &[u8], ctx: &str) {
            let got = tokio::time::timeout(Duration::from_secs(2), self.data_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{ctx}: expected frame {want:?}, stub saw nothing"))
                .expect("stub data channel open");
            assert_eq!(got, want, "{ctx}");
        }

        /// Push `count` frames while the gate is closed, then let the io task
        /// pull them out of the channel into its task-local send queue —
        /// which is exactly the queue a disconnect abandons.
        async fn hold_frames(&self, count: usize) {
            for i in 0..count {
                self.push(format!("h{i:03}").as_bytes()).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        /// Script the stub's disconnect and give the io task time to see it.
        async fn end_session(&self, end: StubEnd) {
            self.end_tx.send(end).await.expect("stub alive");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        /// Assert the stub sees no `CMD_DATA` for `window`. The serial-only
        /// `CMD_DETECT` heartbeat is not TX data and is exempt by design.
        async fn expect_silence(&mut self, window: Duration, ctx: &str) {
            if let Ok(Some(got)) = tokio::time::timeout(window, self.data_rx.recv()).await {
                panic!("{ctx}: stub must see no CMD_DATA, saw {got:?}");
            }
        }
    }

    fn count_event(logs: &str, event: &str) -> usize {
        let needle = format!("event=\"{event}\"");
        logs.lines().filter(|l| l.contains(&needle)).count()
    }

    /// Behaviour 1, lock semantics: the stub airs the first three frames
    /// (each post-TX query answers 0x01), then the duty lock holds TX
    /// (no completion, `RNode_Firmware.ino:1624`). The fourth frame still
    /// ships — it is what fills the firmware queue — and its query answers
    /// 0x00, so with `flow_control = true` the host must hold frames five
    /// and six on its side; they must never appear at the stub.
    #[tokio::test(start_paused = true)]
    async fn test_flow_control_duty_lock_holds_frames_host_side() {
        let mut h = spawn_duty_lock_harness(true, 3);
        let payloads: [&[u8]; 6] = [b"f1", b"f2", b"f3", b"f4", b"f5", b"f6"];
        for p in payloads {
            h.push(p).await;
        }
        for p in &payloads[..4] {
            h.expect_frame(p, "frames up to the one that fills the firmware queue flow")
                .await;
        }
        h.expect_silence(
            Duration::from_secs(5),
            "once the queue is full every poll answers 0x00, \
             so the remaining frames are held host-side",
        )
        .await;
    }

    /// Behaviour 2, bootstrap: the firmware never speaks CMD_READY
    /// unsolicited, so from a cold start with `flow_control = true` the
    /// first frame must go out without asking anything (the gate starts
    /// open; there is no queue state worth asking about before the first
    /// TX). Here the duty lock already holds TX, so that frame jams the
    /// one-deep firmware queue, the post-TX query answers 0x00, and the
    /// second frame waits host-side until a poll learns the queue drained.
    #[tokio::test(start_paused = true)]
    async fn test_flow_control_bootstrap_first_frame_needs_no_ready() {
        let mut h = spawn_duty_lock_harness(true, 0);
        h.push(b"first").await;
        h.push(b"second").await;
        h.expect_frame(
            b"first",
            "cold start: the first frame must ship without any READY exchange",
        )
        .await;
        h.expect_silence(
            Duration::from_secs(3),
            "the second frame must wait while every poll answers 0x00",
        )
        .await;
        h.resume_tx.send(1).await.expect("stub alive");
        h.expect_frame(b"second", "the next poll after the drain re-opens the gate")
            .await;
    }

    /// Behaviour 3, host-queue overflow is loud: with the gate
    /// closed, frames past `FLOW_CONTROL_QUEUE_LIMIT` are dropped oldest-
    /// first. Required: every drop is counted on the interface counters and
    /// named by a structured `RNODE_TX_QUEUE_DROP` event — a silent drop is
    /// exactly the black-hole failure this batch exists to prevent.
    #[tokio::test(start_paused = true)]
    async fn test_flow_control_host_queue_overflow_is_loud() {
        let (logs, guard) = capture_logs();
        let mut h = spawn_duty_lock_harness(true, 0);
        h.push(b"bootstrap").await;
        h.expect_frame(b"bootstrap", "bootstrap frame ships ungated")
            .await;

        // The gate is now closed for good (every poll answers 0x00).
        // Overfill the host-side queue past its cap.
        const EXCESS: usize = 5;
        for i in 0..(FLOW_CONTROL_QUEUE_LIMIT + EXCESS) {
            h.push(format!("q{i:03}").as_bytes()).await;
        }
        // Let the io task drain the channel into its send queue.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(
            h.counters
                .tx_queue_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            EXCESS as u64,
            "every dropped frame must be counted on the interface counters"
        );
        h.expect_silence(
            Duration::from_millis(500),
            "nothing may leak to the stub while gated",
        )
        .await;

        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert_eq!(
            count_event(&logs, "RNODE_TX_QUEUE_DROP"),
            EXCESS,
            "each drop must be named by a structured event\n--- logs ---\n{logs}"
        );
        let line = logs
            .lines()
            .find(|l| l.contains("event=\"RNODE_TX_QUEUE_DROP\""))
            .expect("checked non-zero above");
        for key in ["iface=test_rnode_duty", "len=", "depth="] {
            assert!(
                line.contains(key),
                "drop event must carry {key}; line: {line}"
            );
        }
    }

    /// Behaviour 4, gated-too-long visibility: while the gate
    /// holds queued frames beyond `TX_GATED_EVENT_AFTER` (one CHTM cadence,
    /// 2 s — a READY normally follows a TX within one packet airtime, and a
    /// gate still closed after a full firmware stat cadence is the duty-lock
    /// shape, not airtime wait), the io task emits `RNODE_TX_GATED` with the
    /// held duration and queue depth, repeated every `TX_GATED_EVENT_REPEAT`
    /// (10 s) — bounded, not per loop iteration. Held 25 s ⇒ exactly three
    /// events (t = 2, 12, 22 s), deterministic under paused time.
    #[tokio::test(start_paused = true)]
    async fn test_flow_control_gated_too_long_emits_bounded_events() {
        let (logs, guard) = capture_logs();
        let mut h = spawn_duty_lock_harness(true, 0);
        h.push(b"bootstrap").await;
        h.expect_frame(b"bootstrap", "bootstrap frame ships ungated")
            .await;
        h.push(b"held").await;

        tokio::time::sleep(Duration::from_secs(25)).await;

        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert_eq!(
            count_event(&logs, "RNODE_TX_GATED"),
            3,
            "held 25 s ⇒ events at 2/12/22 s, repeated at a bounded rate\n--- logs ---\n{logs}"
        );
        let line = logs
            .lines()
            .find(|l| l.contains("event=\"RNODE_TX_GATED\""))
            .expect("checked non-zero above");
        for key in ["iface=test_rnode_duty", "held_ms=", "depth=1"] {
            assert!(
                line.contains(key),
                "gated event must carry {key}; line: {line}"
            );
        }
    }

    /// Behaviour 5, release: the duty lock lifts, the queue drains, the
    /// next poll answers 0x01, the held frames drain in order, and one
    /// `RNODE_TX_RELEASED` closes the pair opened by `RNODE_TX_GATED`. The
    /// post-release drain re-closes the gate only for sub-threshold
    /// moments, so no further gated/release events may fire.
    #[tokio::test(start_paused = true)]
    async fn test_flow_control_release_drains_in_order_and_pairs_events() {
        let (logs, guard) = capture_logs();
        let mut h = spawn_duty_lock_harness(true, 2);
        let payloads: [&[u8]; 5] = [b"r1", b"r2", b"r3", b"r4", b"r5"];
        for p in payloads {
            h.push(p).await;
        }
        // Budget 2: r1/r2 air, r3 ships into the locked firmware and jams
        // its queue; r4/r5 are held host-side by the 0x00 poll answers.
        for p in &payloads[..3] {
            h.expect_frame(p, "pre-lock frames flow").await;
        }
        // 5 s > the 2 s threshold: the gated event must have fired before
        // the release below closes the pair.
        h.expect_silence(Duration::from_secs(5), "r4/r5 held under the lock")
            .await;

        h.resume_tx.send(1000).await.expect("stub alive");
        h.expect_frame(b"r4", "held frames drain in order after release")
            .await;
        h.expect_frame(b"r5", "held frames drain in order after release")
            .await;

        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert_eq!(
            count_event(&logs, "RNODE_TX_GATED"),
            1,
            "one gated event during the 5 s hold\n--- logs ---\n{logs}"
        );
        assert_eq!(
            count_event(&logs, "RNODE_TX_RELEASED"),
            1,
            "exactly one release event closes the pair\n--- logs ---\n{logs}"
        );
        let line = logs
            .lines()
            .find(|l| l.contains("event=\"RNODE_TX_RELEASED\""))
            .expect("checked non-zero above");
        for key in ["iface=test_rnode_duty", "held_ms=", "depth=2"] {
            assert!(
                line.contains(key),
                "release event must carry {key}; line: {line}"
            );
        }
    }

    /// Behaviour 6, off means off: with `flow_control = false`
    /// (the default) every frame ships without any READY exchange, in
    /// order, and none of the duty-lock machinery speaks — no gated,
    /// release, or queue-drop events, and no CMD_READY queries on the
    /// serial line either. The default does not change in this batch.
    #[tokio::test(start_paused = true)]
    async fn test_flow_control_off_never_gates_and_stays_silent() {
        let (logs, guard) = capture_logs();
        let mut h = spawn_duty_lock_harness(false, 0);
        let payloads: [&[u8]; 4] = [b"n1", b"n2", b"n3", b"n4"];
        for p in payloads {
            h.push(p).await;
        }
        for p in &payloads {
            h.expect_frame(p, "flow_control=false ships every frame without READY")
                .await;
        }
        // Give a wrongly-armed gate timer ample time to fire before reading
        // the captured logs.
        tokio::time::sleep(Duration::from_secs(30)).await;

        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        for event in ["RNODE_TX_GATED", "RNODE_TX_RELEASED", "RNODE_TX_QUEUE_DROP"] {
            assert_eq!(
                count_event(&logs, event),
                0,
                "{event} must be absent with flow_control off\n--- logs ---\n{logs}"
            );
        }
        assert_eq!(
            h.ready_queries.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "flow_control off must put no CMD_READY queries on the serial line"
        );
    }

    /// Find the single `RNODE_TX_QUEUE_DROP` line in `logs` and assert it
    /// carries exactly the abandon shape for `reason`, with `frames` frames.
    fn assert_single_abandon_event(logs: &str, reason: &str, frames: usize) {
        assert_eq!(
            count_event(logs, "RNODE_TX_QUEUE_DROP"),
            1,
            "one summary event per return path, never one per frame\
             \n--- logs ---\n{logs}"
        );
        let line = logs
            .lines()
            .find(|l| l.contains("event=\"RNODE_TX_QUEUE_DROP\""))
            .expect("checked non-zero above");
        for key in [
            "iface=test_rnode_duty".to_string(),
            format!("frames={frames}"),
            format!("reason=\"{reason}\""),
        ] {
            assert!(
                line.contains(&key),
                "abandon event must carry {key}; line: {line}"
            );
        }
    }

    /// Behaviour 7, a port drop must not swallow held frames
    /// (Codeberg #316): the gate is closed, frames are held host-side, and
    /// then the port goes away. `rnode_io_task`'s send queue is task-local,
    /// so those frames are gone — the reconnected task only inherits what is
    /// still in the mpsc channel. That loss is legitimate; keeping it silent
    /// is not. Every abandoned frame must land on `tx_queue_drops` and the
    /// return path must name itself once.
    #[tokio::test(start_paused = true)]
    async fn test_held_frames_are_counted_when_the_port_drops() {
        let (logs, guard) = capture_logs();
        let mut h = spawn_duty_lock_harness(true, 0);
        h.push(b"bootstrap").await;
        h.expect_frame(b"bootstrap", "bootstrap frame ships ungated")
            .await;

        // The bootstrap frame jammed the stub's one-deep queue, so every
        // poll now answers 0x00 and these stay held on our side.
        const HELD: usize = 3;
        h.hold_frames(HELD).await;
        assert_eq!(
            h.counters
                .tx_queue_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "nothing is dropped while the frames are merely held"
        );

        h.end_session(StubEnd::Eof).await;

        assert_eq!(
            h.counters
                .tx_queue_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            HELD as u64,
            "every frame the dying io task abandons must be counted"
        );
        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert_single_abandon_event(&logs, "serial_eof", HELD);
    }

    /// Behaviour 7b, the same shape through a second return path: the
    /// firmware announces a reset (`CMD_RESET` 0xF8) on a port that is
    /// otherwise fine. The io task returns just as abruptly, so the held
    /// frames are just as gone — and must be just as loud, with the reason
    /// naming this path rather than the EOF one.
    #[tokio::test(start_paused = true)]
    async fn test_held_frames_are_counted_on_device_reset() {
        let (logs, guard) = capture_logs();
        let mut h = spawn_duty_lock_harness(true, 0);
        h.push(b"bootstrap").await;
        h.expect_frame(b"bootstrap", "bootstrap frame ships ungated")
            .await;

        const HELD: usize = 4;
        h.hold_frames(HELD).await;
        h.end_session(StubEnd::DeviceReset).await;

        assert_eq!(
            h.counters
                .tx_queue_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            HELD as u64,
            "a device reset abandons the queue exactly like an EOF does"
        );
        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert_single_abandon_event(&logs, "device_reset", HELD);
    }

    /// Behaviour 7c, silence stays honest: a disconnect with an empty send
    /// queue abandons nothing, so it must say nothing. An unconditional
    /// event at every return path would make `RNODE_TX_QUEUE_DROP` fire on
    /// every ordinary reconnect and train the operator to ignore it.
    #[tokio::test(start_paused = true)]
    async fn test_empty_queue_at_disconnect_stays_silent() {
        let (logs, guard) = capture_logs();
        let mut h = spawn_duty_lock_harness(true, 1);
        h.push(b"only").await;
        h.expect_frame(b"only", "the single frame airs, leaving the queue empty")
            .await;

        h.end_session(StubEnd::Eof).await;

        assert_eq!(
            h.counters
                .tx_queue_drops
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "an empty queue at disconnect drops nothing"
        );
        drop(guard);
        let logs = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert_eq!(
            count_event(&logs, "RNODE_TX_QUEUE_DROP"),
            0,
            "no frames abandoned means no event\n--- logs ---\n{logs}"
        );
    }

    /// The regression net for the flowval leg-B deadlock (2026-08-21):
    /// the firmware answers CMD_READY only when queried
    /// (`RNode_Firmware.ino:1003-1008`) — never spontaneously, not after a
    /// TX, not on queue drain (`kiss_indicate_ready`/`_not_ready`,
    /// `Utilities.h:1157,1164`, have no other call sites). An
    /// implementation that waits for an unsolicited READY starves here:
    /// the stub's queue drains after `resume`, but only a query can learn
    /// that, so `expect_frame(second)` times out against a
    /// wait-for-spontaneous-READY host. The query counter additionally
    /// pins that the reopen was learned by asking.
    #[tokio::test(start_paused = true)]
    async fn test_flow_control_reopen_is_learned_by_query_never_spontaneous() {
        let mut h = spawn_duty_lock_harness(true, 0);
        h.push(b"first").await;
        h.expect_frame(b"first", "cold-start frame ships ungated")
            .await;
        // The frame sits in the firmware queue under the lock; the host
        // gate is closed. Lift the lock: the queue drains, and the
        // firmware says nothing on its own.
        h.resume_tx.send(1).await.expect("stub alive");
        h.push(b"second").await;
        h.expect_frame(
            b"second",
            "the gate must reopen via a CMD_READY query — the firmware never volunteers READY",
        )
        .await;
        assert!(
            h.ready_queries.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "the reopen must have been learned through a CMD_READY query"
        );
    }

    // -----------------------------------------------------------------------
    // Radio stats parse/state path (Codeberg #25)
    // -----------------------------------------------------------------------

    /// A fresh RNode interface is marked radio-capable so `radio_stats()`
    /// returns the default record (0.0 airtime/channel-load, None noise/temp,
    /// Unknown battery) even before any `CMD_STAT_*` frame arrives — mirroring
    /// Python's `hasattr(interface, "r_airtime_short")` always being true for an
    /// RNodeInterface. A non-radio interface stays `None`.
    #[test]
    fn radio_stats_default_present_after_enable() {
        let c = InterfaceCounters::new();
        assert!(
            c.radio_stats().is_none(),
            "non-radio interface has no stats"
        );

        c.enable_radio_stats();
        let r = c.radio_stats().expect("radio stats present after enable");
        assert_eq!(r.airtime_short, 0.0);
        assert_eq!(r.airtime_long, 0.0);
        assert_eq!(r.channel_load_short, 0.0);
        assert_eq!(r.channel_load_long, 0.0);
        assert_eq!(r.noise_floor, None);
        assert_eq!(r.cpu_temp, None);
        assert_eq!(r.battery_state, rnode::BatteryState::Unknown);
        assert_eq!(r.last_rssi, None);
        assert_eq!(r.last_snr, None);
    }

    /// CMD_STAT_RSSI payload is a single raw byte; stored value is dBm
    /// (`raw - 157`), matching Python `r_stat_rssi = byte - RSSI_OFFSET`.
    #[test]
    fn apply_radio_stat_rssi_dbm() {
        let c = InterfaceCounters::new();
        assert!(apply_radio_stat(&c, rnode::CMD_STAT_RSSI, &[100]));
        assert_eq!(c.radio_stats().unwrap().last_rssi, Some(-57));
    }

    /// CMD_STAT_SNR payload is one signed byte scaled by 0.25 dB, matching
    /// Python `r_stat_snr = signed_byte * 0.25`.
    #[test]
    fn apply_radio_stat_snr_scaled() {
        let c = InterfaceCounters::new();
        // 0x28 = 40 -> 10.0 dB
        assert!(apply_radio_stat(&c, rnode::CMD_STAT_SNR, &[0x28]));
        assert_eq!(c.radio_stats().unwrap().last_snr, Some(10.0));
        // 0xF0 = -16 (signed) -> -4.0 dB
        assert!(apply_radio_stat(&c, rnode::CMD_STAT_SNR, &[0xF0]));
        assert_eq!(c.radio_stats().unwrap().last_snr, Some(-4.0));
    }

    /// CMD_STAT_CHTM (single-interface, 11 bytes): four big-endian u16 airtime/
    /// channel-load fields scaled to percent (`raw / 100.0`), plus RSSI, noise
    /// floor (`raw - 157` dBm), and interference. Matches Python's
    /// `ats/100.0 ... nfl - RSSI_OFFSET`.
    #[test]
    fn apply_radio_stat_chtm_scale_and_noise_floor() {
        let c = InterfaceCounters::new();
        let payload = [
            0x01, 0x2C, // airtime_short = 300 -> 3.0%
            0x03, 0xE8, // airtime_long = 1000 -> 10.0%
            0x00, 0xC8, // channel_load_short = 200 -> 2.0%
            0x02, 0x58, // channel_load_long = 600 -> 6.0%
            0xC8, // current_rssi raw 200
            100,  // noise_floor raw 100 -> -57 dBm
            0xFF, // interference none
        ];
        assert!(apply_radio_stat(&c, rnode::CMD_STAT_CHTM, &payload));
        let r = c.radio_stats().unwrap();
        assert_eq!(r.airtime_short, 3.0);
        assert_eq!(r.airtime_long, 10.0);
        assert_eq!(r.channel_load_short, 2.0);
        assert_eq!(r.channel_load_long, 6.0);
        assert_eq!(r.noise_floor, Some(-57));
    }

    /// CMD_STAT_BAT payload is `[state, percent]`; percent is 0..=100. Matches
    /// Python `r_battery_state`/`r_battery_percent`.
    #[test]
    fn apply_radio_stat_battery() {
        let c = InterfaceCounters::new();
        // 0x02 = Charging, 85%
        assert!(apply_radio_stat(&c, rnode::CMD_STAT_BAT, &[0x02, 85]));
        let r = c.radio_stats().unwrap();
        assert_eq!(r.battery_state, rnode::BatteryState::Charging);
        assert_eq!(r.battery_percent, 85);
    }

    /// CMD_STAT_TEMP payload is one byte; temperature is `raw - 120` Celsius,
    /// clamped to `[-30, 90]` and `None` outside that range, matching Python's
    /// `if temp >= -30 and temp <= 90 ... else None`.
    #[test]
    fn apply_radio_stat_temperature_clamped() {
        let c = InterfaceCounters::new();
        // 145 - 120 = 25 C (in range)
        assert!(apply_radio_stat(&c, rnode::CMD_STAT_TEMP, &[145]));
        assert_eq!(c.radio_stats().unwrap().cpu_temp, Some(25));
        // 250 - 120 = 130 C (> 90) -> None
        assert!(apply_radio_stat(&c, rnode::CMD_STAT_TEMP, &[250]));
        assert_eq!(c.radio_stats().unwrap().cpu_temp, None);
        // 80 - 120 = -40 C (< -30) -> None
        assert!(apply_radio_stat(&c, rnode::CMD_STAT_TEMP, &[80]));
        assert_eq!(c.radio_stats().unwrap().cpu_temp, None);
    }

    /// A non-stat command is not consumed by the stats parser.
    #[test]
    fn apply_radio_stat_ignores_non_stat_command() {
        let c = InterfaceCounters::new();
        assert!(!apply_radio_stat(&c, rnode::CMD_DATA, &[1, 2, 3]));
        assert!(c.radio_stats().is_none());
    }

    /// Capture tracing output for the duration of the returned guard. Uses a
    /// thread-local default subscriber, which sees the io task's events
    /// because the current-thread tokio test runtime runs every task on the
    /// test thread. Same pattern as the core TUNNEL event test.
    #[derive(Clone)]
    struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
        type Writer = LogSink;
        fn make_writer(&'a self) -> LogSink {
            self.clone()
        }
    }

    fn capture_logs() -> (
        Arc<std::sync::Mutex<Vec<u8>>>,
        tracing::subscriber::DefaultGuard,
    ) {
        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(LogSink(Arc::clone(&buf)))
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (buf, guard)
    }

    // -----------------------------------------------------------------
    // Derived board-maximum TX power vs. a board that clamps
    // -----------------------------------------------------------------

    /// Firmware stub that echoes every config command back verbatim EXCEPT
    /// `CMD_TXPOWER`, which it clamps at `ceiling` before echoing — exactly
    /// what the RNode firmware does (`RNode_Firmware.ino:861-879`: an
    /// SX127x board caps at 17 dBm, an SX1262 without an external PA at 22).
    async fn rnode_stub_clamping_txpower(mut peer: tokio::io::DuplexStream, ceiling: u8) {
        let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
        let mut buf = [0u8; 512];
        let push = |reply: &mut Vec<u8>, cmd: u8, payload: &[u8]| {
            let mut one = Vec::new();
            kiss::frame(cmd, payload, &mut one);
            reply.extend_from_slice(&one);
        };
        loop {
            let n = match peer.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut reply: Vec<u8> = Vec::new();
            for f in deframer.process(&buf[..n]) {
                if let KissDeframeResult::Frame { command, payload } = f {
                    match command {
                        rnode::CMD_DETECT => {
                            push(&mut reply, rnode::CMD_DETECT, &[rnode::DETECT_RESP]);
                            push(
                                &mut reply,
                                rnode::CMD_FW_VERSION,
                                &[rnode::REQUIRED_FW_MAJ, rnode::REQUIRED_FW_MIN],
                            );
                            push(&mut reply, rnode::CMD_PLATFORM, &[rnode::PLATFORM_ESP32]);
                            push(&mut reply, rnode::CMD_MCU, &[0x00]);
                        }
                        rnode::CMD_TXPOWER => {
                            let asked = payload.first().copied().unwrap_or(0);
                            push(&mut reply, command, &[asked.min(ceiling)]);
                        }
                        rnode::CMD_FREQUENCY
                        | rnode::CMD_BANDWIDTH
                        | rnode::CMD_SF
                        | rnode::CMD_CR
                        | rnode::CMD_RADIO_STATE => push(&mut reply, command, &payload),
                        _ => {}
                    }
                }
            }
            if !reply.is_empty() && peer.write_all(&reply).await.is_err() {
                return;
            }
        }
    }

    fn radio_at(tx_power: u8, tx_power_derived: bool) -> RadioParams {
        RadioParams {
            frequency: 869_525_000,
            bandwidth: 125_000,
            tx_power,
            tx_power_derived,
            sf: 7,
            cr: 5,
            st_alock: None,
            lt_alock: None,
        }
    }

    /// The derived board maximum asks for 22 dBm without probing what this
    /// board can do. An SX127x RNode answers 17 — and because confirmation
    /// is otherwise an exact match, without the derived-value tolerance
    /// every such board would refuse to start rather than run at its own
    /// ceiling. Startup must succeed.
    #[tokio::test]
    async fn a_derived_board_maximum_accepts_a_board_that_clamps_lower() {
        let (mut port, peer) = tokio::io::duplex(64 * 1024);
        let stub = tokio::spawn(rnode_stub_clamping_txpower(peer, 17));

        let result = configure_stream(
            &mut port,
            &radio_at(leviculum_core::rnode::DEFAULT_TX_POWER_DBM as u8, true),
            "clamping-board",
        )
        .await;

        assert!(
            result.is_ok(),
            "a board clamping the derived maximum must still start: {:?}",
            result.err()
        );
        stub.abort();
    }

    /// An explicitly configured power keeps the strict check. 17 dBm is a
    /// value the operator chose; a board that silently delivers 14 has to
    /// say so rather than run 3 dB down in silence.
    #[tokio::test]
    async fn an_explicit_txpower_the_board_clamps_is_still_a_mismatch() {
        let (mut port, peer) = tokio::io::duplex(64 * 1024);
        let stub = tokio::spawn(rnode_stub_clamping_txpower(peer, 14));

        let result = configure_stream(&mut port, &radio_at(17, false), "clamping-board").await;

        match result {
            Err(RNodeError::RadioMismatch(m)) => {
                assert!(m.contains("tx_power"), "wrong mismatch reported: {m}");
                assert!(m.contains("17"), "mismatch must name the request: {m}");
                assert!(m.contains("14"), "mismatch must name what came back: {m}");
            }
            other => panic!("expected a tx_power RadioMismatch, got {other:?}"),
        }
        stub.abort();
    }

    /// The tolerance is one-directional. A board reporting MORE than was
    /// asked for is a mismatch even for the derived default: nothing may
    /// transmit above the requested power, that margin belongs to the
    /// operator's regulatory budget.
    #[tokio::test]
    async fn a_confirmation_above_the_request_is_a_mismatch_even_when_derived() {
        let (mut port, peer) = tokio::io::duplex(64 * 1024);
        // Ceiling above the request, so `min` never bites and the stub
        // answers with a power the interface did not ask for.
        let stub = tokio::spawn(async move {
            let mut peer = peer;
            let mut deframer = KissDeframer::with_max_payload(rnode::HW_MTU);
            let mut buf = [0u8; 512];
            let push = |reply: &mut Vec<u8>, cmd: u8, payload: &[u8]| {
                let mut one = Vec::new();
                kiss::frame(cmd, payload, &mut one);
                reply.extend_from_slice(&one);
            };
            loop {
                let n = match peer.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let mut reply: Vec<u8> = Vec::new();
                for f in deframer.process(&buf[..n]) {
                    if let KissDeframeResult::Frame { command, payload } = f {
                        match command {
                            rnode::CMD_DETECT => {
                                push(&mut reply, rnode::CMD_DETECT, &[rnode::DETECT_RESP]);
                                push(
                                    &mut reply,
                                    rnode::CMD_FW_VERSION,
                                    &[rnode::REQUIRED_FW_MAJ, rnode::REQUIRED_FW_MIN],
                                );
                                push(&mut reply, rnode::CMD_PLATFORM, &[rnode::PLATFORM_ESP32]);
                                push(&mut reply, rnode::CMD_MCU, &[0x00]);
                            }
                            // Answers 30 dBm to a 22 dBm request.
                            rnode::CMD_TXPOWER => push(&mut reply, command, &[30]),
                            rnode::CMD_FREQUENCY
                            | rnode::CMD_BANDWIDTH
                            | rnode::CMD_SF
                            | rnode::CMD_CR
                            | rnode::CMD_RADIO_STATE => push(&mut reply, command, &payload),
                            _ => {}
                        }
                    }
                }
                if !reply.is_empty() && peer.write_all(&reply).await.is_err() {
                    return;
                }
            }
        });

        let result = configure_stream(&mut port, &radio_at(22, true), "overshooting-board").await;

        assert!(
            matches!(result, Err(RNodeError::RadioMismatch(_))),
            "a board answering above the request must fail startup, got {result:?}"
        );
        stub.abort();
    }

    /// The config block idles the radio before it configures it.
    ///
    /// Asserted on the literal byte sequence rather than on "a radio-state
    /// frame is present somewhere": the defect this pins down is that the
    /// trailing `RADIO_STATE_ON` was there all along and the leading
    /// `RADIO_STATE_OFF` was not, so any containment check passes on the
    /// broken block too. Position is the whole assertion.
    #[tokio::test]
    async fn test_radio_config_block_idles_the_radio_first() {
        let radio = RadioParams {
            frequency: 867_200_000,
            bandwidth: 125_000,
            tx_power: 17,
            tx_power_derived: false,
            sf: 9,
            cr: 5,
            st_alock: Some(250),
            lt_alock: Some(1000),
        };

        let mut sink: Vec<u8> = Vec::new();
        send_radio_config(&mut sink, &radio)
            .await
            .expect("writing to a Vec cannot fail");

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            0xC0, 0x06, 0x00, 0xC0,                         // radio state OFF
            0xC0, 0x01, 0x33, 0xB0, 0x6C, 0x00, 0xC0,       // frequency 867.2 MHz
            0xC0, 0x02, 0x00, 0x01, 0xE8, 0x48, 0xC0,       // bandwidth 125 kHz
            0xC0, 0x03, 0x11, 0xC0,                         // tx power 17 dBm
            0xC0, 0x04, 0x09, 0xC0,                         // spreading factor 9
            0xC0, 0x05, 0x05, 0xC0,                         // coding rate 4/5
            0xC0, 0x0B, 0x00, 0xFA, 0xC0,                   // short-term airtime lock
            0xC0, 0x0C, 0x03, 0xE8, 0xC0,                   // long-term airtime lock
            0xC0, 0x06, 0x01, 0xC0,                         // radio state ON
        ];

        assert_eq!(
            sink, expected,
            "config block must be: radio off, parameters, radio on"
        );
    }
}
