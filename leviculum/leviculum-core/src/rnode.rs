//! RNode command protocol, encoding, decoding, and constants
//!
//! This module implements the RNode serial command protocol as a pure data
//! transformation layer (`no_std + alloc`). It defines command constants,
//! encoding functions that produce complete KISS frames, and decoding
//! functions that parse raw payloads from deframed KISS data.
//!
//! The RNode protocol is a KISS superset: standard KISS framing
//! (FEND/FESC/TFEND/TFESC) with RNode-specific command bytes for radio
//! configuration, statistics reporting, and device management.
//!
//! # Encoding
//!
//! Each `build_*` function returns a `Vec<u8>` containing one or more
//! complete KISS frames ready for serial transmission.
//!
//! # Decoding
//!
//! Each `decode_*` function takes the raw payload bytes (already extracted
//! from a KISS frame by [`KissDeframer`](crate::framing::kiss::KissDeframer))
//! and returns `Option<T>`, returning `None` if the payload is too short.
//! All values are returned as raw device integers, no float conversion
//! is performed in core.

use alloc::vec::Vec;

use crate::framing::kiss;

// ---------------------------------------------------------------------------
// Configuration commands
// ---------------------------------------------------------------------------

/// Data packet (standard KISS CMD_DATA)
pub const CMD_DATA: u8 = 0x00;
/// Set/report operating frequency (4 bytes BE, Hz)
pub const CMD_FREQUENCY: u8 = 0x01;
/// Set/report channel bandwidth (4 bytes BE, Hz)
pub const CMD_BANDWIDTH: u8 = 0x02;
/// Set/report TX power (1 byte, dBm)
pub const CMD_TXPOWER: u8 = 0x03;
/// Set/report spreading factor (1 byte, 5-12)
pub const CMD_SF: u8 = 0x04;
/// Set/report coding rate (1 byte, 5-8)
pub const CMD_CR: u8 = 0x05;
/// Set/report radio on/off state
pub const CMD_RADIO_STATE: u8 = 0x06;
/// Device presence detection handshake
pub const CMD_DETECT: u8 = 0x08;
/// Host disconnecting (shutdown notification)
pub const CMD_LEAVE: u8 = 0x0A;
/// Short-term airtime limit (2 bytes BE, value/100 = percent)
pub const CMD_ST_ALOCK: u8 = 0x0B;
/// Long-term airtime limit (2 bytes BE, value/100 = percent)
pub const CMD_LT_ALOCK: u8 = 0x0C;
/// Device ready for next TX packet
pub const CMD_READY: u8 = 0x0F;
/// Select subinterface for next command
/// (multi-interface support, see doc/RNODE_PROTOCOL_RESEARCH.md)
pub const CMD_SEL_INT: u8 = 0x1F;

// ---------------------------------------------------------------------------
// Statistics commands (see Codeberg issue #25)
// ---------------------------------------------------------------------------

/// Total RX packet count (4 bytes BE)
pub const CMD_STAT_RX: u8 = 0x21;
/// Total TX packet count (4 bytes BE)
pub const CMD_STAT_TX: u8 = 0x22;
/// Last packet RSSI (1 byte, unsigned + 157 offset)
pub const CMD_STAT_RSSI: u8 = 0x23;
/// Last packet SNR (1 byte, signed * 0.25 dB)
pub const CMD_STAT_SNR: u8 = 0x24;
/// Channel time/utilization stats (11 bytes single, 8 bytes multi)
pub const CMD_STAT_CHTM: u8 = 0x25;
/// Physical layer parameters (12 bytes single, 10 bytes multi)
pub const CMD_STAT_PHYPRM: u8 = 0x26;
/// Battery status (2 bytes: state, percent)
pub const CMD_STAT_BAT: u8 = 0x27;
/// CSMA contention window params (3 bytes), see Codeberg issue #25
pub const CMD_STAT_CSMA: u8 = 0x28;
/// CPU temperature (1 byte, value - 120 = Celsius)
pub const CMD_STAT_TEMP: u8 = 0x29;

// ---------------------------------------------------------------------------
// System commands
// ---------------------------------------------------------------------------

/// Query/report platform
pub const CMD_PLATFORM: u8 = 0x48;
/// Query/report MCU type
pub const CMD_MCU: u8 = 0x49;
/// Query/report firmware version (2 bytes: major, minor)
pub const CMD_FW_VERSION: u8 = 0x50;
/// Hard reset / reset notification
pub const CMD_RESET: u8 = 0x55;
/// Confirmation byte the RNode firmware requires as CMD_RESET payload
/// before it reboots (RNode_Firmware Framing.h `CMD_RESET_BYTE`).
pub const CMD_RESET_BYTE: u8 = 0xF8;

// ---------------------------------------------------------------------------
// Multi-interface data commands
// (multi-interface support, see doc/RNODE_PROTOCOL_RESEARCH.md)
// ---------------------------------------------------------------------------

/// List available radio interfaces
pub const CMD_INTERFACES: u8 = 0x71;

/// Data commands for multi-interface RNodes (indexed by subinterface)
pub const CMD_INT_DATA: [u8; 12] = [
    0x00, // INT0
    0x10, // INT1
    0x20, // INT2
    0x70, // INT3
    0x75, // INT4
    0x90, // INT5 — NOTE: collides with CMD_ERROR (0x90); context disambiguates
    0xA0, // INT6
    0xB0, // INT7
    0xC0, // INT8 (collides with FEND — see research doc)
    0xD0, // INT9
    0xE0, // INT10
    0xF0, // INT11
];

// ---------------------------------------------------------------------------
// Error command and codes
// ---------------------------------------------------------------------------

/// Error report from device (0x90, same value as `CMD_INT_DATA[5]` and PLATFORM_AVR;
/// disambiguated by protocol context: command byte vs payload byte)
pub const CMD_ERROR: u8 = 0x90;

/// Radio initialization failed
pub const ERROR_INITRADIO: u8 = 0x01;
/// Transmission failed
pub const ERROR_TXFAILED: u8 = 0x02;
/// EEPROM is locked
pub const ERROR_EEPROM_LOCKED: u8 = 0x03;
/// TX queue full (single-interface only)
pub const ERROR_QUEUE_FULL: u8 = 0x04;
/// Memory exhausted (single-interface only)
pub const ERROR_MEMORY_LOW: u8 = 0x05;
/// Modem communication timeout (single-interface only)
pub const ERROR_MODEM_TIMEOUT: u8 = 0x06;

// ---------------------------------------------------------------------------
// Protocol values
// ---------------------------------------------------------------------------

/// Detect request payload (host -> device)
pub const DETECT_REQ: u8 = 0x73;
/// Detect response payload (device -> host)
pub const DETECT_RESP: u8 = 0x46;
/// Radio off
pub const RADIO_STATE_OFF: u8 = 0x00;
/// Radio on
pub const RADIO_STATE_ON: u8 = 0x01;

// ---------------------------------------------------------------------------
// Platform constants
// ---------------------------------------------------------------------------

/// AVR-based RNode
pub const PLATFORM_AVR: u8 = 0x90;
/// ESP32-based RNode
pub const PLATFORM_ESP32: u8 = 0x80;
/// nRF52-based RNode
pub const PLATFORM_NRF52: u8 = 0x70;

// ---------------------------------------------------------------------------
// Firmware requirements
// ---------------------------------------------------------------------------

/// Minimum required firmware major version
pub const REQUIRED_FW_MAJ: u8 = 1;
/// Minimum required firmware minor version
pub const REQUIRED_FW_MIN: u8 = 52;

// ---------------------------------------------------------------------------
// Frequency limits
// ---------------------------------------------------------------------------

/// Minimum allowed operating frequency (Hz)
pub const FREQ_MIN: u32 = 137_000_000;
/// Maximum allowed operating frequency (Hz), covers 2.4 GHz SX128X
pub const FREQ_MAX: u32 = 3_000_000_000;

// ---------------------------------------------------------------------------
// Hardware MTU
// ---------------------------------------------------------------------------

/// Hardware MTU for all RNode variants (bytes)
pub const HW_MTU: usize = 508;

// ---------------------------------------------------------------------------
// Decode/encode constants
// ---------------------------------------------------------------------------

/// RSSI raw-to-dBm offset (subtract from raw byte)
const RSSI_OFFSET: i16 = 157;
/// Temperature raw-to-Celsius offset (subtract from raw byte)
const TEMP_OFFSET: i16 = 120;
/// Interference sentinel (0xFF = no interference data)
const INTERFERENCE_NONE: u8 = 0xFF;
/// Maximum TX power (dBm)
pub const MAX_TX_POWER: u8 = 37;
/// Leave command payload
const LEAVE_PAYLOAD: u8 = 0xFF;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Battery state reported by the device
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatteryState {
    #[default]
    Unknown,
    Discharging,
    Charging,
    Charged,
}

impl BatteryState {
    /// Lowercase label matching Python `RNodeInterface.get_battery_state_string()`
    /// (`"charged"` / `"charging"` / `"discharging"` / `"unknown"`), reported in
    /// the `battery_state` field of `interface_stats` for rnstatus/lnstatus.
    pub fn as_str(self) -> &'static str {
        match self {
            BatteryState::Unknown => "unknown",
            BatteryState::Discharging => "discharging",
            BatteryState::Charging => "charging",
            BatteryState::Charged => "charged",
        }
    }
}

/// Channel time/utilization statistics from CMD_STAT_CHTM
///
/// All `u16` values are raw device values. Divide by 100 for percent.
/// RSSI fields are already converted to dBm (raw byte - 157).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelStats {
    /// Short-term airtime (raw; /100 for percent)
    pub airtime_short: u16,
    /// Long-term airtime (raw; /100 for percent)
    pub airtime_long: u16,
    /// Short-term channel load (raw; /100 for percent)
    pub channel_load_short: u16,
    /// Long-term channel load (raw; /100 for percent)
    pub channel_load_long: u16,
    /// Current RSSI in dBm (single-interface only)
    pub current_rssi: Option<i16>,
    /// Noise floor in dBm (single-interface only)
    pub noise_floor: Option<i16>,
    /// Interference in dBm, None if 0xFF (single-interface only)
    pub interference: Option<i16>,
}

/// Physical layer parameters from CMD_STAT_PHYPRM
///
/// All `u16` values are raw device values unless otherwise noted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhyParams {
    /// Symbol time (raw; /1000 for ms)
    pub symbol_time_raw: u16,
    /// Symbol rate (baud)
    pub symbol_rate: u16,
    /// Preamble symbols
    pub preamble_symbols: u16,
    /// Preamble time (ms)
    pub preamble_time_ms: u16,
    /// CSMA slot time (ms)
    pub csma_slot_time_ms: u16,
    /// DIFS time (ms), None for multi-interface
    pub difs_time_ms: Option<u16>,
}

// ---------------------------------------------------------------------------
// Encoding functions
// ---------------------------------------------------------------------------

/// Build a single KISS frame: FEND + command + escaped payload + FEND
fn build_single_frame(cmd: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    kiss::frame(cmd, payload, &mut out);
    out
}

/// Build the 4-frame detect + query sequence
///
/// Returns the fixed 13-byte sequence that queries device presence,
/// firmware version, platform, and MCU. Frames share FEND delimiters.
///
/// ```text
/// C0 08 73 C0 50 00 C0 48 00 C0 49 00 C0
/// ```
pub fn build_detect_query() -> Vec<u8> {
    // This is a fixed sequence with shared FENDs between frames.
    // Cannot use frame() directly since it clears the output each time.
    alloc::vec![
        kiss::FEND,
        CMD_DETECT,
        DETECT_REQ,
        kiss::FEND,
        CMD_FW_VERSION,
        0x00,
        kiss::FEND,
        CMD_PLATFORM,
        0x00,
        kiss::FEND,
        CMD_MCU,
        0x00,
        kiss::FEND,
    ]
}

/// Build a CMD_FREQUENCY frame (4 bytes big-endian Hz)
pub fn build_set_frequency(hz: u32) -> Vec<u8> {
    build_single_frame(CMD_FREQUENCY, &hz.to_be_bytes())
}

/// Build a CMD_BANDWIDTH frame (4 bytes big-endian Hz)
pub fn build_set_bandwidth(hz: u32) -> Vec<u8> {
    build_single_frame(CMD_BANDWIDTH, &hz.to_be_bytes())
}

/// Build a CMD_TXPOWER frame (1 byte dBm)
pub fn build_set_txpower(dbm: u8) -> Vec<u8> {
    build_single_frame(CMD_TXPOWER, &[dbm])
}

/// Build a CMD_SF frame (1 byte spreading factor)
pub fn build_set_sf(sf: u8) -> Vec<u8> {
    build_single_frame(CMD_SF, &[sf])
}

/// Build a CMD_CR frame (1 byte coding rate)
pub fn build_set_cr(cr: u8) -> Vec<u8> {
    build_single_frame(CMD_CR, &[cr])
}

/// Build a CMD_RADIO_STATE frame (1 byte state)
pub fn build_set_radio_state(state: u8) -> Vec<u8> {
    build_single_frame(CMD_RADIO_STATE, &[state])
}

/// Build a CMD_ST_ALOCK frame (2 bytes big-endian)
///
/// Caller computes `(percent * 100.0) as u16` before calling.
pub fn build_set_st_alock(value: u16) -> Vec<u8> {
    build_single_frame(CMD_ST_ALOCK, &value.to_be_bytes())
}

/// Build a CMD_LT_ALOCK frame (2 bytes big-endian)
///
/// Caller computes `(percent * 100.0) as u16` before calling.
pub fn build_set_lt_alock(value: u16) -> Vec<u8> {
    build_single_frame(CMD_LT_ALOCK, &value.to_be_bytes())
}

/// Build a CMD_DATA frame with KISS-escaped payload
pub fn build_data_frame(data: &[u8]) -> Vec<u8> {
    build_single_frame(CMD_DATA, data)
}

/// Build a CMD_LEAVE frame (0xFF payload)
pub fn build_leave() -> Vec<u8> {
    build_single_frame(CMD_LEAVE, &[LEAVE_PAYLOAD])
}

// ---------------------------------------------------------------------------
// Multi-interface (vport) framing
// ---------------------------------------------------------------------------
//
// A multi-transceiver RNode exposes several LoRa radios as virtual ports
// (vports). Every per-vport command -- radio config or data -- is preceded by a
// CMD_SEL_INT frame naming the target vport, so a single serial link multiplexes
// N logical interfaces. This mirrors Python `RNodeMultiInterface` (the
// `setFrequency`/`setBandwidth`/.../`process_outgoing` methods and the readLoop
// CMD_SEL_INT branch). The core stays a pure framing layer; the std interface
// owns the per-vport routing and lifecycle.

/// Maximum number of subinterfaces (virtual ports) on one RNode.
/// Matches Python `RNodeMultiInterface.MAX_SUBINTERFACES`.
pub const MAX_SUBINTERFACES: u8 = 11;

// Transceiver chip type bytes reported by CMD_INTERFACES (one per vport).
/// SX127x family (sub-GHz)
pub const CHIP_SX127X: u8 = 0x00;
/// SX1276
pub const CHIP_SX1276: u8 = 0x01;
/// SX1278
pub const CHIP_SX1278: u8 = 0x02;
/// SX126x family (sub-GHz)
pub const CHIP_SX126X: u8 = 0x10;
/// SX1262
pub const CHIP_SX1262: u8 = 0x11;
/// SX128x family (2.4 GHz)
pub const CHIP_SX128X: u8 = 0x20;
/// SX1280
pub const CHIP_SX1280: u8 = 0x21;

/// Map a CMD_INTERFACES chip-type byte to a family name, matching Python
/// `KISS.interface_type_to_str`. Unknown types fall back to `"SX127X"`.
pub fn interface_chip_name(chip: u8) -> &'static str {
    match chip {
        CHIP_SX126X | CHIP_SX1262 => "SX126X",
        CHIP_SX128X | CHIP_SX1280 => "SX128X",
        _ => "SX127X",
    }
}

/// Build a "select subinterface" frame: `FEND CMD_SEL_INT index FEND`.
///
/// Tells the firmware which vport the following frame targets. The index byte
/// is emitted raw (not KISS-escaped), byte-for-byte with Python
/// `RNodeMultiInterface` -- valid vport indices (`0..MAX_SUBINTERFACES`) never
/// collide with FEND/FESC.
pub fn build_select_interface(index: u8) -> Vec<u8> {
    alloc::vec![kiss::FEND, CMD_SEL_INT, index, kiss::FEND]
}

/// Prefix a complete command frame with a vport-select frame.
///
/// `command_frame` is any single KISS frame from the `build_*` family. The
/// result is `select(index) ++ command_frame`, byte-for-byte identical to the
/// frames Python's `RNodeMultiInterface` writes for that vport (each of its
/// setters emits `FEND CMD_SEL_INT idx FEND` immediately followed by the
/// single-command frame).
pub fn build_vport_command(index: u8, command_frame: &[u8]) -> Vec<u8> {
    let mut out = build_select_interface(index);
    out.extend_from_slice(command_frame);
    out
}

/// Build a vport-tagged data frame: `select(index)` followed by a CMD_DATA
/// frame carrying the KISS-escaped payload. Mirrors
/// `RNodeMultiInterface.process_outgoing`.
pub fn build_vport_data_frame(index: u8, data: &[u8]) -> Vec<u8> {
    build_vport_command(index, &build_data_frame(data))
}

/// Build the multi-interface detect + query sequence.
///
/// Identical to [`build_detect_query`] but appends a CMD_INTERFACES query, so
/// the device reports its per-vport transceiver types (Python
/// `RNodeMultiInterface.detect`).
pub fn build_detect_query_multi() -> Vec<u8> {
    alloc::vec![
        kiss::FEND,
        CMD_DETECT,
        DETECT_REQ,
        kiss::FEND,
        CMD_FW_VERSION,
        0x00,
        kiss::FEND,
        CMD_PLATFORM,
        0x00,
        kiss::FEND,
        CMD_MCU,
        0x00,
        kiss::FEND,
        CMD_INTERFACES,
        0x00,
        kiss::FEND,
    ]
}

/// Decode a CMD_SEL_INT payload into the selected vport index.
pub fn decode_select_interface(payload: &[u8]) -> Option<u8> {
    payload.first().copied()
}

/// Decode a CMD_INTERFACES report into per-vport chip-type bytes.
///
/// The payload is a sequence of 2-byte records in vport order (vport 0 first).
/// The second byte of each record is the transceiver chip type; the first is
/// reserved. Mirrors the Python readLoop, which appends one entry per 2-byte
/// record. A trailing odd byte (malformed frame) is ignored.
pub fn decode_interfaces(payload: &[u8]) -> Vec<u8> {
    payload.chunks_exact(2).map(|record| record[1]).collect()
}

// ---------------------------------------------------------------------------
// Decoding functions
// ---------------------------------------------------------------------------

/// Decode RSSI from CMD_STAT_RSSI payload
///
/// Returns RSSI in dBm: `raw_byte as i16 - 157`.
pub fn decode_rssi(payload: &[u8]) -> Option<i16> {
    let &byte = payload.first()?;
    Some(byte as i16 - RSSI_OFFSET)
}

/// Decode SNR from CMD_STAT_SNR payload
///
/// Returns the raw signed byte. Caller multiplies by 0.25 for dB.
pub fn decode_snr(payload: &[u8]) -> Option<i8> {
    let &byte = payload.first()?;
    Some(byte as i8)
}

/// Decode battery status from CMD_STAT_BAT payload
///
/// Returns `(BatteryState, percent)`.
pub fn decode_battery(payload: &[u8]) -> Option<(BatteryState, u8)> {
    if payload.len() < 2 {
        return None;
    }
    let state = match payload[0] {
        0x01 => BatteryState::Discharging,
        0x02 => BatteryState::Charging,
        0x03 => BatteryState::Charged,
        _ => BatteryState::Unknown,
    };
    Some((state, payload[1]))
}

/// Decode temperature from CMD_STAT_TEMP payload
///
/// Returns temperature in Celsius: `raw_byte as i16 - 120`.
pub fn decode_temperature(payload: &[u8]) -> Option<i16> {
    let &byte = payload.first()?;
    Some(byte as i16 - TEMP_OFFSET)
}

/// Decode firmware version from CMD_FW_VERSION payload
///
/// Returns `(major, minor)`.
pub fn decode_firmware_version(payload: &[u8]) -> Option<(u8, u8)> {
    if payload.len() < 2 {
        return None;
    }
    Some((payload[0], payload[1]))
}

/// Read a big-endian u16 from a slice at a given offset
fn read_be_u16(data: &[u8], offset: usize) -> Option<u16> {
    if offset + 2 > data.len() {
        return None;
    }
    Some(u16::from_be_bytes([data[offset], data[offset + 1]]))
}

/// Decode channel statistics from CMD_STAT_CHTM payload
///
/// Accepts 11-byte (single-interface) or 8-byte (multi-interface) payloads.
/// For single-interface, includes RSSI/noise_floor/interference fields.
/// Interference is `None` when the raw byte is 0xFF.
pub fn decode_channel_stats(payload: &[u8]) -> Option<ChannelStats> {
    if payload.len() < 8 {
        return None;
    }

    let airtime_short = read_be_u16(payload, 0)?;
    let airtime_long = read_be_u16(payload, 2)?;
    let channel_load_short = read_be_u16(payload, 4)?;
    let channel_load_long = read_be_u16(payload, 6)?;

    let (current_rssi, noise_floor, interference) = if payload.len() >= 11 {
        let rssi = payload[8] as i16 - RSSI_OFFSET;
        let noise = payload[9] as i16 - RSSI_OFFSET;
        let interf = if payload[10] == INTERFERENCE_NONE {
            None
        } else {
            Some(payload[10] as i16 - RSSI_OFFSET)
        };
        (Some(rssi), Some(noise), interf)
    } else {
        (None, None, None)
    };

    Some(ChannelStats {
        airtime_short,
        airtime_long,
        channel_load_short,
        channel_load_long,
        current_rssi,
        noise_floor,
        interference,
    })
}

/// Decode physical parameters from CMD_STAT_PHYPRM payload
///
/// Accepts 12-byte (single-interface) or 10-byte (multi-interface) payloads.
/// For multi-interface, `difs_time_ms` is `None`.
pub fn decode_phy_params(payload: &[u8]) -> Option<PhyParams> {
    if payload.len() < 10 {
        return None;
    }

    let symbol_time_raw = read_be_u16(payload, 0)?;
    let symbol_rate = read_be_u16(payload, 2)?;
    let preamble_symbols = read_be_u16(payload, 4)?;
    let preamble_time_ms = read_be_u16(payload, 6)?;
    let csma_slot_time_ms = read_be_u16(payload, 8)?;

    let difs_time_ms = if payload.len() >= 12 {
        Some(read_be_u16(payload, 10)?)
    } else {
        None
    };

    Some(PhyParams {
        symbol_time_raw,
        symbol_rate,
        preamble_symbols,
        preamble_time_ms,
        csma_slot_time_ms,
        difs_time_ms,
    })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Error returned by [`validate_config`] when a radio parameter is out of range
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    FrequencyOutOfRange,
    InvalidBandwidth,
    TxPowerOutOfRange,
    SpreadingFactorOutOfRange,
    CodingRateOutOfRange,
}

impl core::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FrequencyOutOfRange => write!(f, "frequency out of range (137 MHz - 3 GHz)"),
            Self::InvalidBandwidth => write!(f, "invalid bandwidth"),
            Self::TxPowerOutOfRange => write!(f, "TX power out of range (0-{MAX_TX_POWER} dBm)"),
            Self::SpreadingFactorOutOfRange => {
                write!(f, "spreading factor out of range (5-12)")
            }
            Self::CodingRateOutOfRange => write!(f, "coding rate out of range (5-8)"),
        }
    }
}

/// Check whether the firmware version meets the minimum requirement
pub fn validate_firmware(major: u8, minor: u8) -> bool {
    (major, minor) >= (REQUIRED_FW_MAJ, REQUIRED_FW_MIN)
}

/// TX power a LoRa interface asks for when its config names no `txpower`.
///
/// 22 dBm is the ceiling of the SX1262 high-power PA and the highest value an
/// RNode-firmware board accepts before it clamps
/// (`RNode_Firmware/RNode_Firmware.ino:861-875`), so "the board maximum" is
/// what a board that can do less will clamp this request down to.
///
/// **This is a deliberate deviation from Python-Reticulum**, which resolves an
/// absent `txpower` to 0 dBm (`RNS/Interfaces/RNodeInterface.py`, `txpower`
/// defaulting to 0). The deviation is admissible under the project's rule: no
/// wire field changes, no behaviour a Python peer expects from a neighbour
/// changes — no peer ever learns our TX power — and it removes a failure mode
/// with no symptom at the node. A 0 dBm node is 1 mW, transmits, logs nothing
/// unusual, and is simply not heard; 22 dBm is 158 mW. See
/// `docs/src/concepts/python-rns-compatibility.md`.
///
/// Regulatory note, EU 863-870 MHz: 27 dBm e.r.p. is permitted **only** in
/// 869.4-869.65 MHz (ERC Recommendation 70-03, Annex 1, sub-band h1.7). There,
/// 22 dBm *conducted* stays inside the limit up to roughly 7 dBi of antenna
/// gain (22 + 7 - 2.15 dBd correction ≈ 26.9 dBm e.r.p.); above that the
/// operator has to reduce power explicitly, and that residual is stated in the
/// documentation rather than warned about at runtime, because the stack does
/// not know the antenna. Every other listed European sub-band permits at most
/// 25 mW e.r.p. = 14 dBm, which is why this constant is only ever applied
/// through [`resolve_tx_power`], where [`lawful_erp_dbm`] caps it by frequency.
pub const DEFAULT_TX_POWER_DBM: i8 = 22;

/// Resolve a configured `txpower` to the value the radio is asked for.
///
/// `None` — the key is absent from the interface block — resolves to
/// [`DEFAULT_TX_POWER_DBM`] capped by the lawful e.r.p. limit for `freq_hz`
/// ([`lawful_erp_dbm`]): the out-of-box default reaches as far as the board
/// can, but no further than the band allows. A frequency the table has no
/// entry for gets the uncapped board maximum and a warning — a limit is not
/// invented for a band this tree cannot cite, mirroring
/// [`firmware_default_lt_alock`]'s treatment of the airtime lock.
///
/// `Some(x)` is returned verbatim, **including an explicit `Some(0)`** (an
/// operator who writes `txpower = 0` means 0, the same way an explicit
/// `airtime_limit_long = 0` means unlimited) **and including a value above
/// the derived cap**: the operator may know something the table does not — a
/// licence, a different jurisdiction — so the explicit value wins and the
/// excess is logged, not clamped. Callers must not collapse `None` into
/// `Some(0)` before this point, or the two become indistinguishable.
pub fn resolve_tx_power(configured: Option<i8>, freq_hz: u64) -> i8 {
    match (configured, lawful_erp_dbm(freq_hz)) {
        (Some(x), Some(cap)) if x > cap => {
            crate::tracing::warn!(
                "txpower {} dBm exceeds the derived ERP limit {} dBm for {} Hz; \
                 honouring the explicit value",
                x,
                cap,
                freq_hz
            );
            x
        }
        (Some(x), _) => x,
        (None, Some(cap)) if cap < DEFAULT_TX_POWER_DBM => {
            crate::tracing::info!(
                "no txpower configured, using {} dBm (board max {} dBm, capped by \
                 ERC 70-03 for {} MHz)",
                cap,
                DEFAULT_TX_POWER_DBM,
                freq_hz as f64 / 1e6
            );
            cap
        }
        (None, Some(_)) => {
            crate::tracing::info!(
                "no txpower configured, using board maximum {} dBm",
                DEFAULT_TX_POWER_DBM
            );
            DEFAULT_TX_POWER_DBM
        }
        (None, None) => {
            crate::tracing::warn!(
                "no ERP limit known for {} Hz; using board maximum {} dBm",
                freq_hz,
                DEFAULT_TX_POWER_DBM
            );
            DEFAULT_TX_POWER_DBM
        }
    }
}

/// Validate radio configuration parameters
///
/// Returns `Ok(())` if all parameters are within valid ranges,
/// or `Err(ConfigError)` for the first invalid parameter.
pub fn validate_config(freq: u32, bw: u32, txp: u8, sf: u8, cr: u8) -> Result<(), ConfigError> {
    if !(FREQ_MIN..=FREQ_MAX).contains(&freq) {
        return Err(ConfigError::FrequencyOutOfRange);
    }
    // Valid LoRa bandwidths (Hz)
    match bw {
        7800 | 10400 | 15600 | 20800 | 31250 | 41700 | 62500 | 125000 | 250000 | 500000 => {}
        _ => return Err(ConfigError::InvalidBandwidth),
    }
    if txp > MAX_TX_POWER {
        return Err(ConfigError::TxPowerOutOfRange);
    }
    if !(5..=12).contains(&sf) {
        return Err(ConfigError::SpreadingFactorOutOfRange);
    }
    if !(5..=8).contains(&cr) {
        return Err(ConfigError::CodingRateOutOfRange);
    }
    Ok(())
}

/// Compute the on-air bitrate for a LoRa configuration
///
/// Formula: `sf * 4 * bandwidth / (cr * 2^sf)` (integer arithmetic,
/// algebraically equivalent to Python's `sf * (4.0/cr) / (2^sf / (bw/1000)) * 1000`).
/// Returns 0 for invalid inputs (sf=0, cr=0, bandwidth=0, sf>63).
pub fn compute_bitrate(sf: u8, cr: u8, bandwidth: u32) -> u32 {
    if cr == 0 || bandwidth == 0 || sf == 0 || sf > 63 {
        return 0;
    }
    // Use u64 to avoid overflow for large bandwidth * sf * 4
    let numerator = sf as u64 * 4 * bandwidth as u64;
    let denominator = cr as u64 * (1u64 << sf);
    (numerator / denominator) as u32
}

/// Preamble floor in symbols (`LORA_PREAMBLE_SYMBOLS_MIN`,
/// `RNode_Firmware/Config.h:84`).
pub const LORA_PREAMBLE_SYMBOLS_MIN: u16 = 18;

/// Preamble duration the reference aims for, in milliseconds
/// (`LORA_PREAMBLE_TARGET_MS`, `RNode_Firmware/Config.h:85`).
pub const LORA_PREAMBLE_TARGET_MS: u32 = 24;

/// Milliseconds subtracted from the target above the fast-rate threshold
/// (`LORA_PREAMBLE_FAST_DELTA`, `RNode_Firmware/Config.h:86`).
pub const LORA_PREAMBLE_FAST_DELTA: u32 = 18;

/// Bitrate above which the reference calls a link "fast" and shortens the
/// preamble target (`LORA_FAST_THRESHOLD_BPS`, `RNode_Firmware/Config.h:87`).
pub const LORA_FAST_THRESHOLD_BPS: u32 = 30_000;

/// The preamble length in symbols the RNode firmware programs for a given
/// PHY, which is what a peer running that firmware expects to hear.
///
/// The reference scales the preamble to a target *duration* with a symbol
/// floor, so the symbol count falls as the spreading factor rises. Ours was
/// a constant 24 for every SF, which agrees with the reference at SF7/BW125
/// and disagrees from SF8 down — the entire long-range regime (Codeberg
/// issue in the commit message).
///
/// Reference: `RNode_Firmware/Config.h:84-87` for the constants and
/// `RNode_Firmware/Utilities.h:1235-1258` for the derivation:
///
/// ```text
/// lora_symbol_rate     = bw / 2^sf                       [Hz]
/// lora_symbol_time_ms  = 1000 / lora_symbol_rate         [ms]
/// fast_rate            = lora_bitrate > LORA_FAST_THRESHOLD_BPS
/// target_ms            = LORA_PREAMBLE_TARGET_MS - (fast_rate ? FAST_DELTA : 0)
/// target_symbols       = target_ms / lora_symbol_time_ms
/// if target_symbols < MIN { MIN } else { ceil(target_symbols) }
/// ```
///
/// `lora_bitrate` is the same quantity [`compute_bitrate`] already computes;
/// the reference spells it at `Utilities.h:1237` and truncates to `uint32_t`,
/// which is what integer division does here.
///
/// # Where this is not bit-identical to the reference
///
/// The reference evaluates the last three lines in floating point;
/// this evaluates the exact rational `ceil(target_ms * bw / (2^sf * 1000))`
/// in integers. The two agree whenever the exact quotient is not within
/// float rounding error of an integer — and the quotient only has to be
/// rounded at all where it reaches 18, since below that both forms take the
/// floor and the rounding is never consulted.
///
/// Over the domain the reference itself admits — its ten bandwidths,
/// SF5..=SF12, CR4/5..4/8, 320 points — 64 reach the ceiling, the smallest
/// distance from one of those quotients to an integer is 0.125, the largest
/// quotient is 187.5 and its f32 ulp is 2.2e-5. Four orders of margin, so
/// the results are identical; `derive_preamble_matches_reference_float`
/// asserts that by brute force rather than by this paragraph.
///
/// Outside that domain (an arbitrary bandwidth from a config file) a
/// quotient can land exactly on an integer, and there this returns that
/// integer where the reference could return one more. That is a deliberate
/// difference and the only one.
///
/// Returns [`LORA_PREAMBLE_SYMBOLS_MIN`] for degenerate inputs (sf or
/// bandwidth zero, sf out of the range the modems accept), since the floor is
/// what the reference's own variable is initialised to (`Config.h:91`).
pub fn derive_preamble_symbols(sf: u8, cr: u8, bandwidth_hz: u32) -> u16 {
    if sf == 0 || sf > 12 || bandwidth_hz == 0 || cr == 0 {
        return LORA_PREAMBLE_SYMBOLS_MIN;
    }

    let target_ms = if compute_bitrate(sf, cr, bandwidth_hz) > LORA_FAST_THRESHOLD_BPS {
        LORA_PREAMBLE_TARGET_MS - LORA_PREAMBLE_FAST_DELTA
    } else {
        LORA_PREAMBLE_TARGET_MS
    };

    // target_ms / symbol_time_ms, with symbol_time_ms = 2^sf * 1000 / bw.
    let symbols = (target_ms as u64 * bandwidth_hz as u64).div_ceil((1u64 << sf) * 1000);

    // The clamp only bites for a bandwidth no modem offers (hundreds of MHz);
    // the reference stores a `long` and would not wrap there either.
    symbols.clamp(LORA_PREAMBLE_SYMBOLS_MIN as u64, u16::MAX as u64) as u16
}

/// Maximum random jitter (ms) for the first packet after idle.
/// Matches Python's PATHFINDER_RW = 0.5s.
pub const JITTER_MAX_MS: u64 = 500;

/// Minimum spacing (ms) between consecutive serial writes.
/// Prevents overrunning the serial buffer (508 bytes at 115200 baud ≈ 44ms).
/// For CSMA-fair pacing, use `compute_spacing_ms()` instead, this constant
/// is only the serial-level floor.
pub const MIN_SPACING_MS: u64 = 50;

/// Firmware CSMA DIFS time (ms). The firmware waits this long with the channel
/// clear before starting the contention window. From RNode_Firmware Config.h.
pub const CSMA_DIFS_MS: u64 = 48;

/// Firmware CSMA maximum contention window (ms). 15 slots × 24ms = 360ms.
/// From RNode_Firmware Config.h: csma_slot_ms=24, cw_max=15.
pub const CSMA_MAX_CW_MS: u64 = 360;

/// Safety margin (ms) added to airtime-based pacing to absorb firmware
/// processing jitter and serial I/O overhead.
pub const PACING_MARGIN_MS: u64 = 100;

/// Compute LoRa airtime in milliseconds for a given payload.
///
/// Uses the SX127x formula from Semtech AN1200.13. Parameters:
/// - `payload_bytes`: total bytes on the wire (header + data)
/// - `bandwidth_hz`: signal bandwidth in Hz (e.g., 62500, 125000, 250000)
/// - `sf`: spreading factor (6-12)
/// - `cr`: coding rate denominator (5-8, meaning 4/5 through 4/8)
///
/// Assumes the modem-default programmed preamble of 8 symbols; when the
/// radio is configured with a longer preamble (the RNode derivation yields
/// 18 at SF12, 24 at SF7), use [`airtime_ms_with_preamble`] — at SF12/BW125
/// the 10 extra symbols are 327 ms, more than a whole timeout slack.
///
/// Returns airtime in milliseconds, rounded up.
pub fn airtime_ms(payload_bytes: u32, bandwidth_hz: u32, sf: u8, cr: u8) -> u64 {
    airtime_ms_with_preamble(payload_bytes, bandwidth_hz, sf, cr, 8)
}

/// [`airtime_ms`] with an explicit programmed preamble length in symbols.
///
/// The on-air preamble is the programmed count plus the fixed 4.25-symbol
/// tail (sync word + start markers), i.e. `(4*n + 17)/4` symbols.
///
/// Returns 0 for degenerate inputs (bandwidth 0, sf 0, or sf above 63), the
/// same inputs [`compute_bitrate`] and [`derive_preamble_symbols`] reject:
/// `bandwidth_hz == 0` would divide by zero and `sf > 63` would overflow the
/// `1u64 << sf` shift. A total function here means a bad config value is
/// refused where it is read rather than panicking in the arithmetic
/// (Codeberg #274).
pub fn airtime_ms_with_preamble(
    payload_bytes: u32,
    bandwidth_hz: u32,
    sf: u8,
    cr: u8,
    preamble_symbols: u16,
) -> u64 {
    if sf == 0 || sf > 63 || bandwidth_hz == 0 {
        return 0;
    }
    // Symbol time: T_sym = 2^SF / BW (in seconds)
    // We compute in microseconds to avoid floating point.
    // T_sym_us = 2^SF * 1_000_000 / BW
    let sf = sf as u64;
    let bw = bandwidth_hz as u64;
    let t_sym_us = (1u64 << sf) * 1_000_000 / bw;

    // Preamble: programmed symbols + 4.25 symbols
    // In microseconds: (n + 4.25) * T_sym = ((4n + 17) * T_sym) / 4
    let t_preamble_us = (4 * preamble_symbols as u64 + 17) * t_sym_us / 4;

    // Payload symbol count (explicit header mode, CRC on):
    //   n_payload = 8 + max(ceil((8*PL - 4*SF + 28 + 16) / (4*(SF-2*DE))) * (CR-4+4), 0)
    // where DE=0 for SF<11, DE=1 for SF>=11
    // CR parameter is denominator (5-8), so (CR-4) gives 1-4
    let de: u64 = if sf >= 11 { 1 } else { 0 };
    let pl = payload_bytes as i64;
    let sf_i = sf as i64;
    let cr_factor = (cr as i64 - 4).max(1) as u64; // 1-4

    let numerator = 8 * pl - 4 * sf_i + 28 + 16;
    let denominator = 4 * (sf_i - 2 * de as i64);

    let n_extra = if numerator > 0 && denominator > 0 {
        let ceil_div = (numerator + denominator - 1) / denominator;
        (ceil_div as u64) * (cr_factor + 4)
    } else {
        0
    };
    let n_payload = 8 + n_extra;

    let t_payload_us = n_payload * t_sym_us;

    // Total airtime
    let total_us = t_preamble_us + t_payload_us;

    // Convert to ms, round up
    total_us.div_ceil(1000)
}

/// Compute CSMA-fair inter-frame spacing in milliseconds.
///
/// Ensures the firmware's TX queue has at most one frame, so `flush_queue()`
/// (which sends all queued frames back-to-back without CSMA) only sends one
/// frame per CSMA contest.
///
/// spacing = airtime(frame) + DIFS + max_CW + margin
///
/// The airtime term charges the programmed preamble: pacing that assumes the
/// modem-default 8 symbols undercounts every frame by (preamble-8)*t_sym.
pub fn compute_spacing_ms(
    payload_bytes: u32,
    bandwidth_hz: u32,
    sf: u8,
    cr: u8,
    preamble_symbols: u16,
) -> u64 {
    let air = airtime_ms_with_preamble(payload_bytes, bandwidth_hz, sf, cr, preamble_symbols);
    let spacing = air + CSMA_DIFS_MS + CSMA_MAX_CW_MS + PACING_MARGIN_MS;
    // Never go below the serial-level floor
    spacing.max(MIN_SPACING_MS)
}

// ---------------------------------------------------------------------------
// Radio config wire protocol (for runtime config override via serial)
// ---------------------------------------------------------------------------

/// Magic prefix for radio config frames (distinguishes from Reticulum packets).
pub const RADIO_CONFIG_MAGIC: [u8; 2] = [0xA4, 0xA4];

/// Total config frame payload length (2 magic + 19 parameter bytes).
pub const RADIO_CONFIG_FRAME_LEN: usize = 21;

/// ACK payload sent by T114 after applying radio config.
pub const RADIO_CONFIG_ACK: [u8; 3] = [0xA4, 0xA4, 0x01];

/// Host-to-board reboot request on the same magic-prefixed control channel
/// as the radio-config frame: the RNode protocol's `CMD_RESET` +
/// `CMD_RESET_BYTE` pair inside our magic envelope. The board ACKs with
/// [`RADIO_RESET_ACK`] and then performs a full system reset, which starts
/// the duty-cycle histogram, the radio configuration and all queues from
/// scratch.
///
/// This frame exists only on the USB host-to-modem control channel; it is
/// never carried on the mesh. It cannot swallow peer traffic either: a
/// valid Reticulum wire packet is at least 19 bytes (2 header, 16 address,
/// 1 context), so a 4-byte HDLC payload can never be one, and a Python
/// peer's SerialInterface never emits it — an rnsd-driven LNode simply
/// never sees the frame.
pub const RADIO_RESET_FRAME: [u8; 4] = [0xA4, 0xA4, CMD_RESET, CMD_RESET_BYTE];

/// ACK the board writes back immediately before rebooting, so the host can
/// tell "reset accepted" apart from "firmware predates the reset frame".
pub const RADIO_RESET_ACK: [u8; 3] = [0xA4, 0xA4, 0x02];

/// Parsed radio config from the wire format.
/// All values are in human-readable units (Hz, denominator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioConfigWire {
    pub frequency_hz: u32,
    pub bandwidth_hz: u32,
    pub sf: u8,
    pub cr: u8, // coding rate denominator (5-8)
    pub tx_power_dbm: i8,
    pub preamble_len: u16,
    pub csma_enabled: bool,
    /// When true, the T114 drops every outgoing LoRa packet at the driver
    /// level, the radio stays listening but never transmits. Used by the
    /// integration-test runner to neutralize T114s the scenario does not
    /// bind, so they cannot pollute the benchmark channel with their own
    /// Reticulum announces.
    pub radio_silent: bool,
    /// Short-term airtime limit, RNode `CMD_ST_ALOCK` u16 encoding
    /// (`percent * 100`, see [`alock_u16_to_fraction`]). `0` = unlimited.
    pub st_alock: u16,
    /// Long-term airtime limit, RNode `CMD_LT_ALOCK` u16 encoding
    /// (`percent * 100`, see [`alock_u16_to_fraction`]). `0` = unlimited.
    pub lt_alock: u16,
    /// Whether the parsed frame actually carried the `lt_alock` field (new
    /// 21-byte frame) rather than defaulting it to 0 (old backward-compat
    /// frame). Lets a standalone LNode tell "host set an explicit long-term
    /// airtime lock (including an explicit 0 = off)" apart from "no long-term
    /// lock was provided", so it can fall back to the ETSI lawful default
    /// derived from its own TX frequency (see [`firmware_default_lt_alock`]).
    pub lt_alock_present: bool,
}

/// Parse a radio config from wire bytes (13 to 19 bytes, after magic stripped).
///
/// Wire layout: freq_hz(4 BE) + bw_hz(4 BE) + sf(1) + cr(1) + tx_power(1) + preamble(2 BE)
/// + csma_enabled(1, optional, defaults to false if absent for backward compat)
/// + radio_silent(1, optional, defaults to false if absent for backward compat)
/// + st_alock(2 BE, optional, defaults to 0 if absent for backward compat)
/// + lt_alock(2 BE, optional, defaults to 0 if absent for backward compat)
pub fn parse_radio_config(data: &[u8]) -> Option<RadioConfigWire> {
    if !(13..=19).contains(&data.len()) {
        return None;
    }
    let frequency_hz = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let bandwidth_hz = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let sf = data[8];
    let cr = data[9];
    let tx_power_dbm = data[10] as i8;
    let preamble_len = u16::from_be_bytes([data[11], data[12]]);
    let csma_enabled = data.len() >= 14 && data[13] != 0;
    let radio_silent = data.len() >= 15 && data[14] != 0;
    let st_alock = if data.len() >= 17 {
        u16::from_be_bytes([data[15], data[16]])
    } else {
        0
    };
    let lt_alock_present = data.len() >= 19;
    let lt_alock = if lt_alock_present {
        u16::from_be_bytes([data[17], data[18]])
    } else {
        0
    };

    if !(5..=12).contains(&sf) {
        return None;
    }
    if !(5..=8).contains(&cr) {
        return None;
    }

    Some(RadioConfigWire {
        frequency_hz,
        bandwidth_hz,
        sf,
        cr,
        tx_power_dbm,
        preamble_len,
        csma_enabled,
        radio_silent,
        st_alock,
        lt_alock,
        lt_alock_present,
    })
}

/// Build a radio config frame payload (21 bytes including magic prefix).
pub fn build_radio_config_frame(cfg: &RadioConfigWire) -> Vec<u8> {
    let mut out = Vec::with_capacity(RADIO_CONFIG_FRAME_LEN);
    out.extend_from_slice(&RADIO_CONFIG_MAGIC);
    out.extend_from_slice(&cfg.frequency_hz.to_be_bytes());
    out.extend_from_slice(&cfg.bandwidth_hz.to_be_bytes());
    out.push(cfg.sf);
    out.push(cfg.cr);
    out.push(cfg.tx_power_dbm as u8);
    out.extend_from_slice(&cfg.preamble_len.to_be_bytes());
    out.push(cfg.csma_enabled as u8);
    out.push(cfg.radio_silent as u8);
    out.extend_from_slice(&cfg.st_alock.to_be_bytes());
    out.extend_from_slice(&cfg.lt_alock.to_be_bytes());
    out
}

// ---------------------------------------------------------------------------
// Airtime lock (regulatory duty-cycle enforcement)
// ---------------------------------------------------------------------------
//
// Mirrors the airtime lock in Mark Qvist's RNode firmware so the same host
// airtime-limit value produces the same enforced fraction on our standalone
// LNode as on a stock RNode. Reference (reference/RNode_Firmware):
//   * Config.h:181-198  AIRTIME_LONGTERM / AIRTIME_BINLEN_MS / AIRTIME_BINS,
//                       airtime_bins[], st_airtime_limit, lt_airtime_limit,
//                       airtime_lock, current_airtime_bin().
//   * RNode_Firmware.ino:654-714  add_airtime() / update_airtime().
//   * RNode_Firmware.ino:1673-1696  the lock computation.
//   * RNode_Firmware.ino:932-977  CMD_ST_ALOCK / CMD_LT_ALOCK u16 parsing.
//   * Utilities.h:941-957  the u16 report encoding.
//
// The airtime accounting itself (which frames, how they are keyed) belongs to
// the interface layer; core only provides the medium-agnostic bookkeeping and
// the exact wire-compatible u16<->fraction mapping.

/// Long-term airtime window, in seconds (Config.h:181 `AIRTIME_LONGTERM`).
pub const AIRTIME_LONGTERM_S: u64 = 3600;
/// Long-term airtime window, in milliseconds (Config.h:182).
pub const AIRTIME_LONGTERM_MS: u64 = AIRTIME_LONGTERM_S * 1000;
/// Length of one airtime histogram bin, in milliseconds. Mark derives this from
/// `STATUS_INTERVAL_MS * DCD_SAMPLES = 3 * 2500` (Config.h:176,178,183).
pub const AIRTIME_BINLEN_MS: u64 = 7500;
/// Number of histogram bins spanning the long-term window (Config.h:184):
/// `3_600_000 / 7500 = 480`. A `[u16; 480]` bin array is 960 bytes.
pub const AIRTIME_BINS: usize = (AIRTIME_LONGTERM_MS / AIRTIME_BINLEN_MS) as usize;

/// Convert a `CMD_ST_ALOCK` / `CMD_LT_ALOCK` u16 into an airtime-limit fraction.
///
/// Byte-for-byte compatible with the RNode firmware
/// (`RNode_Firmware.ino:950` `st_airtime_limit = (float)at/(100.0*100.0)`):
/// the fraction is `at / 10000`. `at == 0` means "unlimited" (0.0), and any
/// value mapping to `>= 1.0` is also treated as unlimited (0.0), matching
/// `RNode_Firmware.ino:951`. The host sends `at = percent * 100`
/// (`build_set_st_alock`), so e.g. `5000 -> 0.5` (50% duty cycle).
pub fn alock_u16_to_fraction(at: u16) -> f32 {
    if at == 0 {
        return 0.0;
    }
    let f = at as f32 / (100.0 * 100.0);
    if f >= 1.0 {
        0.0
    } else {
        f
    }
}

/// ETSI duty-cycle cap for a European SRD frequency, as a fraction of the
/// long-term window, or `None` if the frequency is outside every listed band
/// or in a guard gap.
///
/// The rows are read from ERC Recommendation 70-03, Annex 1 — the 863-870 MHz
/// wideband sub-bands h1.3-h1.9 (also carried by EN 300 220-2) and the
/// 433.05-434.79 MHz entry. The letters K-Q are this tree's historical row
/// names; the standard's own designations are given alongside:
///
/// | Sub-band  | Range (MHz)     | Duty cycle | Fraction |
/// |-----------|-----------------|------------|----------|
/// | g1        | 433.05 - 434.79 | 10%        | 0.10     |
/// | K (h1.3)  | 863.0 - 865.0   | 0.1%       | 0.001    |
/// | L (h1.4)  | 865.0 - 868.0   | 1%         | 0.01     |
/// | M (h1.5)  | 868.0 - 868.6   | 1%         | 0.01     |
/// | N (h1.6)  | 868.7 - 869.2   | 0.1%       | 0.001    |
/// | P (h1.7)  | 869.4 - 869.65  | 10%        | 0.10     |
/// | Q (h1.9)  | 869.7 - 870.0   | 1%         | 0.01     |
///
/// The name says `eu868` for historical reasons; the 433.05-434.79 MHz row
/// was added when the table was verified against the standard text, because
/// without it a 433 MHz node derived no airtime limit at all.
///
/// Guard gaps (868.6-868.7, 869.2-869.4, 869.65-869.7 MHz — narrowband-only
/// bands a LoRa carrier is warned about, and then honoured, on at RNode
/// interface build, see [`erp_band_gap`]) and frequencies outside every
/// listed band return `None`
/// (no regulatory auto cap applies). The returned fraction maps to the
/// firmware `lt_alock` via `fraction * 10000` (see `alock_u16_to_fraction`).
///
/// Every sub-band's requirement reads "<= x% duty cycle **or** LBT+AFA". AFA
/// (adaptive frequency agility, i.e. changing channel) is impossible for a
/// fixed-frequency node by construction, so the duty cycle is the only
/// compliance route open here.
///
/// This is a lawful-by-default helper: it lets a LoRa interface derive the
/// long-term airtime limit from its own TX frequency when the operator has not
/// set one. It is receiver-invisible (the cap only shapes local TX) and
/// Python-Reticulum does not do it, so it is our Priority-1 enhancement rather
/// than a wire/semantic requirement.
pub fn etsi_eu868_duty_cycle(freq_hz: u64) -> Option<f64> {
    match freq_hz {
        433_050_000..=434_789_999 => Some(0.10), // g1 433.05-434.79: 10%
        863_000_000..=864_999_999 => Some(0.001), // K (h1.3) 863.0-865.0: 0.1%
        865_000_000..=867_999_999 => Some(0.01), // L (h1.4) 865.0-868.0: 1%
        868_000_000..=868_599_999 => Some(0.01), // M (h1.5) 868.0-868.6: 1%
        868_700_000..=869_199_999 => Some(0.001), // N (h1.6) 868.7-869.2: 0.1%
        869_400_000..=869_649_999 => Some(0.10), // P (h1.7) 869.4-869.65: 10%
        869_700_000..=869_999_999 => Some(0.01), // Q (h1.9) 869.7-870.0: 1%
        _ => None,                               // guard gaps + out-of-band
    }
}

/// Lawful e.r.p. limit in dBm for a LoRa TX frequency, or `None` when the
/// table has no entry for it.
///
/// The rows are read from ERC Recommendation 70-03, Annex 1 ("Non-specific
/// Short Range Devices") — the same sub-bands the duty-cycle table
/// ([`etsi_eu868_duty_cycle`]) is built from:
///
/// | Sub-band | Range (MHz)     | Max power     | dBm |
/// |----------|-----------------|---------------|-----|
/// | g1       | 433.05 - 434.79 | 10 mW e.r.p.  | 10  |
/// | h1.3     | 863.0 - 865.0   | 25 mW e.r.p.  | 14  |
/// | h1.4     | 865.0 - 868.0   | 25 mW e.r.p.  | 14  |
/// | h1.5     | 868.0 - 868.6   | 25 mW e.r.p.  | 14  |
/// | h1.6     | 868.7 - 869.2   | 25 mW e.r.p.  | 14  |
/// | h1.7     | 869.4 - 869.65  | 500 mW e.r.p. | 27  |
/// | h1.9     | 869.7 - 870.0   | 25 mW e.r.p.  | 14  |
///
/// 869.7-870 MHz carries two options in the standard: h1.8 at 5 mW with no
/// duty-cycle requirement, and h1.9 at 25 mW with <= 1% duty cycle. A 125 kHz
/// LoRa carrier here uses the h1.9 option — 25 mW under the duty cycle the
/// airtime lock already enforces — so the row reads 14 dBm, not 7.
///
/// 500 mW is the exception, not the rule: every other listed European band
/// allows 25 mW e.r.p. or less. The bands *between* the h1.x entries are not
/// "no entry" — they are narrowband allocations a LoRa carrier is warned
/// about on at RNode interface build ([`erp_band_gap`]) and then permitted;
/// a default derived here for such a frequency therefore falls through to
/// `None`, the same as any other uncitable band.
///
/// `None` means the tree cannot cite a limit for the frequency (US 902-928,
/// AU/NZ, ...): no cap is derived and the caller warns, exactly as the
/// airtime lock behaves — a limit invented from memory would read as
/// authoritative to exactly the operator who most needs it not to be.
pub fn lawful_erp_dbm(freq_hz: u64) -> Option<i8> {
    match freq_hz {
        433_050_000..=434_789_999 => Some(10), // g1 433.05-434.79: 10 mW
        863_000_000..=864_999_999 => Some(14), // h1.3 863.0-865.0: 25 mW
        865_000_000..=867_999_999 => Some(14), // h1.4 865.0-868.0: 25 mW
        868_000_000..=868_599_999 => Some(14), // h1.5 868.0-868.6: 25 mW
        868_700_000..=869_199_999 => Some(14), // h1.6 868.7-869.2: 25 mW
        869_400_000..=869_649_999 => Some(27), // h1.7 869.4-869.65: 500 mW
        869_700_000..=869_999_999 => Some(14), // h1.9 869.7-870.0: 25 mW
        _ => None,
    }
}

/// The narrowband band a LoRa signal's occupied bandwidth would overlap, or
/// `None` when the signal stays clear of all of them.
///
/// The bands between the wideband h1.x entries of ERC Recommendation 70-03,
/// Annex 1 — 868.6-868.7, 869.2-869.25, 869.25-869.3, 869.3-869.4 and
/// 869.65-869.7 MHz, allocated to alarms in Annex 7 of the same
/// recommendation — permit only <= 25 kHz channel spacing, which no LoRa
/// bandwidth this stack configures can satisfy. A configured centre frequency
/// whose occupied bandwidth (approximated by the signal bandwidth) touches
/// one of them is therefore **worth a loud warning at interface build**, not
/// silence: falling through to "no known limit, board maximum" without a word
/// would be the most permissive outcome exactly where the operator most needs
/// to be told.
///
/// It is a warning and never a refusal. Project policy: no radio
/// configuration is refused for a radio-regulatory reason — the operator, not
/// this software, carries the legal responsibility for compliant operation,
/// and the same carrier is lawful under a licence, in another region, or in a
/// shielded chamber.
///
/// The overlap test is strict on both edges: a signal whose occupied band
/// *ends* exactly where an alarm band begins (e.g. an upper edge at precisely
/// 868.6 MHz) does not overlap it.
pub fn erp_band_gap(centre_hz: u64, bandwidth_hz: u32) -> Option<&'static str> {
    const GAPS: [(u64, u64, &str); 5] = [
        (868_600_000, 868_700_000, "868.6-868.7 MHz"),
        (869_200_000, 869_250_000, "869.2-869.25 MHz"),
        (869_250_000, 869_300_000, "869.25-869.3 MHz"),
        (869_300_000, 869_400_000, "869.3-869.4 MHz"),
        (869_650_000, 869_700_000, "869.65-869.7 MHz"),
    ];
    let half = u64::from(bandwidth_hz) / 2;
    let lo = centre_hz.saturating_sub(half);
    let hi = centre_hz.saturating_add(half);
    GAPS.iter()
        .find(|&&(gap_lo, gap_hi, _)| lo < gap_hi && hi > gap_lo)
        .map(|&(_, _, name)| name)
}

/// Resolve the long-term airtime lock (`lt_alock` u16) a standalone LNode's
/// firmware should enforce, given its TX `freq_hz` and any `explicit` value the
/// host provided on the radio-config frame.
///
/// An explicit value always wins, including an explicit `0` (which the airtime
/// lock reads as "unlimited"): a Part-3-aware host already resolves the right
/// long-term cap and sends it verbatim, so the firmware must not second-guess
/// it. When no explicit value was provided (`None`, e.g. the standalone
/// compiled default or an old-format frame that predates the alock field), the
/// firmware derives the ETSI EU868 lawful default from its own frequency via
/// [`etsi_eu868_duty_cycle`], mapped to the `fraction * 10000` u16 encoding.
/// Non-EU / out-of-band frequencies with no explicit value stay off (`0`).
///
/// This mirrors the host-side resolution so a standalone LNode on an EU
/// 863-870 MHz channel is lawful out of the box even though no host ever runs
/// its Part-3 helper.
pub fn firmware_default_lt_alock(freq_hz: u64, explicit: Option<u16>) -> u16 {
    match explicit {
        Some(v) => v,
        None => etsi_eu868_duty_cycle(freq_hz)
            .map(|fraction| (fraction * 10000.0) as u16)
            .unwrap_or(0),
    }
}

/// Total on-air time, in milliseconds, of the LoRa frame(s) carrying a
/// Reticulum packet of `data_len` bytes.
///
/// Mirrors [`build_lora_frames`]: a payload over [`MAX_SINGLE_PAYLOAD`] is sent
/// as two frames, each with a 1-byte header; otherwise one frame with a 1-byte
/// header. Each frame carries a full programmed preamble on the air, so the
/// preamble is charged per frame, keeping the accounting consistent with the
/// per-frame ledger charge ([`frame_airtime_cost_ms`]).
pub fn packet_airtime_ms(
    data_len: usize,
    bandwidth_hz: u32,
    sf: u8,
    cr: u8,
    preamble_symbols: u16,
) -> u64 {
    if data_len > MAX_SINGLE_PAYLOAD {
        let f1 = (1 + MAX_SINGLE_PAYLOAD) as u32;
        let f2 = (1 + data_len - MAX_SINGLE_PAYLOAD) as u32;
        airtime_ms_with_preamble(f1, bandwidth_hz, sf, cr, preamble_symbols)
            + airtime_ms_with_preamble(f2, bandwidth_hz, sf, cr, preamble_symbols)
    } else {
        airtime_ms_with_preamble(
            (1 + data_len) as u32,
            bandwidth_hz,
            sf,
            cr,
            preamble_symbols,
        )
    }
}

/// On-air cost, in milliseconds, charged to the regulatory airtime ledger
/// for one keyed LoRa frame.
///
/// This is what the interface records via
/// [`AirtimeTracker::add_airtime`] after every successful `transmit()`: the
/// full on-air time of the frame at the live modulation, programmed preamble
/// included, mirroring the RNode firmware's `add_airtime(written)`
/// (RNode_Firmware.ino:654), whose symbol count adds `lora_preamble_symbols`.
pub fn frame_airtime_cost_ms(
    frame_len: u32,
    bandwidth_hz: u32,
    sf: u8,
    cr: u8,
    preamble_symbols: u16,
) -> u64 {
    airtime_ms_with_preamble(frame_len, bandwidth_hz, sf, cr, preamble_symbols)
}

/// Whether a bursting LoRa transmitter must yield the channel (open its
/// post-TX ack window) instead of draining the next queued frame.
///
/// Receivers REQ/ACK per transfer window, not per part, so a full ack window
/// after every part only adds dead air between parts of the same requested
/// window. The burst is bounded by a frame count (anti-livelock ceiling) and
/// an airtime budget (fairness: the peer gets a clear window before its
/// receiver-side timeout), and always ends when the queue is empty. Pure
/// decision on counters only, byte-agnostic, so the firmware loop can call it
/// and the bounds are host-testable.
pub fn burst_should_yield(
    queue_empty: bool,
    frames_since_yield: u32,
    airtime_since_yield_ms: u64,
    max_frames: u32,
    max_airtime_ms: u64,
) -> bool {
    queue_empty || frames_since_yield >= max_frames || airtime_since_yield_ms >= max_airtime_ms
}

/// Rolling airtime histogram + duty-cycle lock, mirroring the RNode firmware.
///
/// Holds a fixed `[u16; AIRTIME_BINS]` bin array (960 bytes, no heap), the
/// short/long-term limit fractions, and the derived lock state. The interface
/// records every keyed frame with [`add_airtime`](Self::add_airtime), calls
/// [`update`](Self::update) before draining its TX queue, and holds any queued
/// frame while [`is_locked`](Self::is_locked) is true, exactly as
/// `RNode_Firmware.ino:1624` gates `tx_queue_handler` on `!airtime_lock`.
#[derive(Clone)]
pub struct AirtimeTracker {
    bins: [u16; AIRTIME_BINS],
    st_limit: f32,
    lt_limit: f32,
    short_term: f32,
    long_term: f32,
    locked: bool,
}

impl Default for AirtimeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl AirtimeTracker {
    /// A tracker with an empty histogram and both limits unset (unlimited).
    pub const fn new() -> Self {
        Self {
            bins: [0; AIRTIME_BINS],
            st_limit: 0.0,
            lt_limit: 0.0,
            short_term: 0.0,
            long_term: 0.0,
            locked: false,
        }
    }

    /// Set the short-term limit from a `CMD_ST_ALOCK` u16 (see
    /// [`alock_u16_to_fraction`]). `0` disables the short-term lock.
    pub fn set_st_limit_u16(&mut self, at: u16) {
        self.st_limit = alock_u16_to_fraction(at);
    }

    /// Set the long-term limit from a `CMD_LT_ALOCK` u16 (see
    /// [`alock_u16_to_fraction`]). `0` disables the long-term lock.
    pub fn set_lt_limit_u16(&mut self, at: u16) {
        self.lt_limit = alock_u16_to_fraction(at);
    }

    /// Current histogram bin for a monotonic `now_ms`
    /// (Config.h:194 `current_airtime_bin`).
    fn current_bin(now_ms: u64) -> usize {
        ((now_ms % AIRTIME_LONGTERM_MS) / AIRTIME_BINLEN_MS) as usize
    }

    /// Record `cost_ms` of on-air time for a frame keyed at `now_ms`
    /// (RNode_Firmware.ino:685-688). Adds to the current bin and clears the
    /// next bin so it starts clean when time rolls into it.
    pub fn add_airtime(&mut self, now_ms: u64, cost_ms: u64) {
        let cb = Self::current_bin(now_ms);
        let nb = (cb + 1) % AIRTIME_BINS;
        let add = cost_ms.min(u16::MAX as u64) as u16;
        self.bins[cb] = self.bins[cb].saturating_add(add);
        self.bins[nb] = 0;
    }

    /// Recompute short-term airtime, long-term airtime, and the lock state for a
    /// monotonic `now_ms`. Mirrors the firmware's `update_airtime`
    /// (RNode_Firmware.ino:693-702) and the `airtime_lock` evaluation in its
    /// main loop (RNode_Firmware.ino:1673-1696).
    ///
    /// Short-term airtime is the fraction of channel time used across the
    /// current and previous bins; long-term airtime is total keyed airtime over
    /// the whole 1-hour window. The lock engages when either fraction meets or
    /// exceeds its (non-zero) limit.
    pub fn update(&mut self, now_ms: u64) {
        let cb = Self::current_bin(now_ms);
        let pb = (cb + AIRTIME_BINS - 1) % AIRTIME_BINS;
        let nb = (cb + 1) % AIRTIME_BINS;
        self.bins[nb] = 0;

        self.short_term =
            (self.bins[cb] as f32 + self.bins[pb] as f32) / (2.0 * AIRTIME_BINLEN_MS as f32);

        let mut sum: u32 = 0;
        for &b in self.bins.iter() {
            sum += b as u32;
        }
        self.long_term = sum as f32 / AIRTIME_LONGTERM_MS as f32;

        self.locked = false;
        if self.st_limit != 0.0 && self.short_term >= self.st_limit {
            self.locked = true;
        }
        if self.lt_limit != 0.0 && self.long_term >= self.lt_limit {
            self.locked = true;
        }
    }

    /// Whether the airtime lock currently holds TX (last [`update`](Self::update)).
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Short-term airtime fraction from the last [`update`](Self::update).
    pub fn short_term_airtime(&self) -> f32 {
        self.short_term
    }

    /// Long-term airtime fraction from the last [`update`](Self::update).
    pub fn long_term_airtime(&self) -> f32 {
        self.long_term
    }

    /// Current short-term limit fraction (0.0 = unlimited).
    pub fn st_limit(&self) -> f32 {
        self.st_limit
    }

    /// Current long-term limit fraction (0.0 = unlimited).
    pub fn lt_limit(&self) -> f32 {
        self.lt_limit
    }
}

// ---------------------------------------------------------------------------
// LoRa frame header: split protocol
// ---------------------------------------------------------------------------

/// Split flag in the 1-byte LoRa frame header. When set, the frame is
/// part of a split packet (payload > 254 bytes sent as two frames).
pub const FLAG_SPLIT: u8 = 0x01;

/// Maximum payload bytes in a single LoRa frame (255-byte FIFO minus 1-byte header).
pub const MAX_SINGLE_PAYLOAD: usize = 254;

/// Build one or two LoRa frames from a Reticulum packet.
///
/// `seq_nibble` must already be masked to upper 4 bits (0xF0).
/// Returns 1 frame for payload <= 254 bytes, 2 frames for larger.
/// Both split frames use the identical header byte (same sequence nibble,
/// same FLAG_SPLIT bit), matching the RNode firmware exactly.
pub fn build_lora_frames(data: &[u8], seq_nibble: u8) -> Vec<Vec<u8>> {
    let seq = seq_nibble & 0xF0;
    if data.len() > MAX_SINGLE_PAYLOAD {
        let header = seq | FLAG_SPLIT;
        let mut frame1 = Vec::with_capacity(1 + MAX_SINGLE_PAYLOAD);
        frame1.push(header);
        frame1.extend_from_slice(&data[..MAX_SINGLE_PAYLOAD]);

        let mut frame2 = Vec::with_capacity(1 + data.len() - MAX_SINGLE_PAYLOAD);
        frame2.push(header);
        frame2.extend_from_slice(&data[MAX_SINGLE_PAYLOAD..]);

        alloc::vec![frame1, frame2]
    } else {
        let mut frame = Vec::with_capacity(1 + data.len());
        frame.push(seq);
        frame.extend_from_slice(data);
        alloc::vec![frame]
    }
}

/// State machine for reassembling split LoRa frames.
///
/// Implements the four-case logic from the RNode firmware:
/// 1. Split + no buffer → store first half, return None
/// 2. Split + same sequence → concatenate, return assembled payload
/// 3. Split + different sequence → discard old, store new first half
/// 4. Not split → return payload directly (discard any pending buffer)
pub struct SplitReassembler {
    buf: Vec<u8>,
    seq: Option<u8>,
    tick: u32,
}

impl Default for SplitReassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl SplitReassembler {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            seq: None,
            tick: 0,
        }
    }

    /// Feed a raw LoRa frame (including the 1-byte header).
    /// Returns `Some(payload)` when a complete packet is ready.
    /// Returns `None` when buffering a split first-half or on errors.
    pub fn feed(&mut self, frame: &[u8], current_tick: u32) -> Option<Vec<u8>> {
        if frame.len() < 2 {
            return None;
        }
        let header = frame[0];
        let is_split = (header & FLAG_SPLIT) != 0;
        let sequence = header >> 4;
        let payload = &frame[1..];

        if is_split && self.seq.is_none() {
            // First part of a split packet
            self.buf.clear();
            self.buf.extend_from_slice(payload);
            self.seq = Some(sequence);
            self.tick = current_tick;
            None
        } else if is_split && self.seq == Some(sequence) {
            // Second part: concatenate and deliver
            self.buf.extend_from_slice(payload);
            self.seq = None;
            Some(core::mem::take(&mut self.buf))
        } else if is_split {
            // Different sequence: discard old, start new
            self.buf.clear();
            self.buf.extend_from_slice(payload);
            self.seq = Some(sequence);
            self.tick = current_tick;
            None
        } else {
            // Not a split packet: deliver directly
            if self.seq.is_some() {
                self.buf.clear();
                self.seq = None;
            }
            Some(payload.to_vec())
        }
    }

    /// Discard stale split buffers older than `max_age` ticks.
    pub fn check_timeout(&mut self, current_tick: u32, max_age: u32) {
        if self.seq.is_some() && current_tick.wrapping_sub(self.tick) >= max_age {
            self.buf.clear();
            self.seq = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::kiss::{KissDeframeResult, KissDeframer};
    use alloc::vec;

    /// An empty outgoing queue must always yield (open the post-TX ack
    /// window), regardless of the burst counters.
    #[test]
    fn burst_yields_on_empty_queue_regardless_of_counters() {
        assert!(burst_should_yield(true, 0, 0, 16, 8000));
        assert!(burst_should_yield(true, 1, 100, 16, 8000));
    }

    /// The frame bound forces a yield even with a non-empty queue and low
    /// airtime (anti-livelock ceiling).
    #[test]
    fn burst_yields_when_frame_bound_reached() {
        assert!(burst_should_yield(false, 16, 100, 16, 8000));
        assert!(burst_should_yield(false, 17, 0, 16, 8000));
    }

    /// The airtime bound forces a yield even with a non-empty queue and few
    /// frames (fairness knob).
    #[test]
    fn burst_yields_when_airtime_bound_reached() {
        assert!(burst_should_yield(false, 2, 8000, 16, 8000));
        assert!(burst_should_yield(false, 0, 9500, 16, 8000));
    }

    /// Mid-burst, below every bound, with more queued: keep bursting (this is
    /// the case that kills the per-part over-yield).
    #[test]
    fn burst_does_not_yield_mid_burst_below_bounds() {
        assert!(!burst_should_yield(false, 1, 2700, 16, 8000));
        assert!(!burst_should_yield(false, 15, 7999, 16, 8000));
    }

    /// Threshold boundaries: `>=` on both bounds, one below stays in burst.
    #[test]
    fn burst_yield_bounds_are_inclusive() {
        assert!(!burst_should_yield(false, 15, 0, 16, 8000));
        assert!(burst_should_yield(false, 16, 0, 16, 8000));
        assert!(!burst_should_yield(false, 0, 7999, 16, 8000));
        assert!(burst_should_yield(false, 0, 8000, 16, 8000));
    }

    /// Deframe a single KISS frame and assert it matches the expected command and payload.
    fn assert_single_frame(frame: &[u8], expected_cmd: u8, expected_payload: &[u8]) {
        let mut deframer = KissDeframer::with_max_payload(HW_MTU);
        let results = deframer.process(frame);
        assert_eq!(results.len(), 1, "expected exactly 1 frame");
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, expected_cmd);
                assert_eq!(payload.as_slice(), expected_payload);
            }
            other => panic!("expected Frame, got {other:?}"),
        }
    }

    // --- Encoding tests ---

    #[test]
    fn test_build_detect_query() {
        let query = build_detect_query();
        assert_eq!(
            query,
            vec![0xC0, 0x08, 0x73, 0xC0, 0x50, 0x00, 0xC0, 0x48, 0x00, 0xC0, 0x49, 0x00, 0xC0]
        );
        assert_eq!(query.len(), 13);
    }

    #[test]
    fn test_build_set_frequency() {
        // 868.0 MHz = 868_000_000 Hz = 0x33BCA100
        assert_single_frame(
            &build_set_frequency(868_000_000),
            CMD_FREQUENCY,
            &[0x33, 0xBC, 0xA1, 0x00],
        );
    }

    #[test]
    fn test_build_set_bandwidth() {
        // 125 kHz = 125_000 Hz = 0x0001E848
        assert_single_frame(
            &build_set_bandwidth(125_000),
            CMD_BANDWIDTH,
            &[0x00, 0x01, 0xE8, 0x48],
        );
    }

    #[test]
    fn test_build_set_txpower() {
        assert_single_frame(&build_set_txpower(17), CMD_TXPOWER, &[17]);
    }

    #[test]
    fn test_build_set_sf() {
        assert_single_frame(&build_set_sf(7), CMD_SF, &[7]);
    }

    #[test]
    fn test_build_set_cr() {
        assert_single_frame(&build_set_cr(5), CMD_CR, &[5]);
    }

    #[test]
    fn test_build_set_radio_state_on() {
        assert_single_frame(
            &build_set_radio_state(RADIO_STATE_ON),
            CMD_RADIO_STATE,
            &[0x01],
        );
    }

    #[test]
    fn test_build_set_st_alock() {
        // 50.0% -> 5000
        assert_single_frame(&build_set_st_alock(5000), CMD_ST_ALOCK, &[0x13, 0x88]);
    }

    #[test]
    fn test_build_set_lt_alock() {
        assert_single_frame(&build_set_lt_alock(1000), CMD_LT_ALOCK, &[0x03, 0xE8]);
    }

    #[test]
    fn test_etsi_eu868_duty_cycle() {
        // P sub-band 869.4-869.65 MHz -> 10% (the 869.525 MHz calling channel).
        assert_eq!(etsi_eu868_duty_cycle(869_525_000), Some(0.10));
        // M sub-band 868.0-868.6 MHz -> 1%.
        assert_eq!(etsi_eu868_duty_cycle(868_100_000), Some(0.01));
        // K sub-band 863.0-865.0 MHz -> 0.1%.
        assert_eq!(etsi_eu868_duty_cycle(864_000_000), Some(0.001));
        // N sub-band 868.7-869.2 MHz -> 0.1%.
        assert_eq!(etsi_eu868_duty_cycle(869_000_000), Some(0.001));
        // L / Q sub-bands -> 1%.
        assert_eq!(etsi_eu868_duty_cycle(867_000_000), Some(0.01));
        assert_eq!(etsi_eu868_duty_cycle(869_800_000), Some(0.01));
        // Guard gaps carry no auto cap.
        assert_eq!(etsi_eu868_duty_cycle(868_650_000), None); // M/N gap
        assert_eq!(etsi_eu868_duty_cycle(869_300_000), None); // N/P gap
        assert_eq!(etsi_eu868_duty_cycle(869_680_000), None); // P/Q gap

        // US 915 MHz and other out-of-band frequencies -> no auto cap.
        assert_eq!(etsi_eu868_duty_cycle(915_000_000), None);
        assert_eq!(etsi_eu868_duty_cycle(862_000_000), None);
        assert_eq!(etsi_eu868_duty_cycle(870_500_000), None);
        // The P cap maps to the exact firmware lt_alock (fraction * 10000).
        let p = etsi_eu868_duty_cycle(869_525_000).unwrap();
        assert_eq!((p * 10000.0) as u16, 1000);
    }

    #[test]
    fn test_build_data_frame() {
        assert_single_frame(&build_data_frame(b"Hello RNode"), CMD_DATA, b"Hello RNode");
    }

    #[test]
    fn test_build_leave() {
        assert_single_frame(&build_leave(), CMD_LEAVE, &[LEAVE_PAYLOAD]);
    }

    // --- Decoding tests ---

    #[test]
    fn test_decode_rssi() {
        // raw 157 -> 157 - 157 = 0 dBm
        assert_eq!(decode_rssi(&[157]), Some(0));
        // raw 0 -> 0 - 157 = -157 dBm
        assert_eq!(decode_rssi(&[0]), Some(-157));
        // raw 200 -> 200 - 157 = 43 dBm
        assert_eq!(decode_rssi(&[200]), Some(43));
        // empty
        assert_eq!(decode_rssi(&[]), None);
    }

    #[test]
    fn test_decode_snr() {
        // raw 0x80 = 128 unsigned, but as i8 = -128
        assert_eq!(decode_snr(&[0x80]), Some(-128_i8));
        // raw 0x00 = 0
        assert_eq!(decode_snr(&[0x00]), Some(0_i8));
        // raw 0x7F = 127
        assert_eq!(decode_snr(&[0x7F]), Some(127_i8));
        // empty
        assert_eq!(decode_snr(&[]), None);
    }

    #[test]
    fn test_decode_battery() {
        // Charging, 85%
        assert_eq!(
            decode_battery(&[0x02, 85]),
            Some((BatteryState::Charging, 85))
        );
        // Discharging, 50%
        assert_eq!(
            decode_battery(&[0x01, 50]),
            Some((BatteryState::Discharging, 50))
        );
        // Charged, 100%
        assert_eq!(
            decode_battery(&[0x03, 100]),
            Some((BatteryState::Charged, 100))
        );
        // Unknown state byte
        assert_eq!(decode_battery(&[0x00, 0]), Some((BatteryState::Unknown, 0)));
        // Too short
        assert_eq!(decode_battery(&[0x02]), None);
        assert_eq!(decode_battery(&[]), None);
    }

    #[test]
    fn test_battery_state_as_str() {
        // Matches Python RNodeInterface.get_battery_state_string() (Codeberg #25).
        assert_eq!(BatteryState::Unknown.as_str(), "unknown");
        assert_eq!(BatteryState::Discharging.as_str(), "discharging");
        assert_eq!(BatteryState::Charging.as_str(), "charging");
        assert_eq!(BatteryState::Charged.as_str(), "charged");
        assert_eq!(BatteryState::default(), BatteryState::Unknown);
    }

    #[test]
    fn test_decode_temperature() {
        // raw 150 -> 150 - 120 = 30 Celsius
        assert_eq!(decode_temperature(&[150]), Some(30));
        // raw 120 -> 120 - 120 = 0 Celsius
        assert_eq!(decode_temperature(&[120]), Some(0));
        // raw 90 -> 90 - 120 = -30 Celsius
        assert_eq!(decode_temperature(&[90]), Some(-30));
        // empty
        assert_eq!(decode_temperature(&[]), None);
    }

    #[test]
    fn test_decode_firmware_version() {
        assert_eq!(decode_firmware_version(&[1, 85]), Some((1, 85)));
        assert_eq!(decode_firmware_version(&[2, 0]), Some((2, 0)));
        // Too short
        assert_eq!(decode_firmware_version(&[1]), None);
        assert_eq!(decode_firmware_version(&[]), None);
    }

    #[test]
    fn test_decode_channel_stats_single() {
        // 11-byte single-interface payload
        let payload = [
            0x00, 0x64, // airtime_short = 100
            0x01, 0xF4, // airtime_long = 500
            0x00, 0xC8, // channel_load_short = 200
            0x02, 0x58, // channel_load_long = 600
            200,  // current_rssi raw -> 200 - 157 = 43
            100,  // noise_floor raw -> 100 - 157 = -57
            50,   // interference raw -> 50 - 157 = -107
        ];
        let stats = decode_channel_stats(&payload).unwrap();
        assert_eq!(stats.airtime_short, 100);
        assert_eq!(stats.airtime_long, 500);
        assert_eq!(stats.channel_load_short, 200);
        assert_eq!(stats.channel_load_long, 600);
        assert_eq!(stats.current_rssi, Some(43));
        assert_eq!(stats.noise_floor, Some(-57));
        assert_eq!(stats.interference, Some(-107));
    }

    #[test]
    fn test_decode_channel_stats_multi() {
        // 8-byte multi-interface payload (no RSSI fields)
        let payload = [
            0x00, 0x64, // airtime_short = 100
            0x01, 0xF4, // airtime_long = 500
            0x00, 0xC8, // channel_load_short = 200
            0x02, 0x58, // channel_load_long = 600
        ];
        let stats = decode_channel_stats(&payload).unwrap();
        assert_eq!(stats.airtime_short, 100);
        assert_eq!(stats.airtime_long, 500);
        assert_eq!(stats.channel_load_short, 200);
        assert_eq!(stats.channel_load_long, 600);
        assert_eq!(stats.current_rssi, None);
        assert_eq!(stats.noise_floor, None);
        assert_eq!(stats.interference, None);
    }

    #[test]
    fn test_decode_channel_stats_interference_ff() {
        // 11-byte payload with 0xFF interference -> None
        let payload = [
            0x00, 0x64, // airtime_short = 100
            0x01, 0xF4, // airtime_long = 500
            0x00, 0xC8, // channel_load_short = 200
            0x02, 0x58, // channel_load_long = 600
            200,  // current_rssi
            100,  // noise_floor
            0xFF, // interference = 0xFF -> None
        ];
        let stats = decode_channel_stats(&payload).unwrap();
        assert_eq!(stats.current_rssi, Some(43));
        assert_eq!(stats.noise_floor, Some(-57));
        assert_eq!(stats.interference, None);
    }

    #[test]
    fn test_decode_phy_params_single() {
        // 12-byte single-interface payload
        let payload = [
            0x00, 0x0A, // symbol_time_raw = 10
            0x03, 0xE8, // symbol_rate = 1000
            0x00, 0x08, // preamble_symbols = 8
            0x00, 0x50, // preamble_time_ms = 80
            0x00, 0x0F, // csma_slot_time_ms = 15
            0x00, 0x1E, // difs_time_ms = 30
        ];
        let params = decode_phy_params(&payload).unwrap();
        assert_eq!(params.symbol_time_raw, 10);
        assert_eq!(params.symbol_rate, 1000);
        assert_eq!(params.preamble_symbols, 8);
        assert_eq!(params.preamble_time_ms, 80);
        assert_eq!(params.csma_slot_time_ms, 15);
        assert_eq!(params.difs_time_ms, Some(30));
    }

    #[test]
    fn test_decode_phy_params_multi() {
        // 10-byte multi-interface payload (no difs_time)
        let payload = [
            0x00, 0x0A, // symbol_time_raw = 10
            0x03, 0xE8, // symbol_rate = 1000
            0x00, 0x08, // preamble_symbols = 8
            0x00, 0x50, // preamble_time_ms = 80
            0x00, 0x0F, // csma_slot_time_ms = 15
        ];
        let params = decode_phy_params(&payload).unwrap();
        assert_eq!(params.symbol_time_raw, 10);
        assert_eq!(params.symbol_rate, 1000);
        assert_eq!(params.preamble_symbols, 8);
        assert_eq!(params.preamble_time_ms, 80);
        assert_eq!(params.csma_slot_time_ms, 15);
        assert_eq!(params.difs_time_ms, None);
    }

    #[test]
    fn test_decode_too_short() {
        // All decode functions return None for empty/short payloads
        assert_eq!(decode_rssi(&[]), None);
        assert_eq!(decode_snr(&[]), None);
        assert_eq!(decode_battery(&[]), None);
        assert_eq!(decode_battery(&[0x01]), None);
        assert_eq!(decode_temperature(&[]), None);
        assert_eq!(decode_firmware_version(&[]), None);
        assert_eq!(decode_firmware_version(&[1]), None);
        assert_eq!(decode_channel_stats(&[]), None);
        assert_eq!(decode_channel_stats(&[0; 7]), None);
        assert_eq!(decode_phy_params(&[]), None);
        assert_eq!(decode_phy_params(&[0; 9]), None);
    }

    // --- Round-trip tests ---

    #[test]
    fn test_roundtrip_frequency() {
        let frame = build_set_frequency(868_000_000);
        let mut deframer = KissDeframer::with_max_payload(HW_MTU);
        let results = deframer.process(&frame);
        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, CMD_FREQUENCY);
                let hz = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                assert_eq!(hz, 868_000_000);
            }
            _ => panic!("Expected frame"),
        }
    }

    #[test]
    fn test_roundtrip_detect_query() {
        let query = build_detect_query();
        let mut deframer = KissDeframer::with_max_payload(HW_MTU);
        let results = deframer.process(&query);

        assert_eq!(results.len(), 4);

        // Frame 1: CMD_DETECT with DETECT_REQ
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, CMD_DETECT);
                assert_eq!(payload.as_slice(), &[DETECT_REQ]);
            }
            _ => panic!("Expected detect frame"),
        }

        // Frame 2: CMD_FW_VERSION query
        match &results[1] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, CMD_FW_VERSION);
                assert_eq!(payload.as_slice(), &[0x00]);
            }
            _ => panic!("Expected fw_version frame"),
        }

        // Frame 3: CMD_PLATFORM query
        match &results[2] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, CMD_PLATFORM);
                assert_eq!(payload.as_slice(), &[0x00]);
            }
            _ => panic!("Expected platform frame"),
        }

        // Frame 4: CMD_MCU query
        match &results[3] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, CMD_MCU);
                assert_eq!(payload.as_slice(), &[0x00]);
            }
            _ => panic!("Expected mcu frame"),
        }
    }

    #[test]
    fn test_roundtrip_data_with_special_bytes() {
        // Data containing FEND and FESC bytes, must survive escaping
        let data = [0xC0, 0xDB, 0x42, 0xC0, 0xDB];
        let frame = build_data_frame(&data);
        let mut deframer = KissDeframer::with_max_payload(HW_MTU);
        let results = deframer.process(&frame);
        assert_eq!(results.len(), 1);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, CMD_DATA);
                assert_eq!(payload.as_slice(), &data);
            }
            _ => panic!("Expected frame"),
        }
    }

    #[test]
    fn test_cmd_int_data_values() {
        assert_eq!(CMD_INT_DATA[0], 0x00);
        assert_eq!(CMD_INT_DATA[1], 0x10);
        assert_eq!(CMD_INT_DATA[2], 0x20);
        assert_eq!(CMD_INT_DATA[3], 0x70);
        assert_eq!(CMD_INT_DATA[4], 0x75);
        assert_eq!(CMD_INT_DATA[5], 0x90);
        assert_eq!(CMD_INT_DATA[6], 0xA0);
        assert_eq!(CMD_INT_DATA[7], 0xB0);
        assert_eq!(CMD_INT_DATA[8], 0xC0);
        assert_eq!(CMD_INT_DATA[9], 0xD0);
        assert_eq!(CMD_INT_DATA[10], 0xE0);
        assert_eq!(CMD_INT_DATA[11], 0xF0);
    }

    // --- Multi-interface (vport) framing tests ---

    #[test]
    fn test_build_select_interface() {
        // FEND CMD_SEL_INT index FEND, index emitted raw (not escaped)
        assert_eq!(build_select_interface(0), vec![0xC0, 0x1F, 0x00, 0xC0]);
        assert_eq!(build_select_interface(1), vec![0xC0, 0x1F, 0x01, 0xC0]);
        assert_eq!(build_select_interface(10), vec![0xC0, 0x1F, 0x0A, 0xC0]);
    }

    #[test]
    fn test_build_vport_command_is_select_plus_frame() {
        // The vport wrapper is exactly select(index) ++ command_frame.
        let inner = build_set_sf(5);
        let composed = build_vport_command(3, &inner);
        let mut expected = build_select_interface(3);
        expected.extend_from_slice(&inner);
        assert_eq!(composed, expected);
        // And byte-for-byte vs Python RNodeMultiInterface.setSpreadingFactor:
        // FEND SEL_INT 03 FEND  FEND CMD_SF 05 FEND
        assert_eq!(
            composed,
            vec![0xC0, 0x1F, 0x03, 0xC0, 0xC0, 0x04, 0x05, 0xC0]
        );
    }

    #[test]
    fn test_vport_frequency_kat() {
        // Python RNodeMultiInterface.setFrequency(2_400_000_000, vport 1):
        //   FEND SEL_INT 01 FEND  FEND CMD_FREQUENCY <4B BE> FEND
        // 2_400_000_000 = 0x8F0D1800
        let composed = build_vport_command(1, &build_set_frequency(2_400_000_000));
        assert_eq!(
            composed,
            vec![0xC0, 0x1F, 0x01, 0xC0, 0xC0, 0x01, 0x8F, 0x0D, 0x18, 0x00, 0xC0]
        );
    }

    #[test]
    fn test_vport_data_frame_kat() {
        // select(2) ++ CMD_DATA "Hi"
        assert_eq!(
            build_vport_data_frame(2, b"Hi"),
            vec![0xC0, 0x1F, 0x02, 0xC0, 0xC0, 0x00, 0x48, 0x69, 0xC0]
        );
    }

    #[test]
    fn test_vport_command_escapes_inner_payload() {
        // The vport-select prefix is raw, but the inner command frame is still
        // KISS-escaped. Frequency 0xC0DB0000 has bytes C0 and DB that must be
        // escaped inside the CMD_FREQUENCY frame (C0->DB DC, DB->DB DD).
        let composed = build_vport_command(0, &build_set_frequency(0xC0DB_0000));
        assert_eq!(
            composed,
            vec![
                0xC0, 0x1F, 0x00, 0xC0, // select(0), raw
                0xC0, 0x01, 0xDB, 0xDC, 0xDB, 0xDD, 0x00, 0x00, 0xC0, // CMD_FREQUENCY escaped
            ]
        );
    }

    #[test]
    fn test_vport_command_roundtrips_through_deframer() {
        // A vport data frame deframes into a CMD_SEL_INT frame (payload = vport)
        // followed by a CMD_DATA frame (payload = data), which is exactly how the
        // std RX router recovers the vport tag.
        let frame = build_vport_data_frame(4, b"payload");
        let mut deframer = KissDeframer::with_max_payload(HW_MTU);
        let results = deframer.process(&frame);
        assert_eq!(results.len(), 2);
        match &results[0] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, CMD_SEL_INT);
                assert_eq!(decode_select_interface(payload), Some(4));
            }
            other => panic!("expected SEL_INT frame, got {other:?}"),
        }
        match &results[1] {
            KissDeframeResult::Frame { command, payload } => {
                assert_eq!(*command, CMD_DATA);
                assert_eq!(payload.as_slice(), b"payload");
            }
            other => panic!("expected DATA frame, got {other:?}"),
        }
    }

    #[test]
    fn test_build_detect_query_multi() {
        // Same as single detect but with a trailing CMD_INTERFACES query.
        assert_eq!(
            build_detect_query_multi(),
            vec![
                0xC0, 0x08, 0x73, 0xC0, 0x50, 0x00, 0xC0, 0x48, 0x00, 0xC0, 0x49, 0x00, 0xC0, 0x71,
                0x00, 0xC0
            ]
        );
    }

    #[test]
    fn test_decode_interfaces() {
        // Two vports: vport0 = SX127X (0x00), vport1 = SX128X (0x20).
        // Records are 2 bytes; second byte is the chip type.
        assert_eq!(
            decode_interfaces(&[0x00, 0x00, 0x00, 0x20]),
            vec![0x00, 0x20]
        );
        // Empty report -> no vports.
        assert_eq!(decode_interfaces(&[]), Vec::<u8>::new());
        // Trailing odd byte is ignored (malformed frame tolerance).
        assert_eq!(decode_interfaces(&[0x00, 0x11, 0x00]), vec![0x11]);
    }

    #[test]
    fn test_interface_chip_name() {
        assert_eq!(interface_chip_name(CHIP_SX127X), "SX127X");
        assert_eq!(interface_chip_name(CHIP_SX1278), "SX127X");
        assert_eq!(interface_chip_name(CHIP_SX126X), "SX126X");
        assert_eq!(interface_chip_name(CHIP_SX1262), "SX126X");
        assert_eq!(interface_chip_name(CHIP_SX128X), "SX128X");
        assert_eq!(interface_chip_name(CHIP_SX1280), "SX128X");
        // Unknown -> conservative SX127X fallback (matches Python).
        assert_eq!(interface_chip_name(0xEE), "SX127X");
    }

    // --- Validation tests ---

    #[test]
    fn test_validate_firmware_ok() {
        assert!(validate_firmware(1, 52));
        assert!(validate_firmware(1, 85));
        assert!(validate_firmware(2, 0));
    }

    #[test]
    fn test_validate_firmware_too_old() {
        assert!(!validate_firmware(1, 51));
        assert!(!validate_firmware(0, 99));
        assert!(!validate_firmware(1, 0));
    }

    #[test]
    fn test_validate_config_ok() {
        assert!(validate_config(868_000_000, 125_000, 17, 7, 5).is_ok());
        assert!(validate_config(915_000_000, 500_000, 22, 12, 8).is_ok());
        assert!(validate_config(2_400_000_000, 250_000, 0, 5, 5).is_ok());
    }

    #[test]
    fn test_validate_config_bad_frequency() {
        assert_eq!(
            validate_config(100_000_000, 125_000, 17, 7, 5),
            Err(ConfigError::FrequencyOutOfRange)
        );
        // u32 max is ~4.29 GHz, over the 3 GHz limit
        assert_eq!(
            validate_config(3_500_000_000, 125_000, 17, 7, 5),
            Err(ConfigError::FrequencyOutOfRange)
        );
    }

    #[test]
    fn test_validate_config_bad_bandwidth() {
        assert_eq!(
            validate_config(868_000_000, 100_000, 17, 7, 5),
            Err(ConfigError::InvalidBandwidth)
        );
    }

    #[test]
    fn test_validate_config_bad_txpower() {
        assert_eq!(
            validate_config(868_000_000, 125_000, 38, 7, 5),
            Err(ConfigError::TxPowerOutOfRange)
        );
    }

    /// The defect this default exists to close: a config with no `txpower`
    /// line used to resolve to 0 dBm — 1 mW — which a node has no symptom
    /// for. Absent means "as far as this board reaches" — where the band
    /// permits it. 869.525 MHz sits in the 500 mW sub-band h1.7, whose
    /// 27 dBm limit is above the board maximum, so the board maximum wins.
    #[test]
    fn an_absent_txpower_resolves_to_the_board_maximum() {
        assert_eq!(resolve_tx_power(None, 869_525_000), DEFAULT_TX_POWER_DBM);
        assert_eq!(resolve_tx_power(None, 869_525_000), 22);
    }

    /// The hole the frequency cap closes: a community frequency in a 25 mW
    /// sub-band (867.2 MHz, h1.4 — Rotterdam/Duffel sit there) with no
    /// configured `txpower` must not transmit 22 dBm. The default is capped
    /// to the lawful 14 dBm.
    #[test]
    fn an_absent_txpower_is_capped_by_the_erp_limit_of_the_band() {
        assert_eq!(resolve_tx_power(None, 867_200_000), 14);
    }

    /// A frequency the table cannot cite derives no cap: the board maximum
    /// is used (and the resolution warns). No limit is invented for a band
    /// without a citable entry — same policy as the airtime lock.
    #[test]
    fn an_absent_txpower_on_an_unlisted_frequency_stays_the_board_maximum() {
        assert_eq!(resolve_tx_power(None, 915_000_000), 22);
    }

    /// The distinction the whole `Option` is carried for: an operator who
    /// writes `txpower = 0` means 0, and must not be given the default just
    /// because 0 is also what the absent case used to produce.
    #[test]
    fn an_explicit_zero_txpower_stays_zero() {
        assert_eq!(resolve_tx_power(Some(0), 867_200_000), 0);
        assert_ne!(
            resolve_tx_power(Some(0), 867_200_000),
            resolve_tx_power(None, 867_200_000)
        );
    }

    /// An explicit value wins even above the derived cap: the operator may
    /// know the jurisdiction or hold a licence. 20 dBm on 867.2 MHz is 6 dB
    /// over the 14 dBm h1.4 limit and is honoured (and logged), not clamped.
    #[test]
    fn an_explicit_txpower_above_the_derived_cap_is_honoured() {
        assert_eq!(resolve_tx_power(Some(20), 867_200_000), 20);
    }

    /// The policy this whole area exists to hold down: **no radio
    /// configuration is refused for a radio-regulatory reason — it is warned
    /// about, loudly, and then honoured.** An explicit 20 dBm on 867.2 MHz is
    /// 6 dB over the lawful 14 dBm e.r.p. of h1.4; the value reaches the
    /// caller untouched *and* the operator is told, at WARN so no debug
    /// filter is needed to see it. Value-only assertions do not pin this:
    /// silently honouring the excess would pass them and is the defect.
    #[cfg(feature = "tracing")]
    #[test]
    fn an_explicit_txpower_over_the_erp_limit_is_honoured_and_warned_about() {
        let (resolved, logs) =
            crate::test_log_capture::with_captured_logs(|| resolve_tx_power(Some(20), 867_200_000));
        assert_eq!(resolved, 20, "the operator's value reaches the caller");
        assert!(
            logs.contains("WARN"),
            "the excess is warned about, not whispered at debug; logs={logs:?}"
        );
        assert!(
            logs.contains("honouring the explicit value"),
            "the warning says the value is honoured; logs={logs:?}"
        );
        assert!(
            logs.contains("20") && logs.contains("14") && logs.contains("867200000"),
            "the warning names the value, the limit and the frequency; logs={logs:?}"
        );
    }

    /// The same for a band this tree cannot cite a limit for: 30 dBm on
    /// 915 MHz (US ISM) reaches the caller unchanged. There is no cap to
    /// exceed, so there is nothing to warn about — and in particular no
    /// warning may claim a limit the table does not hold, which would read as
    /// authoritative to exactly the operator who most needs it not to.
    #[cfg(feature = "tracing")]
    #[test]
    fn an_explicit_txpower_on_an_uncitable_band_is_honoured_unchanged() {
        assert_eq!(
            lawful_erp_dbm(915_000_000),
            None,
            "premise: no citable limit"
        );
        let (resolved, logs) =
            crate::test_log_capture::with_captured_logs(|| resolve_tx_power(Some(30), 915_000_000));
        assert_eq!(resolved, 30, "the operator's value reaches the caller");
        assert!(
            !logs.contains("exceeds the derived ERP limit"),
            "no invented limit is cited for an uncitable band; logs={logs:?}"
        );
    }

    /// The control the two above are worthless without: the cap is a
    /// *default*, not a veto. With no `txpower` key at all, 867.2 MHz still
    /// resolves to the lawful 14 dBm rather than the 22 dBm board maximum —
    /// so "no opinion" and "this value" stay distinguishable, and a guard
    /// that simply deleted the ERP table would fail here.
    #[test]
    fn an_absent_txpower_still_resolves_to_the_capped_default() {
        assert_eq!(resolve_tx_power(None, 867_200_000), 14);
        assert_eq!(
            resolve_tx_power(Some(22), 867_200_000),
            22,
            "and the same band explicitly asked for 22 gets 22"
        );
    }

    /// Every other stated value passes through untouched, negatives included
    /// (`lnflash` accepts down to -9 dBm).
    #[test]
    fn an_explicit_txpower_is_returned_verbatim() {
        for stated in [-9i8, 1, 7, 14, 17, 20, 22, 37] {
            assert_eq!(
                resolve_tx_power(Some(stated), 869_525_000),
                stated,
                "txpower {stated}"
            );
        }
    }

    /// 867.2 MHz — sub-band h1.4 (865-868 MHz), 25 mW e.r.p. The band real
    /// communities sit in (Rotterdam/Duffel 867.2, UK 867.5, Bern 868.0 is
    /// h1.4's upper edge into h1.5, Madrid 868.2).
    #[test]
    fn erp_limit_for_867_2_mhz_is_14_dbm() {
        assert_eq!(lawful_erp_dbm(867_200_000), Some(14));
    }

    /// 869.525 MHz — the shipped default, sub-band h1.7 (869.4-869.65 MHz),
    /// the single 500 mW band in the European SRD spectrum.
    #[test]
    fn erp_limit_for_869_525_mhz_is_27_dbm() {
        assert_eq!(lawful_erp_dbm(869_525_000), Some(27));
    }

    /// 869.463 MHz — the coming NL-consensus default — is in h1.7 too.
    #[test]
    fn erp_limit_for_869_463_mhz_is_27_dbm() {
        assert_eq!(lawful_erp_dbm(869_463_000), Some(27));
    }

    /// 433.575 MHz — 433.05-434.79 MHz (sub-band g1), 10 mW e.r.p.
    #[test]
    fn erp_limit_for_433_575_mhz_is_10_dbm() {
        assert_eq!(lawful_erp_dbm(433_575_000), Some(10));
    }

    /// 869.85 MHz — 869.7-870 MHz. Two options exist there: h1.8 (5 mW, no
    /// duty cycle) and h1.9 (25 mW, 1%). A LoRa carrier uses h1.9.
    #[test]
    fn erp_limit_for_869_85_mhz_is_14_dbm() {
        assert_eq!(lawful_erp_dbm(869_850_000), Some(14));
    }

    /// A frequency with no table entry yields `None`, never an invented
    /// number: 915 MHz (US 902-928) has no citable source in this tree.
    #[test]
    fn erp_limit_for_an_unlisted_frequency_is_none() {
        assert_eq!(lawful_erp_dbm(915_000_000), None);
    }

    /// Every remaining table row, pinned so a range typo names its band.
    #[test]
    fn erp_limit_covers_every_listed_sub_band() {
        assert_eq!(lawful_erp_dbm(864_000_000), Some(14)); // h1.3
        assert_eq!(lawful_erp_dbm(868_300_000), Some(14)); // h1.5
        assert_eq!(lawful_erp_dbm(869_000_000), Some(14)); // h1.6
    }

    /// A 125 kHz carrier centred at 868.65 MHz sits wholly inside the
    /// 868.6-868.7 MHz alarm band, where only <= 25 kHz channel spacing is
    /// permitted. That is a named gap, not an unlisted frequency.
    #[test]
    fn a_carrier_centred_in_an_alarm_band_names_the_gap() {
        assert_eq!(erp_band_gap(868_650_000, 125_000), Some("868.6-868.7 MHz"));
    }

    /// A carrier centred in a listed sub-band whose occupied bandwidth leaks
    /// into an adjacent alarm band is refused too: 868.55 MHz +- 62.5 kHz
    /// reaches 868.6125 MHz, inside 868.6-868.7.
    #[test]
    fn a_carrier_leaking_into_an_alarm_band_names_the_gap() {
        assert_eq!(erp_band_gap(868_550_000, 125_000), Some("868.6-868.7 MHz"));
    }

    /// The shipped default stays clear: 869.525 +- 62.5 kHz is wholly inside
    /// h1.7. Edge-exact occupancy is also clear — an upper edge at precisely
    /// 868.6 MHz does not overlap the band that begins there.
    #[test]
    fn a_carrier_inside_a_listed_sub_band_has_no_gap() {
        assert_eq!(erp_band_gap(869_525_000, 125_000), None);
        assert_eq!(erp_band_gap(868_537_500, 125_000), None); // upper edge = 868.6
        assert_eq!(erp_band_gap(915_000_000, 125_000), None); // unlisted != gap
    }

    /// The 433.05-434.79 MHz row of the duty-cycle table: 10%. Before it was
    /// added, a 433 MHz node derived no airtime limit at all.
    #[test]
    fn duty_cycle_for_433_band_is_10_percent() {
        assert_eq!(etsi_eu868_duty_cycle(433_575_000), Some(0.10));
        assert_eq!(firmware_default_lt_alock(433_575_000, None), 1000);
    }

    #[test]
    fn test_validate_config_bad_sf() {
        assert_eq!(
            validate_config(868_000_000, 125_000, 17, 4, 5),
            Err(ConfigError::SpreadingFactorOutOfRange)
        );
        assert_eq!(
            validate_config(868_000_000, 125_000, 17, 13, 5),
            Err(ConfigError::SpreadingFactorOutOfRange)
        );
    }

    #[test]
    fn test_validate_config_bad_cr() {
        assert_eq!(
            validate_config(868_000_000, 125_000, 17, 7, 4),
            Err(ConfigError::CodingRateOutOfRange)
        );
        assert_eq!(
            validate_config(868_000_000, 125_000, 17, 7, 9),
            Err(ConfigError::CodingRateOutOfRange)
        );
    }

    #[test]
    fn test_compute_bitrate() {
        // SF7, CR5, BW125kHz → Python produces 5468
        // Formula: 7 * (4.0/5) / (128 / 125) * 1000 = 7 * 0.8 / 1.024 * 1000 = 5468.75 → 5468
        let br = compute_bitrate(7, 5, 125_000);
        assert_eq!(br, 5468);
    }

    #[test]
    fn test_compute_bitrate_sf12() {
        // SF12, CR8, BW125kHz
        // 12 * (4.0/8) / (4096 / 125) * 1000 = 12 * 0.5 / 32.768 * 1000 = 183.105... → 183
        let br = compute_bitrate(12, 8, 125_000);
        assert_eq!(br, 183);
    }

    #[test]
    fn test_compute_bitrate_62_5khz() {
        // SF7, CR5, BW62.5kHz, the slow bandwidth used by LoRa integration tests
        // 7 * (4.0/5) / (128 / 62.5) * 1000 = 7 * 0.8 / 2.048 * 1000 = 2734.375 → 2734
        let br = compute_bitrate(7, 5, 62_500);
        assert_eq!(br, 2734);
    }

    #[test]
    fn test_compute_bitrate_500khz() {
        // SF7, CR5, BW500kHz, fast LoRa configuration
        // 7 * (4.0/5) / (128 / 500) * 1000 = 7 * 0.8 / 0.256 * 1000 = 21875
        let br = compute_bitrate(7, 5, 500_000);
        assert_eq!(br, 21875);
    }

    // Preamble derivation tests
    //
    // The ten bandwidths the reference offers (`RNode_Firmware/Framing.h`
    // bandwidth table) crossed with the spreading factors and coding rates a
    // modem accepts. Everything below is checked over this grid.
    const REFERENCE_BANDWIDTHS: [u32; 10] = [
        7_800, 10_400, 15_600, 20_800, 31_250, 41_700, 62_500, 125_000, 250_000, 500_000,
    ];

    /// The reference derivation transcribed literally, in the float types the
    /// firmware uses, so the integer implementation has something to be
    /// compared against rather than re-derived from.
    ///
    /// `RNode_Firmware/Utilities.h:1235-1258`.
    fn reference_preamble_symbols_f32(sf: u8, cr: u8, bw: u32) -> u16 {
        let two_pow_sf = 2f64.powi(sf as i32);
        // Utilities.h:1235-1236
        let lora_symbol_rate = (bw as f32) / (two_pow_sf as f32);
        let lora_symbol_time_ms = ((1.0f64 / lora_symbol_rate as f64) * 1000.0) as f32;
        // Utilities.h:1237, truncated to uint32_t
        let lora_bitrate = ((sf as f64)
            * ((4.0f64 / cr as f64) / ((two_pow_sf as f32) as f64 / (bw as f64 / 1000.0)))
            * 1000.0) as u32;
        // Utilities.h:1240, 1245-1247
        let fast_rate = lora_bitrate > 30_000;
        let mut target_ms: f32 = 24.0;
        if fast_rate {
            target_ms -= 18.0;
        }
        // Utilities.h:1254-1256
        let mut target_symbols: f32 = target_ms / lora_symbol_time_ms;
        if target_symbols < 18.0 {
            target_symbols = 18.0;
        } else {
            target_symbols = target_symbols.ceil();
        }
        target_symbols as u16
    }

    /// The whole point of the exercise: over every PHY the reference itself
    /// can be configured for, our integer derivation returns exactly what the
    /// firmware's float derivation returns. This is the assertion behind the
    /// "not bit-identical" caveat in the doc comment — it says where the two
    /// *are* identical, by exhaustion rather than by argument.
    #[test]
    fn derive_preamble_matches_reference_float() {
        for sf in 5..=12u8 {
            for cr in 5..=8u8 {
                for bw in REFERENCE_BANDWIDTHS {
                    assert_eq!(
                        derive_preamble_symbols(sf, cr, bw),
                        reference_preamble_symbols_f32(sf, cr, bw),
                        "sf={sf} cr={cr} bw={bw}"
                    );
                }
            }
        }
    }

    /// The float and integer bitrates agree on the fast-rate side of
    /// `LORA_FAST_THRESHOLD_BPS` everywhere on the grid, which is the one
    /// place a rounding difference could change a preamble by 18 symbols
    /// rather than by one. Checked separately from the derivation so a
    /// failure says which of the two steps drifted.
    #[test]
    fn derive_preamble_fast_rate_decision_matches_reference() {
        for sf in 5..=12u8 {
            for cr in 5..=8u8 {
                for bw in REFERENCE_BANDWIDTHS {
                    let ours = compute_bitrate(sf, cr, bw) > LORA_FAST_THRESHOLD_BPS;
                    let theirs = ((sf as f64)
                        * ((4.0f64 / cr as f64)
                            / ((2f64.powi(sf as i32) as f32) as f64 / (bw as f64 / 1000.0)))
                        * 1000.0) as u32
                        > 30_000;
                    assert_eq!(ours, theirs, "sf={sf} cr={cr} bw={bw}");
                }
            }
        }
    }

    /// Hand-computed values at the bandwidth every LoRa scenario uses, and
    /// the two that matter for the interop defect this function exists to
    /// close: SF7 derives the 24 we used to hardcode (which is why SF7 always
    /// worked), and SF8 and slower derive the floor of 18 (which is why they
    /// did not).
    ///
    /// T_sym at BW125 is 2^SF/125 ms, so target_symbols = 24 * 125 / 2^SF:
    ///   SF5  93.75 -> 94    SF6  46.875 -> 47   SF7  23.4375 -> 24
    ///   SF8  11.72 -> 18    SF9  5.86 -> 18     SF10 2.93 -> 18
    ///   SF11 1.46 -> 18     SF12 0.73 -> 18
    #[test]
    fn derive_preamble_bw125_hand_computed() {
        assert_eq!(derive_preamble_symbols(5, 5, 125_000), 94);
        assert_eq!(derive_preamble_symbols(6, 5, 125_000), 47);
        assert_eq!(derive_preamble_symbols(7, 5, 125_000), 24);
        assert_eq!(derive_preamble_symbols(8, 5, 125_000), 18);
        assert_eq!(derive_preamble_symbols(9, 5, 125_000), 18);
        assert_eq!(derive_preamble_symbols(10, 5, 125_000), 18);
        assert_eq!(derive_preamble_symbols(11, 5, 125_000), 18);
        assert_eq!(derive_preamble_symbols(12, 5, 125_000), 18);
    }

    /// The coding rate reaches the result only through the fast-rate test, so
    /// at BW125 (where no SF is fast) the derivation is CR-independent. The
    /// SF10/CR8 cell the mixed-pair measurement ran on is the third row.
    #[test]
    fn derive_preamble_bw125_ignores_coding_rate() {
        for sf in 5..=12u8 {
            let at_cr5 = derive_preamble_symbols(sf, 5, 125_000);
            for cr in 6..=8u8 {
                assert_eq!(derive_preamble_symbols(sf, cr, 125_000), at_cr5, "sf={sf}");
            }
        }
    }

    /// The fast-rate branch, which only three PHYs on the grid reach:
    /// BW500k/SF5 (all CRs), BW500k/SF6 (CR4/5 and 4/6), BW250k/SF5 (CR4/5).
    /// There the target drops from 24 ms to 6 ms and the symbol count with
    /// it — 375 -> 94 at BW500k/SF5. A derivation that skipped the branch
    /// would program a four-times-too-long preamble on exactly the settings
    /// where airtime is scarcest.
    #[test]
    fn derive_preamble_fast_rate_branch() {
        assert_eq!(derive_preamble_symbols(5, 5, 500_000), 94);
        assert_eq!(derive_preamble_symbols(5, 8, 500_000), 94);
        assert_eq!(derive_preamble_symbols(6, 5, 500_000), 47);
        assert_eq!(derive_preamble_symbols(5, 5, 250_000), 47);
        // Just below the threshold: BW500k/SF6/CR4/7 is 26785 bps, so the
        // target stays 24 ms and the count is four times the fast one.
        assert_eq!(compute_bitrate(6, 7, 500_000), 26_785);
        assert_eq!(derive_preamble_symbols(6, 7, 500_000), 188);
    }

    /// Degenerate inputs return the floor rather than dividing by zero or
    /// wrapping, and no input can return below the floor.
    #[test]
    fn derive_preamble_degenerate_inputs() {
        assert_eq!(derive_preamble_symbols(0, 5, 125_000), 18);
        assert_eq!(derive_preamble_symbols(13, 5, 125_000), 18);
        assert_eq!(derive_preamble_symbols(7, 5, 0), 18);
        assert_eq!(derive_preamble_symbols(7, 0, 125_000), 18);
        // The u16 clamp, reachable only at the lowest SF and a bandwidth in
        // the hundreds of megahertz: 6 ms / (32 / 4294.97 MHz) = 805307.
        assert_eq!(derive_preamble_symbols(5, 5, u32::MAX), u16::MAX);
    }

    // Airtime tests
    #[test]
    fn test_airtime_491b_bw62500_sf7_cr5() {
        // 491-byte resource data segment at 62.5kHz SF7 CR5
        // Semtech calculator: ~1440ms
        let ms = airtime_ms(491, 62_500, 7, 5);
        assert!(
            (1400..=1500).contains(&ms),
            "airtime={ms}ms, expected ~1440ms"
        );
    }

    #[test]
    fn test_airtime_491b_bw125000_sf7_cr5() {
        // Same payload at 125kHz, should be ~half
        let ms = airtime_ms(491, 125_000, 7, 5);
        assert!((700..=800).contains(&ms), "airtime={ms}ms, expected ~750ms");
    }

    #[test]
    fn test_airtime_491b_bw250000_sf7_cr5() {
        // Same payload at 250kHz, should be ~quarter
        let ms = airtime_ms(491, 250_000, 7, 5);
        assert!((350..=420).contains(&ms), "airtime={ms}ms, expected ~380ms");
    }

    #[test]
    fn test_airtime_small_packet() {
        // 20-byte keepalive at 62.5kHz SF7 CR5
        let ms = airtime_ms(20, 62_500, 7, 5);
        assert!(
            ms > 0 && ms < 500,
            "airtime={ms}ms, expected <500ms for small packet"
        );
    }

    #[test]
    fn test_airtime_sf12_long_range() {
        // 100 bytes at SF12 125kHz CR8, very slow long range
        let ms = airtime_ms(100, 125_000, 12, 8);
        assert!(ms >= 2000, "airtime={ms}ms, expected >2000ms for SF12");
    }

    #[test]
    fn airtime_is_total_for_degenerate_inputs() {
        // A zero bandwidth used to divide by zero; a zero or oversized sf used
        // to overflow the `1u64 << sf` shift. Both now return 0 rather than
        // panicking, matching compute_bitrate/derive_preamble_symbols
        // (Codeberg #274).
        assert_eq!(airtime_ms_with_preamble(184, 0, 7, 5, 8), 0);
        assert_eq!(airtime_ms_with_preamble(184, 125_000, 0, 5, 8), 0);
        assert_eq!(airtime_ms_with_preamble(184, 125_000, 64, 5, 8), 0);
        assert_eq!(airtime_ms(184, 0, 7, 5), 0);
    }

    #[test]
    fn test_airtime_ms_is_the_preamble_8_case() {
        assert_eq!(
            airtime_ms(184, 125_000, 12, 8),
            airtime_ms_with_preamble(184, 125_000, 12, 8, 8)
        );
        // The SF12 announce that motivated the parameter: 18 programmed
        // preamble symbols put a 184-byte frame at 10.69 s on the air, 328 ms
        // more than the preamble-8 formula reports.
        assert_eq!(airtime_ms_with_preamble(184, 125_000, 12, 8, 18), 10_691);
    }

    /// The regulatory ledger must be charged what was actually on the air.
    /// Since the preamble fix the radios run SF-derived preambles (18 at
    /// SF12/BW125), so one 184-byte SF12 frame is 10_691 ms on the air; a
    /// preamble-8 charge (10_363 ms) undercounts every frame by 328 ms.
    #[test]
    fn airtime_lock_charges_the_programmed_preamble() {
        let cost = frame_airtime_cost_ms(184, 125_000, 12, 8, 18);
        assert!(
            cost >= 10_691,
            "ledger charge {cost}ms undercounts the on-air frame (preamble-8 \
             accounting); the radio spent 10_691ms on the air"
        );
        assert_eq!(cost, airtime_ms_with_preamble(184, 125_000, 12, 8, 18));
    }

    #[test]
    fn test_compute_spacing_includes_csma_overhead() {
        let air = airtime_ms_with_preamble(491, 62_500, 7, 5, 24);
        let spacing = compute_spacing_ms(491, 62_500, 7, 5, 24);
        assert_eq!(
            spacing,
            air + CSMA_DIFS_MS + CSMA_MAX_CW_MS + PACING_MARGIN_MS,
            "spacing must be airtime + DIFS + max CW + margin"
        );
    }

    /// TX pacing charges the programmed preamble: at SF12/BW125 the derived
    /// 18-symbol preamble adds 328 ms per frame over the preamble-8 formula.
    #[test]
    fn test_compute_spacing_charges_the_programmed_preamble() {
        let pre18 = compute_spacing_ms(184, 125_000, 12, 8, 18);
        let pre8 = compute_spacing_ms(184, 125_000, 12, 8, 8);
        assert!(pre18 - pre8 >= 327, "preamble delta {}ms", pre18 - pre8);
    }

    #[test]
    fn test_compute_spacing_floor() {
        // Tiny packet with huge bandwidth, airtime < MIN_SPACING_MS
        let spacing = compute_spacing_ms(1, 500_000, 7, 5, 8);
        assert!(
            spacing >= MIN_SPACING_MS,
            "spacing must never go below MIN_SPACING_MS"
        );
    }

    // -----------------------------------------------------------------------
    // Split protocol tests
    // -----------------------------------------------------------------------

    // TX tests

    #[test]
    fn single_frame_small() {
        let data = vec![0xAA; 100];
        let frames = build_lora_frames(&data, 0x50);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0][0] & FLAG_SPLIT, 0);
        assert_eq!(&frames[0][1..], &data[..]);
    }

    #[test]
    fn single_frame_exact_254() {
        let data = vec![0xBB; 254];
        let frames = build_lora_frames(&data, 0x30);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0][0] & FLAG_SPLIT, 0);
        assert_eq!(frames[0].len(), 255);
    }

    #[test]
    fn split_at_255() {
        let data = vec![0xCC; 255];
        let frames = build_lora_frames(&data, 0x70);
        assert_eq!(frames.len(), 2);
        assert_ne!(frames[0][0] & FLAG_SPLIT, 0);
        assert_ne!(frames[1][0] & FLAG_SPLIT, 0);
        assert_eq!(frames[0].len(), 255); // 1 header + 254 payload
        assert_eq!(frames[1].len(), 2); // 1 header + 1 payload
    }

    #[test]
    fn split_300_bytes() {
        let data: Vec<u8> = (0u16..300).map(|i| (i & 0xFF) as u8).collect();
        let frames = build_lora_frames(&data, 0xA0);
        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0][1..], &data[..254]);
        assert_eq!(&frames[1][1..], &data[254..]);
    }

    #[test]
    fn split_max_508() {
        let data = vec![0xDD; 508];
        let frames = build_lora_frames(&data, 0xE0);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].len(), 255);
        assert_eq!(frames[1].len(), 255);
    }

    #[test]
    fn header_sequence_nibble() {
        let frames = build_lora_frames(&[1, 2, 3], 0xB0);
        assert_eq!(frames[0][0] >> 4, 0x0B);
    }

    #[test]
    fn both_frames_same_header() {
        let data = vec![0xFF; 300];
        let frames = build_lora_frames(&data, 0x40);
        assert_eq!(frames[0][0], frames[1][0]);
    }

    #[test]
    fn empty_payload() {
        let frames = build_lora_frames(&[], 0x20);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), 1); // header only
        assert_eq!(frames[0][0] & FLAG_SPLIT, 0);
    }

    // RX tests

    #[test]
    fn single_frame_delivery() {
        let mut r = SplitReassembler::new();
        let frame = vec![0x50, 0xAA, 0xBB, 0xCC];
        let result = r.feed(&frame, 0);
        assert_eq!(result, Some(vec![0xAA, 0xBB, 0xCC]));
    }

    #[test]
    fn split_reassembly() {
        let mut r = SplitReassembler::new();
        let header: u8 = 0x30 | FLAG_SPLIT;
        let frame1 = {
            let mut f = vec![header];
            f.extend_from_slice(&[1; 254]);
            f
        };
        let frame2 = {
            let mut f = vec![header];
            f.extend_from_slice(&[2; 46]);
            f
        };
        assert_eq!(r.feed(&frame1, 0), None);
        let result = r.feed(&frame2, 1).unwrap();
        assert_eq!(result.len(), 300);
        assert!(result[..254].iter().all(|&b| b == 1));
        assert!(result[254..].iter().all(|&b| b == 2));
    }

    #[test]
    fn split_different_sequence_restarts() {
        let mut r = SplitReassembler::new();
        let frame_a = vec![0x30 | FLAG_SPLIT, 0xAA];
        let frame_b = vec![0x50 | FLAG_SPLIT, 0xBB];
        assert_eq!(r.feed(&frame_a, 0), None);
        assert_eq!(r.feed(&frame_b, 1), None);
        // Buffer should now hold frame_b's payload
        let frame_b2 = vec![0x50 | FLAG_SPLIT, 0xCC];
        let result = r.feed(&frame_b2, 2).unwrap();
        assert_eq!(result, vec![0xBB, 0xCC]);
    }

    #[test]
    fn non_split_clears_pending() {
        let mut r = SplitReassembler::new();
        let split_frame = vec![0x30 | FLAG_SPLIT, 0xAA];
        assert_eq!(r.feed(&split_frame, 0), None);
        let single_frame = vec![0x70, 0xDD, 0xEE];
        let result = r.feed(&single_frame, 1);
        assert_eq!(result, Some(vec![0xDD, 0xEE]));
        // Buffer should be cleared, a second split half should not match
        let split_half2 = vec![0x30 | FLAG_SPLIT, 0xBB];
        assert_eq!(r.feed(&split_half2, 2), None); // new first half
    }

    #[test]
    fn timeout_clears_buffer() {
        let mut r = SplitReassembler::new();
        let frame = vec![0x30 | FLAG_SPLIT, 0xAA];
        assert_eq!(r.feed(&frame, 100), None);
        r.check_timeout(109, 10); // not expired yet
                                  // Buffer should still be there
        let frame2 = vec![0x30 | FLAG_SPLIT, 0xBB];
        let result = r.feed(&frame2, 109).unwrap();
        assert_eq!(result, vec![0xAA, 0xBB]);
    }

    #[test]
    fn timeout_expires_buffer() {
        let mut r = SplitReassembler::new();
        let frame = vec![0x30 | FLAG_SPLIT, 0xAA];
        assert_eq!(r.feed(&frame, 100), None);
        r.check_timeout(110, 10); // expired
                                  // Buffer cleared, second half should start a new buffer
        let frame2 = vec![0x30 | FLAG_SPLIT, 0xBB];
        assert_eq!(r.feed(&frame2, 111), None); // new first half, not reassembly
    }

    #[test]
    fn frame_too_short() {
        let mut r = SplitReassembler::new();
        assert_eq!(r.feed(&[], 0), None);
        assert_eq!(r.feed(&[0x50], 0), None);
    }

    #[test]
    fn round_trip_split() {
        let data: Vec<u8> = (0u16..300).map(|i| (i & 0xFF) as u8).collect();
        let frames = build_lora_frames(&data, 0x60);
        let mut r = SplitReassembler::new();
        assert_eq!(r.feed(&frames[0], 0), None);
        let result = r.feed(&frames[1], 1).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn round_trip_single() {
        let data = vec![0x42; 100];
        let frames = build_lora_frames(&data, 0x80);
        let mut r = SplitReassembler::new();
        let result = r.feed(&frames[0], 0).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn round_trip_exact_boundary() {
        // 254 bytes = single frame
        let data254 = vec![0x11; 254];
        let frames = build_lora_frames(&data254, 0x90);
        assert_eq!(frames.len(), 1);
        let mut r = SplitReassembler::new();
        assert_eq!(r.feed(&frames[0], 0).unwrap(), data254);

        // 255 bytes = split
        let data255 = vec![0x22; 255];
        let frames = build_lora_frames(&data255, 0x90);
        assert_eq!(frames.len(), 2);
        let mut r = SplitReassembler::new();
        assert_eq!(r.feed(&frames[0], 0), None);
        assert_eq!(r.feed(&frames[1], 1).unwrap(), data255);
    }

    #[test]
    fn interleaved_sequences() {
        let mut r = SplitReassembler::new();
        let frame_a1 = vec![0xA0 | FLAG_SPLIT, 0x11];
        let frame_b1 = vec![0xB0 | FLAG_SPLIT, 0x22];
        assert_eq!(r.feed(&frame_a1, 0), None);
        // frame_b1 arrives, frame_a1 is discarded, frame_b1 is now buffered
        assert_eq!(r.feed(&frame_b1, 1), None);
        // frame_a2 arrives, sequence mismatch with frame_b1, frame_b1 is discarded,
        // frame_a2 becomes the new buffer
        let frame_a2 = vec![0xA0 | FLAG_SPLIT, 0x33];
        assert_eq!(r.feed(&frame_a2, 2), None);
        // Another A-sequence frame arrives, same sequence as buffered frame, reassemble
        let frame_a3 = vec![0xA0 | FLAG_SPLIT, 0x44];
        let result = r.feed(&frame_a3, 3).unwrap();
        assert_eq!(result, vec![0x33, 0x44]);
    }

    #[test]
    fn split_second_half_never_arrives() {
        let mut r = SplitReassembler::new();
        let frame = vec![0x50 | FLAG_SPLIT, 0xAA, 0xBB];
        assert_eq!(r.feed(&frame, 0), None);
        // No second half, timeout will clear it
        r.check_timeout(10, 10);
        assert_eq!(r.seq, None);
    }

    #[test]
    fn two_complete_split_packets_in_sequence() {
        let mut r = SplitReassembler::new();
        // First split pair (seq 0x30)
        let f1a = vec![0x30 | FLAG_SPLIT, 0x11, 0x22];
        let f1b = vec![0x30 | FLAG_SPLIT, 0x33, 0x44];
        assert_eq!(r.feed(&f1a, 0), None);
        assert_eq!(r.feed(&f1b, 1).unwrap(), vec![0x11, 0x22, 0x33, 0x44]);
        // Second split pair (seq 0x70)
        let f2a = vec![0x70 | FLAG_SPLIT, 0xAA];
        let f2b = vec![0x70 | FLAG_SPLIT, 0xBB];
        assert_eq!(r.feed(&f2a, 2), None);
        assert_eq!(r.feed(&f2b, 3).unwrap(), vec![0xAA, 0xBB]);
    }

    #[test]
    fn non_split_between_split_halves() {
        let mut r = SplitReassembler::new();
        // First half of split
        let split1 = vec![0x30 | FLAG_SPLIT, 0xAA];
        assert_eq!(r.feed(&split1, 0), None);
        // Non-split interrupts, buffer cleared, non-split delivered
        let single = vec![0x50, 0xDD];
        assert_eq!(r.feed(&single, 1).unwrap(), vec![0xDD]);
        // Second half with same seq, treated as NEW first half (buffer was cleared)
        let split2 = vec![0x30 | FLAG_SPLIT, 0xBB];
        assert_eq!(r.feed(&split2, 2), None);
    }

    // Radio config wire protocol tests
    fn medium_profile() -> RadioConfigWire {
        RadioConfigWire {
            frequency_hz: 869_525_000,
            bandwidth_hz: 125_000,
            sf: 7,
            cr: 5,
            tx_power_dbm: 17,
            preamble_len: 24,
            csma_enabled: false,
            radio_silent: false,
            st_alock: 0,
            lt_alock: 0,
            // These profiles are built into full 21-byte frames, so the parsed
            // round-trip sees the lt_alock field present.
            lt_alock_present: true,
        }
    }

    fn fast_profile() -> RadioConfigWire {
        RadioConfigWire {
            frequency_hz: 869_525_000,
            bandwidth_hz: 500_000,
            sf: 7,
            cr: 5,
            tx_power_dbm: 17,
            preamble_len: 24,
            csma_enabled: false,
            radio_silent: false,
            st_alock: 0,
            lt_alock: 0,
            lt_alock_present: true,
        }
    }

    fn slow_profile() -> RadioConfigWire {
        RadioConfigWire {
            frequency_hz: 869_525_000,
            bandwidth_hz: 125_000,
            sf: 10,
            cr: 8,
            tx_power_dbm: 17,
            preamble_len: 24,
            csma_enabled: false,
            radio_silent: false,
            st_alock: 0,
            lt_alock: 0,
            lt_alock_present: true,
        }
    }

    #[test]
    fn radio_config_round_trip_medium() {
        let cfg = medium_profile();
        let frame = build_radio_config_frame(&cfg);
        assert_eq!(frame.len(), RADIO_CONFIG_FRAME_LEN);
        assert_eq!(&frame[0..2], &RADIO_CONFIG_MAGIC);
        let parsed = parse_radio_config(&frame[2..]).unwrap();
        assert_eq!(parsed, cfg);
    }

    /// The reset frame is pinned byte-for-byte: periculum drives it against
    /// flashed firmware, so the two sides must agree without a shared build.
    /// It must also stay distinguishable from every other frame on the
    /// control channel by (magic, length) alone — that pair is the firmware's
    /// entire dispatch.
    #[test]
    fn radio_reset_frame_is_pinned_and_unambiguous() {
        assert_eq!(RADIO_RESET_FRAME, [0xA4, 0xA4, 0x55, 0xF8]);
        assert_eq!(RADIO_RESET_ACK, [0xA4, 0xA4, 0x02]);
        assert_ne!(RADIO_RESET_FRAME.len(), RADIO_CONFIG_FRAME_LEN);
        assert_ne!(RADIO_RESET_FRAME.len(), RADIO_CONFIG_ACK.len());
        assert_ne!(RADIO_RESET_ACK, RADIO_CONFIG_ACK);
        // Too short to ever be a valid Reticulum packet (>= 19 bytes), so
        // consuming it can never swallow mesh traffic.
        assert!(RADIO_RESET_FRAME.len() < 19);
    }

    #[test]
    fn radio_config_round_trip_fast() {
        let cfg = fast_profile();
        let frame = build_radio_config_frame(&cfg);
        let parsed = parse_radio_config(&frame[2..]).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn radio_config_round_trip_slow() {
        let cfg = slow_profile();
        let frame = build_radio_config_frame(&cfg);
        let parsed = parse_radio_config(&frame[2..]).unwrap();
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn radio_config_byte_layout() {
        let cfg = medium_profile();
        let frame = build_radio_config_frame(&cfg);
        // magic
        assert_eq!(frame[0], 0xA4);
        assert_eq!(frame[1], 0xA4);
        // freq 869525000 = 0x33D3_E608
        assert_eq!(&frame[2..6], &[0x33, 0xD3, 0xE6, 0x08]);
        // bw 125000 = 0x0001_E848
        assert_eq!(&frame[6..10], &[0x00, 0x01, 0xE8, 0x48]);
        // sf=7, cr=5, txp=17
        assert_eq!(frame[10], 7);
        assert_eq!(frame[11], 5);
        assert_eq!(frame[12], 17);
        // preamble 24 = 0x0018
        assert_eq!(&frame[13..15], &[0x00, 0x18]);
        // csma_enabled = false
        assert_eq!(frame[15], 0);
        // radio_silent = false
        assert_eq!(frame[16], 0);
        // st_alock = 0 (2 BE)
        assert_eq!(&frame[17..19], &[0x00, 0x00]);
        // lt_alock = 0 (2 BE)
        assert_eq!(&frame[19..21], &[0x00, 0x00]);
        assert_eq!(frame.len(), RADIO_CONFIG_FRAME_LEN);
    }

    #[test]
    fn radio_config_parse_too_short() {
        assert!(parse_radio_config(&[0; 12]).is_none());
    }

    #[test]
    fn radio_config_parse_too_long() {
        assert!(parse_radio_config(&[0; 20]).is_none());
    }

    #[test]
    fn radio_config_parse_13_byte_backward_compat() {
        // Old 13-byte payload (no csma, radio_silent, or alock) must still
        // parse, defaulting the flags to false and the alock limits to 0.
        let cfg = medium_profile();
        let frame = build_radio_config_frame(&cfg);
        // Keep magic-stripped payload's first 13 bytes only.
        let legacy = &frame[2..2 + 13];
        assert_eq!(legacy.len(), 13);
        let parsed = parse_radio_config(legacy).unwrap();
        assert!(!parsed.csma_enabled);
        assert!(!parsed.radio_silent);
        assert_eq!(parsed.st_alock, 0);
        assert_eq!(parsed.lt_alock, 0);
        assert!(!parsed.lt_alock_present); // old frame carried no alock field
        assert_eq!(parsed.sf, 7);
    }

    #[test]
    fn radio_config_parse_14_byte_backward_compat() {
        // 14-byte payload from before the radio_silent flag existed, the
        // csma byte is present but radio_silent must default to false.
        let cfg = RadioConfigWire {
            csma_enabled: true,
            ..medium_profile()
        };
        let frame = build_radio_config_frame(&cfg);
        let legacy = &frame[2..2 + 14];
        assert_eq!(legacy.len(), 14);
        let parsed = parse_radio_config(legacy).unwrap();
        assert!(parsed.csma_enabled);
        assert!(!parsed.radio_silent);
        assert_eq!(parsed.st_alock, 0);
        assert_eq!(parsed.lt_alock, 0);
        assert!(!parsed.lt_alock_present);
    }

    #[test]
    fn radio_config_parse_15_byte_backward_compat() {
        // 15-byte payload (csma + radio_silent, no alock) must parse with the
        // alock limits defaulting to 0.
        let cfg = RadioConfigWire {
            csma_enabled: true,
            radio_silent: true,
            ..medium_profile()
        };
        let frame = build_radio_config_frame(&cfg);
        let legacy = &frame[2..2 + 15];
        assert_eq!(legacy.len(), 15);
        let parsed = parse_radio_config(legacy).unwrap();
        assert!(parsed.csma_enabled);
        assert!(parsed.radio_silent);
        assert_eq!(parsed.st_alock, 0);
        assert_eq!(parsed.lt_alock, 0);
        assert!(!parsed.lt_alock_present); // 15-byte frame stops before alock
    }

    #[test]
    fn radio_config_round_trip_radio_silent() {
        let cfg = RadioConfigWire {
            csma_enabled: true,
            radio_silent: true,
            ..medium_profile()
        };
        let frame = build_radio_config_frame(&cfg);
        assert_eq!(frame.len(), RADIO_CONFIG_FRAME_LEN);
        assert_eq!(frame[15], 1);
        assert_eq!(frame[16], 1);
        let parsed = parse_radio_config(&frame[2..]).unwrap();
        assert_eq!(parsed, cfg);
        assert!(parsed.radio_silent);
    }

    #[test]
    fn radio_config_round_trip_csma_enabled() {
        let cfg = RadioConfigWire {
            csma_enabled: true,
            ..medium_profile()
        };
        let frame = build_radio_config_frame(&cfg);
        assert_eq!(frame.len(), RADIO_CONFIG_FRAME_LEN);
        assert_eq!(frame[15], 1);
        let parsed = parse_radio_config(&frame[2..]).unwrap();
        assert_eq!(parsed, cfg);
        assert!(parsed.csma_enabled);
    }

    #[test]
    fn radio_config_parse_invalid_sf() {
        let cfg = medium_profile();
        let frame = build_radio_config_frame(&cfg);
        let mut data = frame[2..].to_vec();
        data[8] = 13; // SF out of range
        assert!(parse_radio_config(&data).is_none());
    }

    #[test]
    fn radio_config_parse_invalid_cr() {
        let cfg = medium_profile();
        let frame = build_radio_config_frame(&cfg);
        let mut data = frame[2..].to_vec();
        data[9] = 4; // CR too low
        assert!(parse_radio_config(&data).is_none());
        data[9] = 9; // CR too high
        assert!(parse_radio_config(&data).is_none());
    }

    #[test]
    fn radio_config_negative_tx_power() {
        let cfg = RadioConfigWire {
            tx_power_dbm: -3,
            ..medium_profile()
        };
        let frame = build_radio_config_frame(&cfg);
        let parsed = parse_radio_config(&frame[2..]).unwrap();
        assert_eq!(parsed.tx_power_dbm, -3);
    }

    #[test]
    fn radio_config_all_bandwidths() {
        for &bw in &[
            7_810u32, 10_420, 15_630, 20_830, 31_250, 41_670, 62_500, 125_000, 250_000, 500_000,
        ] {
            let cfg = RadioConfigWire {
                bandwidth_hz: bw,
                ..medium_profile()
            };
            let frame = build_radio_config_frame(&cfg);
            let parsed = parse_radio_config(&frame[2..]).unwrap();
            assert_eq!(parsed.bandwidth_hz, bw);
        }
    }

    #[test]
    fn radio_config_all_coding_rates() {
        for cr in 5..=8u8 {
            let cfg = RadioConfigWire {
                cr,
                ..medium_profile()
            };
            let frame = build_radio_config_frame(&cfg);
            let parsed = parse_radio_config(&frame[2..]).unwrap();
            assert_eq!(parsed.cr, cr);
        }
    }

    #[test]
    fn radio_config_sf_boundaries() {
        for sf in 5..=12u8 {
            let cfg = RadioConfigWire {
                sf,
                ..medium_profile()
            };
            let frame = build_radio_config_frame(&cfg);
            let parsed = parse_radio_config(&frame[2..]).unwrap();
            assert_eq!(parsed.sf, sf);
        }
    }

    #[test]
    fn radio_config_ack_format() {
        assert_eq!(RADIO_CONFIG_ACK, [0xA4, 0xA4, 0x01]);
    }

    #[test]
    fn radio_config_round_trip_alock() {
        // 50% short-term, 10% long-term limits survive the wire round-trip.
        let cfg = RadioConfigWire {
            st_alock: 5000,
            lt_alock: 1000,
            ..medium_profile()
        };
        let frame = build_radio_config_frame(&cfg);
        assert_eq!(frame.len(), RADIO_CONFIG_FRAME_LEN);
        // st_alock 5000 = 0x1388, lt_alock 1000 = 0x03E8, both big-endian.
        assert_eq!(&frame[17..19], &[0x13, 0x88]);
        assert_eq!(&frame[19..21], &[0x03, 0xE8]);
        let parsed = parse_radio_config(&frame[2..]).unwrap();
        assert_eq!(parsed, cfg);
        assert_eq!(parsed.st_alock, 5000);
        assert_eq!(parsed.lt_alock, 1000);
        assert!(parsed.lt_alock_present); // full frame carried the alock field
    }

    #[test]
    fn firmware_default_lt_alock_derives_from_frequency() {
        // Standalone default (no explicit host value): the ETSI P sub-band at
        // 869.525 MHz maps to the 10% cap -> lt_alock 1000 (0.10 * 10000).
        assert_eq!(firmware_default_lt_alock(869_525_000, None), 1000);
        // Other EU sub-bands derive their own lawful caps.
        assert_eq!(firmware_default_lt_alock(868_100_000, None), 100); // M: 1%
        assert_eq!(firmware_default_lt_alock(864_000_000, None), 10); // K: 0.1%
                                                                      // A US 915 MHz / out-of-band frequency with no explicit value stays off.
        assert_eq!(firmware_default_lt_alock(915_000_000, None), 0);
        assert_eq!(firmware_default_lt_alock(869_300_000, None), 0); // guard gap
    }

    #[test]
    fn firmware_default_lt_alock_explicit_wins() {
        // An explicit host value is used verbatim even on an EU frequency where
        // the lawful default would otherwise apply...
        assert_eq!(firmware_default_lt_alock(869_525_000, Some(5000)), 5000);
        // ...including an explicit 0 (host chose "unlimited"), which must NOT be
        // overridden by the frequency-derived default.
        assert_eq!(firmware_default_lt_alock(869_525_000, Some(0)), 0);
    }

    // -----------------------------------------------------------------------
    // Airtime lock tests
    // -----------------------------------------------------------------------

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-6, "expected {b}, got {a}");
    }

    #[test]
    fn alock_u16_mapping_matches_rnode() {
        // RNode firmware: st_airtime_limit = at / (100.0 * 100.0).
        approx(alock_u16_to_fraction(0), 0.0); // unlimited
        approx(alock_u16_to_fraction(100), 0.01); // 1%
        approx(alock_u16_to_fraction(1000), 0.1); // 10%
        approx(alock_u16_to_fraction(5000), 0.5); // 50%
        approx(alock_u16_to_fraction(9999), 0.9999);
        // >= 1.0 is treated as unlimited, matching RNode_Firmware.ino:951.
        approx(alock_u16_to_fraction(10000), 0.0);
        approx(alock_u16_to_fraction(20000), 0.0);
    }

    #[test]
    fn airtime_bins_size_matches_rnode() {
        // 3_600_000 ms / 7500 ms = 480 bins.
        assert_eq!(AIRTIME_BINS, 480);
        assert_eq!(AIRTIME_LONGTERM_MS, 3_600_000);
        assert_eq!(AIRTIME_BINLEN_MS, 7500);
    }

    #[test]
    fn airtime_add_and_update_fractions() {
        let mut t = AirtimeTracker::new();
        // Key 3000 ms of airtime into bin 0 at t=0.
        t.add_airtime(0, 3000);
        t.update(0);
        // short-term = (bin0 + prev) / (2 * binlen) = 3000 / 15000 = 0.2
        approx(t.short_term_airtime(), 0.2);
        // long-term = 3000 / 3_600_000
        approx(t.long_term_airtime(), 3000.0 / 3_600_000.0);
        assert!(!t.is_locked());
    }

    #[test]
    fn airtime_accumulates_within_bin() {
        let mut t = AirtimeTracker::new();
        t.add_airtime(0, 1000);
        t.add_airtime(100, 1500); // same bin (bin 0)
        t.update(200);
        // 2500 ms in bin 0 -> short-term 2500/15000
        approx(t.short_term_airtime(), 2500.0 / 15000.0);
        approx(t.long_term_airtime(), 2500.0 / 3_600_000.0);
    }

    #[test]
    fn airtime_zero_limit_never_locks() {
        let mut t = AirtimeTracker::new();
        // Saturate the current window with airtime but leave both limits at 0.
        t.add_airtime(0, u16::MAX as u64);
        t.update(0);
        assert!(t.short_term_airtime() > 0.0);
        assert!(!t.is_locked(), "limit 0.0 must never lock");
    }

    #[test]
    fn airtime_short_term_lock_boundary() {
        let mut t = AirtimeTracker::new();
        t.set_st_limit_u16(2000); // 20%
                                  // Just below: 20% of the 15000 ms two-bin window = 3000 ms.
        t.add_airtime(0, 2999);
        t.update(0);
        assert!(!t.is_locked());
        // Meets the limit -> locks (>= comparison, matching RNode).
        t.add_airtime(0, 1);
        t.update(0);
        approx(t.short_term_airtime(), 0.2);
        assert!(t.is_locked());
    }

    #[test]
    fn airtime_long_term_lock_boundary() {
        let mut t = AirtimeTracker::new();
        t.set_lt_limit_u16(100); // 1% of the 1-hour window = 36_000 ms
                                 // Spread airtime across distinct bins to build up the long-term sum
                                 // without tripping the short-term (unset) limit.
        for i in 0..11u64 {
            let now = i * AIRTIME_BINLEN_MS;
            t.add_airtime(now, 3000);
        }
        // 11 * 3000 = 33_000 ms < 36_000 ms -> below limit
        t.update(11 * AIRTIME_BINLEN_MS);
        assert!(!t.is_locked());
        // Add two more bins to cross 36_000 ms.
        t.add_airtime(11 * AIRTIME_BINLEN_MS, 3000);
        t.add_airtime(12 * AIRTIME_BINLEN_MS, 3000);
        t.update(12 * AIRTIME_BINLEN_MS);
        assert!(t.long_term_airtime() >= 0.01);
        assert!(t.is_locked());
    }

    #[test]
    fn airtime_lock_clears_when_window_empties() {
        let mut t = AirtimeTracker::new();
        t.set_st_limit_u16(2000); // 20%
        t.add_airtime(0, 3000);
        t.update(0);
        assert!(t.is_locked());
        // Advance two bins so the loaded bins are neither current nor previous;
        // the short-term window is empty again and the lock clears.
        let later = 3 * AIRTIME_BINLEN_MS;
        t.update(later);
        approx(t.short_term_airtime(), 0.0);
        assert!(!t.is_locked());
    }

    #[test]
    fn packet_airtime_single_vs_split() {
        // A <=254-byte payload is one frame; a larger one is two frames, and
        // each frame carries a full programmed preamble on the air.
        let single = packet_airtime_ms(100, 125_000, 7, 5, 24);
        assert_eq!(single, airtime_ms_with_preamble(101, 125_000, 7, 5, 24));
        let split = packet_airtime_ms(300, 125_000, 7, 5, 24);
        let expected = airtime_ms_with_preamble(255, 125_000, 7, 5, 24)
            + airtime_ms_with_preamble(1 + 300 - 254, 125_000, 7, 5, 24);
        assert_eq!(split, expected);
    }
}
