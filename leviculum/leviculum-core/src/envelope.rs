//! Framed control envelope for the LNode USB channel (Codeberg #238).
//!
//! The transport CDC carries HDLC-framed Reticulum packets, plus a small
//! out-of-band control plane between the attached host (`lnflash`, `lnsd`)
//! and the firmware. Historically that control plane was one hand-cut magic
//! per feature ([`RADIO_CONFIG_MAGIC`], [`RADIO_RESET_FRAME`]); this module
//! is the single envelope every further control frame rides in, so a new
//! frame type is a new constant and a payload codec, never new framing.
//!
//! # Wire format
//!
//! One envelope per HDLC frame:
//!
//! ```text
//! [0xA4, 0xA5] [type: u8] [len: u16 BE] [payload: len bytes]
//! ```
//!
//! The length is strict: a frame whose payload is shorter or longer than
//! `len` is malformed and answered with a refusal, never silence. A reader
//! that knows the envelope but not the type refuses it by name and stays in
//! sync — the HDLC delimiter bounds the frame, the header names what was
//! skipped.
//!
//! # Distinguishability from Reticulum packets
//!
//! Same argument as the legacy magics, sharpened per frame:
//!
//! * The first byte `0xA4` has the IFAC bit set; the USB channel runs
//!   without IFAC, so no Reticulum peer on it ever emits a packet whose
//!   first byte matches, and firmware older than the envelope drops a
//!   received envelope frame in packet parsing for the same reason.
//! * Every frame a host sends *before* it knows the firmware speaks the
//!   envelope (the [`TYPE_CAPABILITIES`] probe, [`TYPE_WALL_TIME`],
//!   [`TYPE_RESET`]) is shorter than the 19-byte minimum Reticulum wire
//!   packet, so it cannot be packet-shaped at all. Longer frames
//!   ([`TYPE_RADIO_CONFIG`], [`TYPE_TELEMETRY_TARGET`]) are only sent after
//!   a capability report proved the peer is envelope-speaking firmware.
//!
//! # Compatibility window
//!
//! The two legacy magics stay accepted by the firmware, and
//! [`classify_control_frame`] answers them with their legacy two-byte acks,
//! so a field board and a field host tool interoperate across the
//! transition in both directions. The retirement story lives in
//! `docs/src/firmware/usb-control-envelope.md`.

use alloc::vec::Vec;

use crate::constants::{IDENTITY_KEY_SIZE, TRUNCATED_HASHBYTES};
use crate::node_name::{NodeName, NODE_NAME_MAX_LEN};
use crate::rnode::{
    parse_radio_config, RadioConfigWire, RADIO_CONFIG_FRAME_LEN, RADIO_CONFIG_MAGIC,
    RADIO_RESET_FRAME,
};

/// Magic prefix of every envelope frame. Distinct from the legacy
/// [`RADIO_CONFIG_MAGIC`] in the second byte, so the two control planes can
/// never shadow each other during the compatibility window.
pub const ENVELOPE_MAGIC: [u8; 2] = [0xA4, 0xA5];

/// Envelope header: magic (2) + type (1) + length (2, big-endian).
pub const ENVELOPE_HEADER_LEN: usize = 5;

/// Version reported in the capability report. Bumped only if the envelope
/// framing itself ever changes shape; new frame types do not bump it.
pub const ENVELOPE_VERSION: u8 = 1;

// ---------------------------------------------------------------------------
// Frame types: host -> board commands
// ---------------------------------------------------------------------------

/// Placeholder type used in a refusal when the offending frame was too
/// short to carry a type byte at all.
pub const TYPE_UNSPECIFIED: u8 = 0x00;
/// Radio configuration; payload is the parameter block of the legacy frame
/// (see [`parse_radio_config`]), without any magic.
pub const TYPE_RADIO_CONFIG: u8 = 0x01;
/// Full system reset; empty payload.
pub const TYPE_RESET: u8 = 0x02;
/// Wall-time injection; payload is unix seconds as u64 big-endian.
pub const TYPE_WALL_TIME: u8 = 0x03;
/// Capability query; empty payload. Answered with
/// [`TYPE_CAPABILITY_REPORT`], which replaces guess-by-timeout probing.
pub const TYPE_CAPABILITIES: u8 = 0x04;
/// Telemetry target (Codeberg #236): destination hash, optional public
/// key, profile id. `profile == TELEMETRY_PROFILE_OFF` clears the target,
/// which is how telemetry is switched off — the configured target is the
/// switch.
pub const TYPE_TELEMETRY_TARGET: u8 = 0x05;
/// On-air transmit spacing (Codeberg #345); payload is milliseconds as
/// u16 big-endian. A measurement knob: it sets the gap the LoRa interface
/// leaves between the end of one packet's airtime and the key-up of the
/// next, so the spacing of the telemetry announce/report pair can be swept
/// without a reflash. `0` is the compiled default and imposes nothing.
pub const TYPE_TX_SPACING: u8 = 0x06;
/// Radio-configuration query (Codeberg #349); empty payload. Answered with
/// [`TYPE_RADIO_REPORT`] carrying the settings the radio is running right
/// now.
///
/// The read direction of [`TYPE_RADIO_CONFIG`], and it exists because that
/// frame carries the *whole* parameter set: a host that wants to change one
/// value and has no way to read the other nine can only substitute its own
/// defaults for them, which turns "set the transmit power" into "reset the
/// bandwidth as well". A sweep whose points silently differ in modulation is
/// not a sweep. Five bytes, so firmware from before the envelope drops it
/// silently and the probe times out, exactly like [`TYPE_CAPABILITIES`].
pub const TYPE_RADIO_QUERY: u8 = 0x07;
/// User-set fixed position: latitude, longitude, optional altitude, in the
/// scaled-integer units the telemetry wire uses. While set it replaces the
/// GNSS sensor as the reported position entirely — the user's "this is
/// where this node is" beats a wandering fix — and an explicit clear
/// returns the node to sensor reporting. Persisted beside the telemetry
/// target. See [`FixedPositionWire`] for the payload.
pub const TYPE_FIXED_POSITION: u8 = 0x08;
/// Media profile: which carriers this node meshes over. Payload is the
/// one flag byte of [`MediaProfileWire`], persisted beside the telemetry
/// target and the fixed position.
///
/// The frame exists because a node that meshes over LoRa **and** BLE at
/// once cannot be measured on either: a packet that arrived over the
/// other medium masks a loss on the medium under test, so every
/// single-medium number a dual-carrier node produces is falsifiable. The
/// profile is the declaration that makes the measurement honest, and it
/// is persisted so a reboot does not quietly undo it mid-run.
///
/// Answered with [`TYPE_MEDIA_REPORT`], not with a bare ack: a carrier
/// that did not come up at boot cannot be started before the next reset
/// — its driver task was never spawned — and an ack would claim
/// otherwise. The report states what is running and what is configured,
/// and the difference IS the "takes effect at reboot" answer.
pub const TYPE_MEDIA_PROFILE: u8 = 0x09;
/// Media-profile query; empty payload. The read direction of
/// [`TYPE_MEDIA_PROFILE`], answered with [`TYPE_MEDIA_REPORT`]. Five
/// bytes, so firmware from before the envelope drops it silently and the
/// probe times out, exactly like [`TYPE_CAPABILITIES`].
pub const TYPE_MEDIA_QUERY: u8 = 0x0A;
/// Position-source query; empty payload. Answered with
/// [`TYPE_POSITION_SOURCE_REPORT`].
///
/// The read direction of "does this node have a position source", which is
/// the second clause of the telemetry send condition
/// (`docs/src/concepts/telemetry.md`): a node with a target and no fixed
/// position and no GNSS receiver reports nothing at all. A host that has
/// just had a telemetry target acked cannot otherwise know that, and would
/// have to choose between saying nothing (and letting the operator wait for
/// reports that will never come) and guessing from the board model (wrong
/// the moment a T114 carries a pin). Five bytes, so firmware from before
/// the envelope drops it silently and the probe times out, exactly like
/// [`TYPE_CAPABILITIES`].
pub const TYPE_POSITION_SOURCE_QUERY: u8 = 0x0B;
/// Node name (Codeberg #235): the display name an operator chooses for
/// this board, replacing the derived `LNode-<hex8>` / `LN-<hex8>` pair.
/// Payload is a set/clear flag byte and, when set, the name's UTF-8
/// bytes; see [`encode_node_name`]. Persisted beside the telemetry
/// target, the fixed position and the media profile.
///
/// **One name, both surfaces.** The mesh display name (the LXMF
/// announce's `app_data`, what Columba lists) and the BLE GAP device
/// name (what a phone shows in its Bluetooth settings) are the same
/// value: a board that answered to two different names in two places
/// would be worse than one that answers to a hex string in both.
///
/// The name is **display only**. It never touches the identity, so two
/// boards may carry the same name and stay distinguishable — the hash
/// remains the addressing and disambiguation mechanism everywhere.
///
/// Answered with [`TYPE_NODE_NAME_REPORT`], not with a bare ack, for
/// the [`TYPE_MEDIA_PROFILE`] reason: the two surfaces do not adopt the
/// name at the same moment. The mesh name is in force for the next
/// announce; the BLE name is baked into the advertisement at boot and
/// follows at the next reset. The report states both, and the
/// difference IS the "takes effect at reboot" answer.
pub const TYPE_NODE_NAME: u8 = 0x0C;
/// Node-name query; empty payload. The read direction of
/// [`TYPE_NODE_NAME`], answered with [`TYPE_NODE_NAME_REPORT`]. Five
/// bytes, so firmware from before the envelope drops it silently and the
/// probe times out, exactly like [`TYPE_CAPABILITIES`].
pub const TYPE_NODE_NAME_QUERY: u8 = 0x0D;
/// Identity query; empty payload. Answered with
/// [`TYPE_IDENTITY_REPORT`] — the three hashes a prober needs before it
/// can address this board: the node identity hash, the
/// `rnstransport.probe` responder destination, and the LXMF delivery
/// destination.
///
/// The board reports the hashes it actually registered rather than the
/// host deriving them from the identity hash: derivation would encode
/// this firmware's application names into every host, and a report that
/// says "no probe responder" is an answer a derivation cannot give.
/// Read-only, safe mid-measurement like [`TYPE_MEDIA_QUERY`].
pub const TYPE_IDENTITY_QUERY: u8 = 0x0E;
/// Announce-now command (Codeberg #376); empty payload. The board
/// announces its LXMF delivery destination immediately, on all
/// interfaces, exactly as the telemetry path does before a report —
/// same app data, same clock gate. Without a calendar clock the
/// announce is withheld ([`REFUSE_NO_CLOCK`]) rather than sent: the
/// emission timestamp inside the announce is what peers rank paths by,
/// and an uptime-stamped announce would poison the very path the bench
/// is trying to observe.
///
/// A bench instrument: it separates "the announce never left the
/// board" from "it left and the receiver did not take it" without
/// waiting out the board's own announce cadence. One-shot, nothing is
/// persisted.
pub const TYPE_ANNOUNCE: u8 = 0x0F;
/// BLE inter-packet transmit gap (Codeberg #376); payload is the gap
/// in milliseconds, one big-endian u16 like [`TYPE_TX_SPACING`]. The
/// firmware's BLE drain leaves at least this gap between the last
/// fragment of one packet and the first fragment of the next packet on
/// the same connection handle. With no override the pumps serve the
/// compiled default of 100 ms (`leviculum-ble-tx`'s
/// `DEFAULT_TX_GAP_MS`, the measured desk value); a set value overrides
/// it and `0` disables the gap entirely.
///
/// A bench instrument like [`TYPE_TX_SPACING`], and volatile like it:
/// not persisted, a reset restores the default. Values above
/// [`BLE_TX_GAP_MAX_MS`] are refused with [`REFUSE_VALUE`] — the bound
/// lives in [`classify_control_frame`], so every binary and every
/// host-side stub refuses the same values.
pub const TYPE_BLE_TX_GAP: u8 = 0x10;

/// The largest [`TYPE_BLE_TX_GAP`] a board accepts. Five seconds is
/// already far beyond any honest inter-packet gap (the keepalive
/// interval is in that region); anything larger is a typo, not an
/// experiment, and gets a named refusal instead of a wedged-looking
/// link.
pub const BLE_TX_GAP_MAX_MS: u16 = 5_000;

// ---------------------------------------------------------------------------
// Frame types: board -> host responses
// ---------------------------------------------------------------------------

/// Positive acknowledgement; payload is the one type byte being acked.
pub const TYPE_ACK: u8 = 0x81;
/// Named refusal; payload is `[refused_type, reason]`.
pub const TYPE_REFUSAL: u8 = 0x82;
/// Capability report; payload is `[ENVELOPE_VERSION, accepted types...]`.
pub const TYPE_CAPABILITY_REPORT: u8 = 0x83;
/// Radio report (Codeberg #349); payload is the same parameter block
/// [`TYPE_RADIO_CONFIG`] carries, describing what the radio is running.
///
/// The same codec in both directions on purpose: a host reads one of these,
/// changes one field, and sends it straight back as a config. Any asymmetry
/// between the two encodings would be a place for a value to change while
/// being copied.
pub const TYPE_RADIO_REPORT: u8 = 0x84;
/// Media report; payload is `[running_flags, configured_flags]` in the
/// [`MediaProfileWire`] flag encoding. The answer to both
/// [`TYPE_MEDIA_PROFILE`] and [`TYPE_MEDIA_QUERY`].
///
/// Two values rather than one because they can honestly differ. Switching
/// a carrier off always takes effect at once, and switching one back on
/// does too **as long as it came up at boot** — it was only being
/// ignored. A carrier that did **not** come up at boot has no driver
/// task to un-ignore and cannot start before the next reset, and that is
/// the case where the two values part: `configured` is what a reboot
/// would come up with, `running` is what the board is doing right now. A
/// host that sees them differ says "takes effect at reboot" as a fact it
/// read off the board, not as a guess.
pub const TYPE_MEDIA_REPORT: u8 = 0x85;
/// Position-source report; payload is one flag byte
/// ([`POSITION_SOURCE_FIXED`], [`POSITION_SOURCE_GNSS`]). The answer to
/// [`TYPE_POSITION_SOURCE_QUERY`].
///
/// Flags rather than a bool because the two sources are not
/// interchangeable to an operator being told what to do next: "no pin, but
/// a receiver" is a node that will report as soon as it is outdoors, and
/// "neither" is a node that needs `--set-position`.
pub const TYPE_POSITION_SOURCE_REPORT: u8 = 0x86;
/// Node-name report; payload is `[flags, mesh_len, mesh…, ble_len, ble…]`
/// (see [`encode_node_name_report`]). The answer to both
/// [`TYPE_NODE_NAME`] and [`TYPE_NODE_NAME_QUERY`].
///
/// The two names are the **effective** ones, not the stored record: what
/// the next announce will carry, and what the BLE advertisement is
/// carrying right now. Effective rather than stored because the two
/// derived defaults differ (`LNode-<hex8>` on the mesh, `LN-<hex8>` on
/// BLE) and the BLE name is additionally truncated to
/// `leviculum_ble_tx::DEVICE_NAME_LEN` — so a host handed the record
/// alone could not print either string, and would have to guess at what
/// an operator is about to go looking for on a phone.
pub const TYPE_NODE_NAME_REPORT: u8 = 0x87;
/// Identity report; payload is `[flags, identity(16), probe(16),
/// lxmf(16)]`, 49 bytes fixed. The answer to [`TYPE_IDENTITY_QUERY`].
///
/// Fixed-length with validity flags rather than variable-length fields:
/// a destination a board did not register still occupies its 16 zeroed
/// bytes, and the flag says so. That keeps the parse a length check and
/// lets a future fourth hash extend the payload without ambiguity —
/// hosts reject only payloads *shorter* than they know
/// ([`decode_identity_report_payload`]), so an older lnflash keeps
/// reading the first three hashes from a longer report.
pub const TYPE_IDENTITY_REPORT: u8 = 0x88;

/// [`TYPE_IDENTITY_REPORT`] flag: the probe hash field is a registered
/// `rnstransport.probe` responder destination.
pub const IDENTITY_REPORT_PROBE: u8 = 0x01;
/// [`TYPE_IDENTITY_REPORT`] flag: the lxmf hash field is a registered
/// LXMF delivery destination.
pub const IDENTITY_REPORT_LXMF: u8 = 0x02;

/// [`TYPE_NODE_NAME_REPORT`] flag: an operator-set name is stored, so
/// the names in the report are that name rather than the derived
/// defaults.
pub const NODE_NAME_FLAG_STORED: u8 = 0x01;
/// [`TYPE_NODE_NAME_REPORT`] flag: the BLE surfaces are not yet showing
/// the configured name and will pick it up at the next boot.
///
/// The board decides this, not the host: only the board knows what its
/// advertisement was built with, and only it knows that the derived BLE
/// default (`LN-<hex8>`) is a different string from the derived mesh
/// default (`LNode-<hex8>`) rather than a truncation of it. A host that
/// compared the two names itself would call a freshly cleared board
/// "pending" forever.
pub const NODE_NAME_FLAG_BLE_PENDING: u8 = 0x02;

/// [`TYPE_POSITION_SOURCE_REPORT`] flag: a user-set fixed position is
/// stored.
pub const POSITION_SOURCE_FIXED: u8 = 0x01;
/// [`TYPE_POSITION_SOURCE_REPORT`] flag: a GNSS receiver is built into the
/// firmware and active. Set whether or not it currently has a fix — the
/// send condition is about intent, not possession.
pub const POSITION_SOURCE_GNSS: u8 = 0x02;

// ---------------------------------------------------------------------------
// Refusal reasons
// ---------------------------------------------------------------------------

/// The firmware does not accept this frame type (unknown, or known but not
/// yet consumed by this firmware).
pub const REFUSE_UNKNOWN_TYPE: u8 = 0x01;
/// The envelope header or the payload shape is wrong (truncated header,
/// length mismatch, payload that fails its codec).
pub const REFUSE_MALFORMED: u8 = 0x02;
/// The frame parsed but its value was refused (e.g. a wall time outside
/// the plausibility window).
pub const REFUSE_VALUE: u8 = 0x03;
/// The firmware is momentarily unable to take the frame; retry.
pub const REFUSE_BUSY: u8 = 0x04;
/// The frame type is known to the envelope layer, but this binary carries
/// no consumer that would honor it — retrying or rebooting cannot help,
/// only different firmware can. Distinct from [`REFUSE_UNKNOWN_TYPE`]
/// (the type itself is foreign) and from [`REFUSE_BUSY`] (retry works):
/// the shared control-envelope layer must never ack a capability the
/// binary does not have, which is exactly what the T114 did to a
/// telemetry target before it carried a reporter.
pub const REFUSE_UNSUPPORTED: u8 = 0x05;
/// The value was applied but did not reach flash, so a reset would lose
/// it (Codeberg #358).
///
/// The one refusal that is not a rejection: the board took the frame and
/// is running the new value right now. What it could not promise is the
/// half the client actually acts on — a scripted `set` followed by a
/// reset needs the record to be *durable*, and a board that acked while
/// the write was still owed sent that client into a reboot that lost the
/// setting. Distinct from [`REFUSE_BUSY`] because a retry is not obviously
/// the answer (the flash write already exhausted its own retries) and
/// distinct from [`REFUSE_VALUE`] because the value was fine.
pub const REFUSE_PERSIST: u8 = 0x06;
/// The command needs a calendar clock and the board has none yet.
///
/// Named rather than folded into [`REFUSE_VALUE`] (there is no value)
/// or [`REFUSE_BUSY`] (waiting alone cannot help a board with no time
/// source): the fix is specific — seed the clock, via a GNSS fix or
/// `--set-time` ([`TYPE_WALL_TIME`]) — and the host can only say so if
/// the refusal says which problem it has. First consumer:
/// [`TYPE_ANNOUNCE`], whose clock gate is the telemetry path's.
pub const REFUSE_NO_CLOCK: u8 = 0x07;

// ---------------------------------------------------------------------------
// Generic encode / decode
// ---------------------------------------------------------------------------

/// A decoded envelope frame, borrowing its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlFrame<'a> {
    pub frame_type: u8,
    pub payload: &'a [u8],
}

/// Why a byte sequence is not a well-formed envelope frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The magic is absent: this is not an envelope frame at all (most
    /// likely a Reticulum packet; not an error on the channel).
    NotEnvelope,
    /// The magic is present but the frame is not `header + len` bytes.
    Malformed {
        /// The type byte if the frame was long enough to carry one,
        /// [`TYPE_UNSPECIFIED`] otherwise. For the refusal answer.
        frame_type: u8,
    },
}

/// Whether these bytes claim to be an envelope frame (magic check only).
pub fn is_envelope(data: &[u8]) -> bool {
    data.len() >= ENVELOPE_MAGIC.len() && data[..ENVELOPE_MAGIC.len()] == ENVELOPE_MAGIC
}

/// Encode one envelope frame.
pub fn encode_frame(frame_type: u8, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= u16::MAX as usize);
    let mut out = Vec::with_capacity(ENVELOPE_HEADER_LEN + payload.len());
    out.extend_from_slice(&ENVELOPE_MAGIC);
    out.push(frame_type);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Decode one envelope frame. Strict: the frame must be exactly
/// `ENVELOPE_HEADER_LEN + len` bytes.
pub fn decode_frame(data: &[u8]) -> Result<ControlFrame<'_>, EnvelopeError> {
    if !is_envelope(data) {
        return Err(EnvelopeError::NotEnvelope);
    }
    let frame_type = if data.len() > 2 {
        data[2]
    } else {
        TYPE_UNSPECIFIED
    };
    if data.len() < ENVELOPE_HEADER_LEN {
        return Err(EnvelopeError::Malformed { frame_type });
    }
    let len = u16::from_be_bytes([data[3], data[4]]) as usize;
    if data.len() != ENVELOPE_HEADER_LEN + len {
        return Err(EnvelopeError::Malformed { frame_type });
    }
    Ok(ControlFrame {
        frame_type,
        payload: &data[ENVELOPE_HEADER_LEN..],
    })
}

// ---------------------------------------------------------------------------
// Payload codecs
// ---------------------------------------------------------------------------

/// Encode a complete wall-time frame.
pub fn encode_wall_time(unix_secs: u64) -> Vec<u8> {
    encode_frame(TYPE_WALL_TIME, &unix_secs.to_be_bytes())
}

/// Decode a wall-time payload: exactly 8 bytes, u64 big-endian.
pub fn decode_wall_time_payload(payload: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = payload.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

/// Encode a complete transmit-spacing frame (#345).
pub fn encode_tx_spacing(spacing_ms: u16) -> Vec<u8> {
    encode_frame(TYPE_TX_SPACING, &spacing_ms.to_be_bytes())
}

/// Decode a transmit-spacing payload: exactly 2 bytes, u16 big-endian.
///
/// Every value the two bytes can hold is a legal spacing, `0` included —
/// `0` is the default, "impose nothing", and not an absent value. So there
/// is no refusable range here and the only malformed frame is one of the
/// wrong length.
pub fn decode_tx_spacing_payload(payload: &[u8]) -> Option<u16> {
    let bytes: [u8; 2] = payload.try_into().ok()?;
    Some(u16::from_be_bytes(bytes))
}

/// Encode a complete announce-now frame (#376).
pub fn encode_announce() -> Vec<u8> {
    encode_frame(TYPE_ANNOUNCE, &[])
}

/// Encode a complete BLE transmit-gap frame (#376).
pub fn encode_ble_tx_gap(gap_ms: u16) -> Vec<u8> {
    encode_frame(TYPE_BLE_TX_GAP, &gap_ms.to_be_bytes())
}

/// Decode a BLE transmit-gap payload: exactly 2 bytes, u16 big-endian.
///
/// Shape only — the [`BLE_TX_GAP_MAX_MS`] bound is a value judgement
/// and belongs to [`classify_control_frame`], which refuses it with
/// [`REFUSE_VALUE`] rather than calling the frame malformed.
pub fn decode_ble_tx_gap_payload(payload: &[u8]) -> Option<u16> {
    let bytes: [u8; 2] = payload.try_into().ok()?;
    Some(u16::from_be_bytes(bytes))
}

/// Encode a complete reset frame.
pub fn encode_reset() -> Vec<u8> {
    encode_frame(TYPE_RESET, &[])
}

/// Encode a complete capability query.
pub fn encode_capability_query() -> Vec<u8> {
    encode_frame(TYPE_CAPABILITIES, &[])
}

/// Encode a complete radio-config frame; the payload is the same
/// parameter block the legacy magic frame carries after its magic.
pub fn encode_radio_config(cfg: &RadioConfigWire) -> Vec<u8> {
    let legacy = crate::rnode::build_radio_config_frame(cfg);
    encode_frame(TYPE_RADIO_CONFIG, &legacy[RADIO_CONFIG_MAGIC.len()..])
}

/// Encode a complete radio-config query (Codeberg #349).
pub fn encode_radio_query() -> Vec<u8> {
    encode_frame(TYPE_RADIO_QUERY, &[])
}

/// Encode a complete radio report: the settings the board is running, in the
/// codec [`encode_radio_config`] uses, so the host can change one field and
/// send it straight back.
pub fn encode_radio_report(cfg: &RadioConfigWire) -> Vec<u8> {
    let legacy = crate::rnode::build_radio_config_frame(cfg);
    encode_frame(TYPE_RADIO_REPORT, &legacy[RADIO_CONFIG_MAGIC.len()..])
}

/// Decode a radio-report payload back into the settings it describes.
pub fn decode_radio_report_payload(payload: &[u8]) -> Option<RadioConfigWire> {
    parse_radio_config(payload)
}

/// Encode a complete acknowledgement for `acked_type`.
pub fn encode_ack(acked_type: u8) -> Vec<u8> {
    encode_frame(TYPE_ACK, &[acked_type])
}

/// Decode an ack payload into the acked type.
pub fn decode_ack_payload(payload: &[u8]) -> Option<u8> {
    match payload {
        [acked] => Some(*acked),
        _ => None,
    }
}

/// Encode a complete named refusal of `refused_type` for `reason`.
pub fn encode_refusal(refused_type: u8, reason: u8) -> Vec<u8> {
    encode_frame(TYPE_REFUSAL, &[refused_type, reason])
}

/// Decode a refusal payload into `(refused_type, reason)`.
pub fn decode_refusal_payload(payload: &[u8]) -> Option<(u8, u8)> {
    match payload {
        [refused, reason] => Some((*refused, *reason)),
        _ => None,
    }
}

/// Encode a complete capability report for the given accepted types.
pub fn encode_capability_report(accepted: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + accepted.len());
    payload.push(ENVELOPE_VERSION);
    payload.extend_from_slice(accepted);
    encode_frame(TYPE_CAPABILITY_REPORT, &payload)
}

/// Decode a capability-report payload into `(version, accepted types)`.
pub fn decode_capability_report_payload(payload: &[u8]) -> Option<(u8, &[u8])> {
    let (version, accepted) = payload.split_first()?;
    Some((*version, accepted))
}

// ---------------------------------------------------------------------------
// Telemetry target (Codeberg #236 — wire format allocated here)
// ---------------------------------------------------------------------------

/// Telemetry-target profile id that **clears** the target instead of
/// setting one (Codeberg #236).
///
/// The configured target is the on-switch, so "off" is the absence of a
/// target and needs an encoding of its own. It rides in the profile slot
/// rather than in a magic destination hash: the payload already carries a
/// field whose whole job is to say which cadence applies, and "none"
/// belongs in that field's vocabulary. The rest of the payload is still
/// parsed and must still be well-formed — a clear frame is not a licence
/// to send a short one — and [`decode_telemetry_target_payload`] returns
/// it like any other, so the *reader* decides what an absent profile
/// means rather than the framing.
pub const TELEMETRY_PROFILE_OFF: u8 = 0x00;
/// Telemetry-target profile id: movement-driven reporting (see #236).
pub const TELEMETRY_PROFILE_TRACKER: u8 = 0x01;
/// Telemetry-target profile id: slow stationary heartbeat (see #236).
pub const TELEMETRY_PROFILE_STATION: u8 = 0x02;

/// Encode a complete telemetry-target frame that clears the target.
///
/// The destination hash is zeroed and no key is carried: with
/// [`TELEMETRY_PROFILE_OFF`] in the profile slot neither is read, and
/// sending the old target back to say "forget it" would put a
/// destination on the wire for no reason.
pub fn encode_telemetry_clear() -> Vec<u8> {
    encode_telemetry_target(&TelemetryTargetWire {
        profile: TELEMETRY_PROFILE_OFF,
        dest_hash: [0u8; TRUNCATED_HASHBYTES],
        public_key: None,
    })
}

/// The telemetry-target frame payload (Codeberg #236, amended 2026-08-22):
/// the public key is OPTIONAL and its presence is an explicit flag byte,
/// never inferred from the length. Hash-only is the common case — the user
/// knows the LXMF address, the node resolves the key over the air.
///
/// Payload layout:
///
/// ```text
/// [profile: u8] [dest_hash: 16] [key_present: u8] ([public_key: 64])
/// ```
///
/// `key_present` is `0x00` (absent, 18-byte payload) or `0x01` (present,
/// 82-byte payload); any other value is malformed. Profile semantics —
/// which id is the default, what each cadence policy means — belong to
/// #236; this module only fixes the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryTargetWire {
    pub profile: u8,
    pub dest_hash: [u8; TRUNCATED_HASHBYTES],
    pub public_key: Option<[u8; IDENTITY_KEY_SIZE]>,
}

/// Encode a complete telemetry-target frame.
pub fn encode_telemetry_target(target: &TelemetryTargetWire) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + TRUNCATED_HASHBYTES + 1 + IDENTITY_KEY_SIZE);
    payload.push(target.profile);
    payload.extend_from_slice(&target.dest_hash);
    match &target.public_key {
        Some(key) => {
            payload.push(0x01);
            payload.extend_from_slice(key);
        }
        None => payload.push(0x00),
    }
    encode_frame(TYPE_TELEMETRY_TARGET, &payload)
}

/// Decode a telemetry-target payload.
pub fn decode_telemetry_target_payload(payload: &[u8]) -> Option<TelemetryTargetWire> {
    const HASH_END: usize = 1 + TRUNCATED_HASHBYTES;
    if payload.len() < HASH_END + 1 {
        return None;
    }
    let profile = payload[0];
    let mut dest_hash = [0u8; TRUNCATED_HASHBYTES];
    dest_hash.copy_from_slice(&payload[1..HASH_END]);
    let public_key = match (payload[HASH_END], payload.len() - HASH_END - 1) {
        (0x00, 0) => None,
        (0x01, IDENTITY_KEY_SIZE) => {
            let mut key = [0u8; IDENTITY_KEY_SIZE];
            key.copy_from_slice(&payload[HASH_END + 1..]);
            Some(key)
        }
        _ => return None,
    };
    Some(TelemetryTargetWire {
        profile,
        dest_hash,
        public_key,
    })
}

// ---------------------------------------------------------------------------
// Fixed position (user-set position as telemetry source)
// ---------------------------------------------------------------------------

/// A user-set fixed position, in the scaled-integer units the telemetry
/// codec packs (`leviculum_lxmf::telemetry::Location`) and the policy
/// crate decides on: degrees × 1e6, metres × 1e2. Staying in the integer
/// domain end to end means the coordinates the user typed are the
/// coordinates that go on the air, without a float round trip in between.
///
/// Payload layout:
///
/// ```text
/// [set: u8] ([latitude_e6: i32 BE] [longitude_e6: i32 BE]
///            [alt_present: u8] ([altitude_e2: i32 BE]))
/// ```
///
/// `set` is `0x00` (clear, 1-byte payload — the whole command is "back to
/// the sensor", so nothing else travels) or `0x01` (set). `alt_present`
/// follows the telemetry target's key-present rule: an explicit flag byte,
/// never inferred from the length. A latitude outside ±90° or a longitude
/// outside ±180° is not a coordinate and the payload is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedPositionWire {
    /// Degrees north, times 1e6. Negative is the southern hemisphere.
    pub latitude_e6: i32,
    /// Degrees east, times 1e6. Negative is the western hemisphere.
    pub longitude_e6: i32,
    /// Metres above sea level, times 1e2; `None` when the user gave no
    /// altitude (reported as 0, the reference's own default for a
    /// synthesized location).
    pub altitude_e2: Option<i32>,
}

/// The largest legal `latitude_e6` (90°).
pub const FIXED_POSITION_MAX_LAT_E6: i32 = 90_000_000;
/// The largest legal `longitude_e6` (180°).
pub const FIXED_POSITION_MAX_LON_E6: i32 = 180_000_000;

/// Encode a complete fixed-position frame; `None` is the explicit clear.
pub fn encode_fixed_position(position: Option<&FixedPositionWire>) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + 4 + 4 + 1 + 4);
    match position {
        None => payload.push(0x00),
        Some(pos) => {
            payload.push(0x01);
            payload.extend_from_slice(&pos.latitude_e6.to_be_bytes());
            payload.extend_from_slice(&pos.longitude_e6.to_be_bytes());
            match pos.altitude_e2 {
                Some(alt) => {
                    payload.push(0x01);
                    payload.extend_from_slice(&alt.to_be_bytes());
                }
                None => payload.push(0x00),
            }
        }
    }
    encode_frame(TYPE_FIXED_POSITION, &payload)
}

/// Decode a fixed-position payload. The outer `None` is a malformed
/// payload; the inner `None` is a well-formed clear.
pub fn decode_fixed_position_payload(payload: &[u8]) -> Option<Option<FixedPositionWire>> {
    match payload {
        [0x00] => Some(None),
        [0x01, rest @ ..] if rest.len() >= 9 => {
            let latitude_e6 = i32::from_be_bytes(rest[..4].try_into().ok()?);
            let longitude_e6 = i32::from_be_bytes(rest[4..8].try_into().ok()?);
            let altitude_e2 = match (rest[8], rest.len()) {
                (0x00, 9) => None,
                (0x01, 13) => Some(i32::from_be_bytes(rest[9..].try_into().ok()?)),
                _ => return None,
            };
            if latitude_e6.unsigned_abs() > FIXED_POSITION_MAX_LAT_E6 as u32
                || longitude_e6.unsigned_abs() > FIXED_POSITION_MAX_LON_E6 as u32
            {
                return None;
            }
            Some(Some(FixedPositionWire {
                latitude_e6,
                longitude_e6,
                altitude_e2,
            }))
        }
        _ => None,
    }
}

/// Whether a persisted control record reached flash before the board
/// answered the frame that set it (Codeberg #358).
///
/// The third input to every answer on the persist path, beside the
/// binary's capability and the frame's delivery. An enum rather than a
/// third `bool` because the two states are not "worked / did not work":
/// [`Persist::Lost`] is a board that *is* running the new value and
/// cannot promise it survives a reboot, which is a different sentence
/// from either of the other two refusals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persist {
    /// The record is on the page: a reset comes back with it. The only
    /// state that may be acked.
    Durable,
    /// Applied in RAM, not written: [`REFUSE_PERSIST`].
    Lost,
}

/// The answer to a fixed-position frame, decided by capability first —
/// the exact rule [`telemetry_target_answer`] states, because the consumer
/// is the same one: only the telemetry reporter reads the fixed position,
/// so a binary without a reporter must refuse rather than ack a position
/// nothing will ever report. With a reporter, an undelivered frame is a
/// full channel: [`REFUSE_BUSY`], the host retries.
///
/// `persist` is the #358 clause: the pin is persisted, so the ack has to
/// mean the page carries it.
pub fn fixed_position_answer(reporter_wired: bool, delivered: bool, persist: Persist) -> Vec<u8> {
    if !reporter_wired {
        encode_refusal(TYPE_FIXED_POSITION, REFUSE_UNSUPPORTED)
    } else if !delivered {
        encode_refusal(TYPE_FIXED_POSITION, REFUSE_BUSY)
    } else if persist == Persist::Durable {
        encode_ack(TYPE_FIXED_POSITION)
    } else {
        encode_refusal(TYPE_FIXED_POSITION, REFUSE_PERSIST)
    }
}

// ---------------------------------------------------------------------------
// Media profile (which carriers this node meshes over)
// ---------------------------------------------------------------------------

/// The LoRa bit of a media-profile flag byte.
pub const MEDIA_FLAG_LORA: u8 = 0b0000_0001;
/// The BLE bit of a media-profile flag byte.
pub const MEDIA_FLAG_BLE: u8 = 0b0000_0010;
/// Every bit the flag byte currently assigns a meaning to. A byte with a
/// bit outside this mask was written by a firmware that knows a carrier
/// this one does not, and is refused rather than silently reinterpreted
/// as "that carrier is off".
pub const MEDIA_FLAGS_KNOWN: u8 = MEDIA_FLAG_LORA | MEDIA_FLAG_BLE;

/// Which carriers a node meshes over ([`TYPE_MEDIA_PROFILE`]).
///
/// Payload layout: one flag byte, `bit0 = lora`, `bit1 = ble`, set means
/// enabled. Bits are a byte rather than two bools on the wire because the
/// two carriers are one *profile* — "LoRa only" is a statement about both
/// of them, and splitting it into two independently-settable frames would
/// make a board reachable in a state no one asked for while the second
/// frame was in flight.
///
/// **The default, when no record was ever written, is both on.** That is
/// today's behaviour, and the absence of a profile must change nothing:
/// every fielded board is running both carriers, and a firmware update
/// that read an erased page as "everything off" would take those boards
/// off the mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaProfileWire {
    pub lora_enabled: bool,
    pub ble_enabled: bool,
}

impl MediaProfileWire {
    /// The default profile: both carriers on (see the type docs).
    pub const BOTH: Self = Self {
        lora_enabled: true,
        ble_enabled: true,
    };

    /// This profile as its flag byte.
    pub const fn flags(self) -> u8 {
        (if self.lora_enabled {
            MEDIA_FLAG_LORA
        } else {
            0
        }) | (if self.ble_enabled { MEDIA_FLAG_BLE } else { 0 })
    }

    /// A flag byte back as a profile, or `None` for a byte carrying a bit
    /// this firmware assigns no carrier to (see [`MEDIA_FLAGS_KNOWN`]).
    pub const fn from_flags(flags: u8) -> Option<Self> {
        if flags & !MEDIA_FLAGS_KNOWN != 0 {
            return None;
        }
        Some(Self {
            lora_enabled: flags & MEDIA_FLAG_LORA != 0,
            ble_enabled: flags & MEDIA_FLAG_BLE != 0,
        })
    }
}

/// Encode a complete media-profile frame.
pub fn encode_media_profile(profile: &MediaProfileWire) -> Vec<u8> {
    encode_frame(TYPE_MEDIA_PROFILE, &[profile.flags()])
}

/// Decode a media-profile payload: exactly one flag byte.
pub fn decode_media_profile_payload(payload: &[u8]) -> Option<MediaProfileWire> {
    match payload {
        [flags] => MediaProfileWire::from_flags(*flags),
        _ => None,
    }
}

/// Encode a complete media-profile query.
pub fn encode_media_query() -> Vec<u8> {
    encode_frame(TYPE_MEDIA_QUERY, &[])
}

/// Encode a complete media report: what the board is carrying traffic on
/// right now, and what a reboot would come up with. See
/// [`TYPE_MEDIA_REPORT`] for why those are two values.
pub fn encode_media_report(running: &MediaProfileWire, configured: &MediaProfileWire) -> Vec<u8> {
    encode_frame(TYPE_MEDIA_REPORT, &[running.flags(), configured.flags()])
}

/// Decode a media-report payload into `(running, configured)`.
pub fn decode_media_report_payload(payload: &[u8]) -> Option<(MediaProfileWire, MediaProfileWire)> {
    match payload {
        [running, configured] => Some((
            MediaProfileWire::from_flags(*running)?,
            MediaProfileWire::from_flags(*configured)?,
        )),
        _ => None,
    }
}

/// Encode a complete position-source query.
pub fn encode_position_source_query() -> Vec<u8> {
    encode_frame(TYPE_POSITION_SOURCE_QUERY, &[])
}

/// Encode a complete position-source report.
pub fn encode_position_source_report(flags: u8) -> Vec<u8> {
    encode_frame(TYPE_POSITION_SOURCE_REPORT, &[flags])
}

/// Decode a position-source report payload: exactly one flag byte.
///
/// Unknown bits are kept rather than refused: a newer board that grows a
/// third source must not read as malformed to an older host, which only
/// ever asks "is anything set" and can answer that from any non-zero value.
pub fn decode_position_source_report_payload(payload: &[u8]) -> Option<u8> {
    match payload {
        [flags] => Some(*flags),
        _ => None,
    }
}

/// The answer to a position-source query: the flags, or the same
/// capability refusal the telemetry target gets.
///
/// `reporter_wired` is the binary's declaration that it constructs a
/// telemetry reporter — the same gate as [`telemetry_target_answer`],
/// because the question only means anything about a board that reports at
/// all. A reporter-less binary answering `flags = 0` would be read as "set
/// a position and it will report", which is exactly the false promise
/// [`REFUSE_UNSUPPORTED`] exists to prevent.
pub fn position_source_query_answer(reporter_wired: bool, flags: u8) -> Vec<u8> {
    if reporter_wired {
        encode_position_source_report(flags)
    } else {
        encode_refusal(TYPE_POSITION_SOURCE_QUERY, REFUSE_UNSUPPORTED)
    }
}

/// The answer to a media-profile frame or a media query, decided by
/// capability first — the [`telemetry_target_answer`] rule on a third
/// frame.
///
/// `media_wired` is the binary's declaration that it reads the profile
/// and gates its carriers on it; `delivered` is whether this frame
/// actually took effect (for a query: always true, nothing had to be
/// taken). A binary that never wired the gate answers
/// [`REFUSE_UNSUPPORTED`] rather than acking a profile it would ignore —
/// which is the whole point of the profile, since a measurement run
/// against a board that silently kept both carriers up is a measurement
/// of nothing.
///
/// The accepted answer is a [`TYPE_MEDIA_REPORT`], never a bare ack: see
/// that constant for why the two numbers it carries are the honest reply
/// to "switch this medium on".
///
/// `persist` is the #358 clause, and it bites hardest on this frame: the
/// report says what a reboot would come up with, so a report written
/// before the record reached the page is a claim about a reboot that the
/// reboot itself disproved three times on the rig.
pub fn media_profile_answer(
    media_wired: bool,
    delivered: bool,
    persist: Persist,
    running: MediaProfileWire,
    configured: MediaProfileWire,
) -> Vec<u8> {
    if !media_wired {
        encode_refusal(TYPE_MEDIA_PROFILE, REFUSE_UNSUPPORTED)
    } else if !delivered {
        encode_refusal(TYPE_MEDIA_PROFILE, REFUSE_BUSY)
    } else if persist == Persist::Durable {
        encode_media_report(&running, &configured)
    } else {
        encode_refusal(TYPE_MEDIA_PROFILE, REFUSE_PERSIST)
    }
}

/// The answer to a media query: the same capability gate, refused under
/// the query's own type so a host can tell which frame was turned down.
pub fn media_query_answer(
    media_wired: bool,
    running: MediaProfileWire,
    configured: MediaProfileWire,
) -> Vec<u8> {
    if media_wired {
        encode_media_report(&running, &configured)
    } else {
        encode_refusal(TYPE_MEDIA_QUERY, REFUSE_UNSUPPORTED)
    }
}

// ---------------------------------------------------------------------------
// Node name (the operator-chosen display name, Codeberg #235)
// ---------------------------------------------------------------------------

/// Encode a complete node-name frame; `None` is the explicit clear, back
/// to the derived default.
///
/// Payload layout, the fixed position's set/clear shape on a
/// variable-length value:
///
/// ```text
/// [set: u8] ([name: 1..=NODE_NAME_MAX_LEN bytes of UTF-8])
/// ```
///
/// No length byte: the envelope header already carries the frame length,
/// and a second, redundant count is a second thing that can disagree with
/// the first. `set` is `0x00` (clear, 1-byte payload) or `0x01`.
pub fn encode_node_name(name: Option<&NodeName>) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + NODE_NAME_MAX_LEN);
    match name {
        None => payload.push(0x00),
        Some(name) => {
            payload.push(0x01);
            payload.extend_from_slice(name.as_bytes());
        }
    }
    encode_frame(TYPE_NODE_NAME, &payload)
}

/// Decode a node-name payload. The outer `None` is a malformed payload;
/// the inner `None` is a well-formed clear.
///
/// The name is validated with [`NodeName::parse`], the same rule the host
/// applies before sending: a board must not store a name it would then
/// have to render differently from how it arrived, whatever a host chose
/// to put on the wire.
pub fn decode_node_name_payload(payload: &[u8]) -> Option<Option<NodeName>> {
    match payload {
        [0x00] => Some(None),
        [0x01, name @ ..] => Some(Some(NodeName::parse(name).ok()?)),
        _ => None,
    }
}

/// Encode a complete node-name query.
pub fn encode_node_name_query() -> Vec<u8> {
    encode_frame(TYPE_NODE_NAME_QUERY, &[])
}

/// Encode a complete node-name report: the flags, the name the next
/// announce will carry, and the name the BLE advertisement is carrying
/// right now. See [`TYPE_NODE_NAME_REPORT`] for why both travel.
///
/// ```text
/// [flags: u8] [mesh_len: u8] [mesh: mesh_len bytes] [ble_len: u8] [ble: ble_len bytes]
/// ```
///
/// Length-prefixed here where the set frame is not, because this payload
/// carries two variable-length values and the envelope header can only
/// delimit one.
pub fn encode_node_name_report(flags: u8, mesh: &NodeName, ble: &NodeName) -> Vec<u8> {
    let mut payload = Vec::with_capacity(3 + mesh.len() + ble.len());
    payload.push(flags);
    payload.push(mesh.len() as u8);
    payload.extend_from_slice(mesh.as_bytes());
    payload.push(ble.len() as u8);
    payload.extend_from_slice(ble.as_bytes());
    encode_frame(TYPE_NODE_NAME_REPORT, &payload)
}

/// What a board said about its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeNameState {
    /// Whether an operator-set name is stored, as opposed to both names
    /// being derived from the identity.
    pub stored: bool,
    /// Whether the BLE surfaces still carry the previous name and will
    /// pick this one up at the next boot.
    pub ble_pending: bool,
    /// The display name the next LXMF announce will carry — what Columba
    /// lists. In force now.
    pub mesh: NodeName,
    /// The GAP/advertised name a scanner sees right now. Possibly
    /// truncated relative to [`mesh`](Self::mesh), and possibly still the
    /// previous name; see [`ble_pending`](Self::ble_pending).
    pub ble: NodeName,
}

/// Decode a node-name report payload.
///
/// Names are decoded with [`NodeName::decode`], not `parse`: a truncated
/// GAP name and a derived default are legitimate answers that the input
/// rules would refuse. Unknown flag bits are kept rather than refused —
/// a newer board that grows a third name surface must not read as
/// malformed to an older host, which only asks about the two bits it
/// knows.
pub fn decode_node_name_report_payload(payload: &[u8]) -> Option<NodeNameState> {
    let (&flags, rest) = payload.split_first()?;
    let (mesh, rest) = take_length_prefixed(rest)?;
    let (ble, rest) = take_length_prefixed(rest)?;
    if !rest.is_empty() {
        return None;
    }
    Some(NodeNameState {
        stored: flags & NODE_NAME_FLAG_STORED != 0,
        ble_pending: flags & NODE_NAME_FLAG_BLE_PENDING != 0,
        mesh: NodeName::decode(mesh).ok()?,
        ble: NodeName::decode(ble).ok()?,
    })
}

/// One `[len][bytes]` field of a node-name report, and what follows it.
fn take_length_prefixed(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&len, rest) = data.split_first()?;
    let len = usize::from(len);
    if rest.len() < len {
        return None;
    }
    Some(rest.split_at(len))
}

/// What a board is able to say about its names right now: the report's
/// flags, the mesh name in force, and the BLE name on the air.
///
/// `None` is not "no names" — it is "not yet": the derived defaults are
/// built from the identity hash, and USB comes up before the node does.
/// Passing `None` makes the answer [`REFUSE_BUSY`], which is the truth
/// about a boot-order window a retry gets past.
pub type NodeNameReport<'a> = Option<(u8, &'a NodeName, &'a NodeName)>;

/// The answer to a node-name frame, decided by capability first — the
/// [`media_profile_answer`] rule on a fourth frame.
///
/// `name_wired` is the binary's declaration that it reads the name record
/// and feeds both display surfaces from it. A binary that never wired
/// that answers [`REFUSE_UNSUPPORTED`] rather than acking a name it would
/// ignore: an operator who was told the board is now `Balkon-Nord` and
/// then cannot find it under that name on either surface has been lied
/// to, and no retry or reboot fixes it.
///
/// `report` is the readiness clause described on [`NodeNameReport`]; a
/// board that cannot state the result must not have applied the frame
/// either, so a `None` here means the host's retry is a real retry.
///
/// The accepted answer is a [`TYPE_NODE_NAME_REPORT`], never a bare ack:
/// see that constant for why the two names it carries are the honest
/// reply to "call this board X".
///
/// `persist` is the #358 clause: the name is persisted, and a report
/// written before the record reached the page is a claim about the next
/// boot that the next boot would disprove.
pub fn node_name_answer(name_wired: bool, persist: Persist, report: NodeNameReport) -> Vec<u8> {
    match (name_wired, report) {
        (false, _) => encode_refusal(TYPE_NODE_NAME, REFUSE_UNSUPPORTED),
        (true, None) => encode_refusal(TYPE_NODE_NAME, REFUSE_BUSY),
        (true, Some(_)) if persist != Persist::Durable => {
            encode_refusal(TYPE_NODE_NAME, REFUSE_PERSIST)
        }
        (true, Some((flags, mesh, ble))) => encode_node_name_report(flags, mesh, ble),
    }
}

/// The answer to a node-name query: the same two gates, refused under the
/// query's own type so a host can tell which frame was turned down.
pub fn node_name_query_answer(name_wired: bool, report: NodeNameReport) -> Vec<u8> {
    match (name_wired, report) {
        (false, _) => encode_refusal(TYPE_NODE_NAME_QUERY, REFUSE_UNSUPPORTED),
        (true, None) => encode_refusal(TYPE_NODE_NAME_QUERY, REFUSE_BUSY),
        (true, Some((flags, mesh, ble))) => encode_node_name_report(flags, mesh, ble),
    }
}

/// The answer to a telemetry-target frame, decided by capability first.
///
/// `reporter_wired` is the binary's declaration that it constructs a
/// telemetry reporter and drains the target channel; `delivered` is
/// whether this frame actually reached that channel. A binary without a
/// reporter answers [`REFUSE_UNSUPPORTED`] no matter what the envelope
/// layer could parse — an ack is a promise the target will be honored,
/// and only the reporter can keep it. With a reporter, an undelivered
/// frame is a full channel: [`REFUSE_BUSY`], the host retries.
///
/// `persist` is the #358 clause: the target is persisted, and the ack is
/// what a scripted `set`-then-reset acts on, so it may only go out once
/// the record is on the page.
///
/// Pure so the refusal path is provable on the host with a reporter-less
/// configuration, independent of which BSPs happen to wire a reporter
/// today.
pub fn telemetry_target_answer(reporter_wired: bool, delivered: bool, persist: Persist) -> Vec<u8> {
    if !reporter_wired {
        encode_refusal(TYPE_TELEMETRY_TARGET, REFUSE_UNSUPPORTED)
    } else if !delivered {
        encode_refusal(TYPE_TELEMETRY_TARGET, REFUSE_BUSY)
    } else if persist == Persist::Durable {
        encode_ack(TYPE_TELEMETRY_TARGET)
    } else {
        encode_refusal(TYPE_TELEMETRY_TARGET, REFUSE_PERSIST)
    }
}

// ---------------------------------------------------------------------------
// Identity report (lnprobe batch — the hashes a prober addresses)
// ---------------------------------------------------------------------------

/// The three hashes a [`TYPE_IDENTITY_REPORT`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityReportWire {
    /// The node identity hash (truncated, 16 bytes).
    pub identity: [u8; 16],
    /// The `rnstransport.probe` responder destination, when registered.
    pub probe: Option<[u8; 16]>,
    /// The LXMF delivery destination, when registered.
    pub lxmf: Option<[u8; 16]>,
}

/// Encode an identity query (empty payload).
pub fn encode_identity_query() -> Vec<u8> {
    encode_frame(TYPE_IDENTITY_QUERY, &[])
}

/// Encode an identity report: `[flags, identity(16), probe(16), lxmf(16)]`.
pub fn encode_identity_report(report: &IdentityReportWire) -> Vec<u8> {
    let mut payload = Vec::with_capacity(49);
    let mut flags = 0u8;
    if report.probe.is_some() {
        flags |= IDENTITY_REPORT_PROBE;
    }
    if report.lxmf.is_some() {
        flags |= IDENTITY_REPORT_LXMF;
    }
    payload.push(flags);
    payload.extend_from_slice(&report.identity);
    payload.extend_from_slice(&report.probe.unwrap_or([0u8; 16]));
    payload.extend_from_slice(&report.lxmf.unwrap_or([0u8; 16]));
    encode_frame(TYPE_IDENTITY_REPORT, &payload)
}

/// Decode an identity-report payload.
///
/// Accepts payloads *longer* than 49 bytes (see [`TYPE_IDENTITY_REPORT`]
/// for why a future extension must not read as malformed here), refuses
/// shorter ones.
pub fn decode_identity_report_payload(payload: &[u8]) -> Option<IdentityReportWire> {
    if payload.len() < 49 {
        return None;
    }
    let flags = payload[0];
    let field = |start: usize| -> [u8; 16] {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&payload[start..start + 16]);
        buf
    };
    Some(IdentityReportWire {
        identity: field(1),
        probe: (flags & IDENTITY_REPORT_PROBE != 0).then(|| field(17)),
        lxmf: (flags & IDENTITY_REPORT_LXMF != 0).then(|| field(33)),
    })
}

/// The answer to an identity query. `None` is the boot-order window in
/// which the node — and with it the hashes — does not exist yet:
/// [`REFUSE_BUSY`], the host retries, exactly the
/// [`node_name_query_answer`] readiness clause.
pub fn identity_query_answer(report: Option<&IdentityReportWire>) -> Vec<u8> {
    match report {
        None => encode_refusal(TYPE_IDENTITY_QUERY, REFUSE_BUSY),
        Some(report) => encode_identity_report(report),
    }
}

// ---------------------------------------------------------------------------
// Control-plane classification (the firmware accept path, testable on host)
// ---------------------------------------------------------------------------

/// What one deframed HDLC frame on the transport CDC asks the firmware to
/// do. Produced by [`classify_control_frame`]; the firmware executes the
/// action and writes the named answer, so the whole decision — including
/// the legacy magics and every refusal — is pure and host-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    /// Not control traffic: hand the frame to the node core as a
    /// Reticulum packet.
    NotControl,
    /// Legacy 4-byte reset magic: answer `RADIO_RESET_ACK`, then reset.
    LegacyReset,
    /// Legacy 21-byte config magic, payload valid: apply, persist, answer
    /// `RADIO_CONFIG_ACK`.
    LegacyRadioConfig(RadioConfigWire),
    /// Legacy config magic with an invalid payload. The legacy behaviour —
    /// log locally, answer nothing — is preserved verbatim; audible
    /// refusals begin with the envelope.
    LegacyRadioConfigInvalid,
    /// Envelope radio config: apply, persist, answer
    /// `encode_ack(TYPE_RADIO_CONFIG)`.
    RadioConfig(RadioConfigWire),
    /// Envelope reset: answer `encode_ack(TYPE_RESET)`, then reset.
    Reset,
    /// Envelope wall time: seed the calendar via
    /// `set_wall_time_unix_secs(.., TimeSource::Host)`; the seam's bool
    /// picks `encode_ack` or `encode_refusal(.., REFUSE_VALUE)`.
    WallTime(u64),
    /// Envelope capability query: answer
    /// `encode_capability_report(accepted)`.
    CapabilityQuery,
    /// Envelope radio query (Codeberg #349): answer
    /// `encode_radio_report(&running_config)`. Read-only — it changes
    /// nothing about the radio, which is what makes it safe to send to a
    /// board mid-measurement.
    RadioQuery,
    /// Envelope telemetry target (Codeberg #236): set or clear the
    /// reporting target, persist it, answer
    /// `encode_ack(TYPE_TELEMETRY_TARGET)`. `profile ==
    /// TELEMETRY_PROFILE_OFF` is the clear encoding; the destination hash
    /// and key are then meaningless and the firmware ignores them.
    TelemetryTarget(TelemetryTargetWire),
    /// Envelope transmit spacing (Codeberg #345): hand the value to the
    /// LoRa interface, which applies it at key-up, and answer
    /// `encode_ack(TYPE_TX_SPACING)`. A measurement knob, not persisted:
    /// a reset returns the board to the compiled default.
    TxSpacing(u16),
    /// Envelope fixed position: set (`Some`) or clear (`None`) the
    /// user-set position, persist it, answer via
    /// [`fixed_position_answer`] — the ack is gated on the binary's
    /// declared reporter exactly like the telemetry target's.
    FixedPosition(Option<FixedPositionWire>),
    /// Envelope media profile: apply and persist which carriers this node
    /// meshes over, answer via [`media_profile_answer`]. Switching a
    /// medium off takes effect at once; switching one on takes effect at
    /// the next boot, and the report says which happened.
    MediaProfile(MediaProfileWire),
    /// Envelope media query: answer via [`media_query_answer`]. Read-only,
    /// like [`RadioQuery`](Self::RadioQuery) — safe to send to a board
    /// mid-measurement.
    MediaQuery,
    /// Envelope position-source query: answer via
    /// [`position_source_query_answer`]. Read-only, like
    /// [`MediaQuery`](Self::MediaQuery).
    PositionSourceQuery,
    /// Envelope node name (Codeberg #235): set (`Some`) or clear
    /// (`None`) the operator-chosen display name, persist it, answer via
    /// [`node_name_answer`]. The mesh name is in force for the next
    /// announce; the BLE name follows at the next boot, and the report
    /// says so.
    NodeName(Option<NodeName>),
    /// Envelope node-name query: answer via [`node_name_query_answer`].
    /// Read-only, like [`MediaQuery`](Self::MediaQuery).
    NodeNameQuery,
    /// Envelope identity query: answer via [`identity_query_answer`].
    /// Read-only, like [`MediaQuery`](Self::MediaQuery).
    IdentityQuery,
    /// Envelope announce-now (Codeberg #376): the main loop — the one
    /// place the node lives — announces the LXMF delivery destination
    /// on all interfaces exactly as the telemetry path does, and
    /// answers `encode_ack(TYPE_ANNOUNCE)`; without a calendar clock it
    /// withholds and answers
    /// `encode_refusal(TYPE_ANNOUNCE, REFUSE_NO_CLOCK)`.
    AnnounceNow,
    /// Envelope BLE transmit gap (Codeberg #376): hand the value to the
    /// BLE interface layer, which serves it per connection between
    /// packets, and answer `encode_ack(TYPE_BLE_TX_GAP)`. A measurement
    /// knob like [`TxSpacing`](Self::TxSpacing), not persisted: a reset
    /// returns the board to `0`. The value is already inside
    /// [`BLE_TX_GAP_MAX_MS`] — the classifier refused anything larger.
    BleTxGap(u16),
    /// Anything envelope-shaped that cannot be executed: answer
    /// `encode_refusal(refused_type, reason)`. Never silence.
    Refuse { refused_type: u8, reason: u8 },
}

/// Classify one deframed frame from the transport CDC.
///
/// `accepted` is the firmware's accepted-type list (what the capability
/// report advertises). A type outside it is refused with
/// [`REFUSE_UNKNOWN_TYPE`] — which is also how a #236-aware host detects
/// a pre-#236 board: [`TYPE_TELEMETRY_TARGET`] comes back refused by
/// name instead of acked.
pub fn classify_control_frame(data: &[u8], accepted: &[u8]) -> ControlAction {
    if data == RADIO_RESET_FRAME {
        return ControlAction::LegacyReset;
    }
    if data.len() == RADIO_CONFIG_FRAME_LEN && data[..2] == RADIO_CONFIG_MAGIC {
        return match parse_radio_config(&data[2..]) {
            Some(cfg) => ControlAction::LegacyRadioConfig(cfg),
            None => ControlAction::LegacyRadioConfigInvalid,
        };
    }
    let frame = match decode_frame(data) {
        Ok(frame) => frame,
        Err(EnvelopeError::NotEnvelope) => return ControlAction::NotControl,
        Err(EnvelopeError::Malformed { frame_type }) => {
            return ControlAction::Refuse {
                refused_type: frame_type,
                reason: REFUSE_MALFORMED,
            }
        }
    };
    if !accepted.contains(&frame.frame_type) {
        return ControlAction::Refuse {
            refused_type: frame.frame_type,
            reason: REFUSE_UNKNOWN_TYPE,
        };
    }
    let malformed = ControlAction::Refuse {
        refused_type: frame.frame_type,
        reason: REFUSE_MALFORMED,
    };
    match frame.frame_type {
        TYPE_RADIO_CONFIG => match parse_radio_config(frame.payload) {
            Some(cfg) => ControlAction::RadioConfig(cfg),
            None => malformed,
        },
        TYPE_RESET => {
            if frame.payload.is_empty() {
                ControlAction::Reset
            } else {
                malformed
            }
        }
        TYPE_WALL_TIME => match decode_wall_time_payload(frame.payload) {
            Some(unix_secs) => ControlAction::WallTime(unix_secs),
            None => malformed,
        },
        TYPE_CAPABILITIES => {
            if frame.payload.is_empty() {
                ControlAction::CapabilityQuery
            } else {
                malformed
            }
        }
        TYPE_RADIO_QUERY => {
            if frame.payload.is_empty() {
                ControlAction::RadioQuery
            } else {
                malformed
            }
        }
        TYPE_TELEMETRY_TARGET => match decode_telemetry_target_payload(frame.payload) {
            Some(target) => ControlAction::TelemetryTarget(target),
            None => malformed,
        },
        TYPE_TX_SPACING => match decode_tx_spacing_payload(frame.payload) {
            Some(spacing_ms) => ControlAction::TxSpacing(spacing_ms),
            None => malformed,
        },
        TYPE_FIXED_POSITION => match decode_fixed_position_payload(frame.payload) {
            Some(position) => ControlAction::FixedPosition(position),
            None => malformed,
        },
        TYPE_MEDIA_PROFILE => match decode_media_profile_payload(frame.payload) {
            Some(profile) => ControlAction::MediaProfile(profile),
            None => malformed,
        },
        TYPE_MEDIA_QUERY => {
            if frame.payload.is_empty() {
                ControlAction::MediaQuery
            } else {
                malformed
            }
        }
        TYPE_POSITION_SOURCE_QUERY => {
            if frame.payload.is_empty() {
                ControlAction::PositionSourceQuery
            } else {
                malformed
            }
        }
        TYPE_NODE_NAME => match decode_node_name_payload(frame.payload) {
            Some(name) => ControlAction::NodeName(name),
            None => malformed,
        },
        TYPE_NODE_NAME_QUERY => {
            if frame.payload.is_empty() {
                ControlAction::NodeNameQuery
            } else {
                malformed
            }
        }
        TYPE_IDENTITY_QUERY => {
            if frame.payload.is_empty() {
                ControlAction::IdentityQuery
            } else {
                malformed
            }
        }
        TYPE_ANNOUNCE => {
            if frame.payload.is_empty() {
                ControlAction::AnnounceNow
            } else {
                malformed
            }
        }
        TYPE_BLE_TX_GAP => match decode_ble_tx_gap_payload(frame.payload) {
            // The bound is enforced here in the shared decision
            // function, so every binary and the host-side test stub
            // refuse the same values by the same rule.
            Some(gap_ms) if gap_ms <= BLE_TX_GAP_MAX_MS => ControlAction::BleTxGap(gap_ms),
            Some(_) => ControlAction::Refuse {
                refused_type: TYPE_BLE_TX_GAP,
                reason: REFUSE_VALUE,
            },
            None => malformed,
        },
        // In the accepted list but without an executor here: refusing is
        // more honest than a firmware that acks what it cannot do.
        _ => ControlAction::Refuse {
            refused_type: frame.frame_type,
            reason: REFUSE_UNKNOWN_TYPE,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// The accepted list of the current firmware (both boards).
    const ACCEPTED: &[u8] = &[
        TYPE_RADIO_CONFIG,
        TYPE_RESET,
        TYPE_WALL_TIME,
        TYPE_CAPABILITIES,
        TYPE_TELEMETRY_TARGET,
        TYPE_TX_SPACING,
        TYPE_RADIO_QUERY,
        TYPE_FIXED_POSITION,
        TYPE_MEDIA_PROFILE,
        TYPE_MEDIA_QUERY,
        TYPE_POSITION_SOURCE_QUERY,
        TYPE_NODE_NAME,
        TYPE_NODE_NAME_QUERY,
        TYPE_IDENTITY_QUERY,
        TYPE_ANNOUNCE,
        TYPE_BLE_TX_GAP,
    ];

    /// A firmware from before #236 landed its telemetry consumer.
    const ACCEPTED_PRE_236: &[u8] = &[
        TYPE_RADIO_CONFIG,
        TYPE_RESET,
        TYPE_WALL_TIME,
        TYPE_CAPABILITIES,
    ];

    fn sample_config() -> RadioConfigWire {
        RadioConfigWire {
            frequency_hz: 869_463_000,
            bandwidth_hz: 125_000,
            sf: 8,
            cr: 5,
            tx_power_dbm: 22,
            preamble_len: 16,
            csma_enabled: true,
            radio_silent: false,
            st_alock: 0,
            lt_alock: 1000,
            lt_alock_present: true,
        }
    }

    #[test]
    fn a_frame_round_trips_through_encode_and_decode() {
        let payload = [0xDE, 0xAD, 0xBE, 0xEF];
        let bytes = encode_frame(0x7F, &payload);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.frame_type, 0x7F);
        assert_eq!(frame.payload, &payload);
    }

    #[test]
    fn an_empty_payload_round_trips() {
        let bytes = encode_frame(TYPE_RESET, &[]);
        assert_eq!(bytes.len(), ENVELOPE_HEADER_LEN);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.frame_type, TYPE_RESET);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn a_reticulum_packet_is_not_mistaken_for_an_envelope() {
        // A plausible packet header: no envelope magic.
        let packet = [0x02u8; 24];
        assert_eq!(decode_frame(&packet), Err(EnvelopeError::NotEnvelope));
        assert_eq!(
            classify_control_frame(&packet, ACCEPTED),
            ControlAction::NotControl
        );
    }

    #[test]
    fn a_truncated_header_is_malformed_with_the_type_it_managed_to_carry() {
        // Magic + type, but no length bytes.
        let bytes = [ENVELOPE_MAGIC[0], ENVELOPE_MAGIC[1], TYPE_WALL_TIME];
        assert_eq!(
            decode_frame(&bytes),
            Err(EnvelopeError::Malformed {
                frame_type: TYPE_WALL_TIME
            })
        );
        // Magic alone: not even a type byte survived.
        let bytes = ENVELOPE_MAGIC;
        assert_eq!(
            decode_frame(&bytes),
            Err(EnvelopeError::Malformed {
                frame_type: TYPE_UNSPECIFIED
            })
        );
    }

    #[test]
    fn a_length_mismatch_is_malformed_in_both_directions() {
        let mut truncated = encode_wall_time(1_790_000_000);
        truncated.pop();
        assert_eq!(
            decode_frame(&truncated),
            Err(EnvelopeError::Malformed {
                frame_type: TYPE_WALL_TIME
            })
        );
        let mut oversized = encode_wall_time(1_790_000_000);
        oversized.push(0x00);
        assert_eq!(
            decode_frame(&oversized),
            Err(EnvelopeError::Malformed {
                frame_type: TYPE_WALL_TIME
            })
        );
    }

    #[test]
    fn an_unknown_type_is_refused_by_name_not_silence() {
        let bytes = encode_frame(0x6E, &[1, 2, 3]);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::Refuse {
                refused_type: 0x6E,
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    #[test]
    fn a_pre_236_firmware_refuses_the_telemetry_type_by_name() {
        // How a #236-aware host detects an older board: a named refusal,
        // not a timeout.
        let target = TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_TRACKER,
            dest_hash: [0x11; TRUNCATED_HASHBYTES],
            public_key: None,
        };
        let bytes = encode_telemetry_target(&target);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED_PRE_236),
            ControlAction::Refuse {
                refused_type: TYPE_TELEMETRY_TARGET,
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    #[test]
    fn a_telemetry_target_classifies_into_its_action() {
        let target = TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_STATION,
            dest_hash: [0x11; TRUNCATED_HASHBYTES],
            public_key: None,
        };
        assert_eq!(
            classify_control_frame(&encode_telemetry_target(&target), ACCEPTED),
            ControlAction::TelemetryTarget(target)
        );
    }

    #[test]
    fn the_clear_frame_carries_the_off_profile_and_no_destination() {
        let bytes = encode_telemetry_clear();
        let target = match classify_control_frame(&bytes, ACCEPTED) {
            ControlAction::TelemetryTarget(t) => t,
            other => panic!("clear frame classified as {other:?}"),
        };
        assert_eq!(target.profile, TELEMETRY_PROFILE_OFF);
        assert_eq!(target.dest_hash, [0u8; TRUNCATED_HASHBYTES]);
        assert_eq!(target.public_key, None);
    }

    #[test]
    fn a_binary_without_a_reporter_refuses_the_target_it_cannot_honor() {
        // Ack honesty: the reporter-less configuration answers a named
        // refusal no matter what the channel would have taken — an ack
        // here was the T114's "telemetry on" over a binary with no
        // reporter to honor it.
        for delivered in [false, true] {
            for persist in [Persist::Durable, Persist::Lost] {
                assert_eq!(
                    decode_refusal_payload(
                        &telemetry_target_answer(false, delivered, persist)[ENVELOPE_HEADER_LEN..]
                    ),
                    Some((TYPE_TELEMETRY_TARGET, REFUSE_UNSUPPORTED))
                );
            }
        }
    }

    #[test]
    fn a_binary_with_a_reporter_acks_a_delivered_target_and_names_a_full_channel() {
        assert_eq!(
            decode_ack_payload(
                &telemetry_target_answer(true, true, Persist::Durable)[ENVELOPE_HEADER_LEN..]
            ),
            Some(TYPE_TELEMETRY_TARGET)
        );
        assert_eq!(
            decode_refusal_payload(
                &telemetry_target_answer(true, false, Persist::Durable)[ENVELOPE_HEADER_LEN..]
            ),
            Some((TYPE_TELEMETRY_TARGET, REFUSE_BUSY))
        );
    }

    /// Codeberg #358: the ack is what a scripted `set`-then-reset acts
    /// on, so a target that only ever reached RAM has to come back as a
    /// refusal the client can tell apart from every other one.
    #[test]
    fn a_target_that_did_not_reach_flash_is_refused_by_its_own_name() {
        assert_eq!(
            decode_refusal_payload(
                &telemetry_target_answer(true, true, Persist::Lost)[ENVELOPE_HEADER_LEN..]
            ),
            Some((TYPE_TELEMETRY_TARGET, REFUSE_PERSIST))
        );
        assert_ne!(REFUSE_PERSIST, REFUSE_BUSY);
        assert_ne!(REFUSE_PERSIST, REFUSE_VALUE);
        assert_ne!(REFUSE_PERSIST, REFUSE_UNSUPPORTED);
    }

    #[test]
    fn a_malformed_telemetry_payload_is_refused_audibly() {
        // Truncated hash: the payload's own codec refuses it, and the
        // classifier turns that into a named refusal rather than an ack
        // for a target it could not read.
        let bytes = encode_frame(TYPE_TELEMETRY_TARGET, &[TELEMETRY_PROFILE_STATION, 0x01]);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_TELEMETRY_TARGET,
                reason: REFUSE_MALFORMED
            }
        );
    }

    // -----------------------------------------------------------------
    // Fixed position (user-set position as telemetry source)
    // -----------------------------------------------------------------

    /// Berlin, with an altitude: the payload that exercises every field.
    fn berlin() -> FixedPositionWire {
        FixedPositionWire {
            latitude_e6: 52_520_008,
            longitude_e6: 13_404_954,
            altitude_e2: Some(3_400),
        }
    }

    #[test]
    fn a_fixed_position_round_trips_with_and_without_an_altitude() {
        for position in [
            berlin(),
            FixedPositionWire {
                altitude_e2: None,
                ..berlin()
            },
            // Southern and western hemispheres, below sea level.
            FixedPositionWire {
                latitude_e6: -36_848_460,
                longitude_e6: -73_044_440,
                altitude_e2: Some(-43_000),
            },
        ] {
            assert_eq!(
                classify_control_frame(&encode_fixed_position(Some(&position)), ACCEPTED),
                ControlAction::FixedPosition(Some(position))
            );
        }
    }

    #[test]
    fn the_clear_frame_is_one_byte_and_classifies_as_clear() {
        let bytes = encode_fixed_position(None);
        assert_eq!(bytes.len(), ENVELOPE_HEADER_LEN + 1);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::FixedPosition(None)
        );
    }

    #[test]
    fn a_coordinate_outside_the_globe_is_refused_as_malformed() {
        // ±90°/±180° themselves are legal (the poles and the antimeridian
        // are places); one microdegree beyond is not a coordinate.
        for (latitude_e6, longitude_e6) in [
            (90_000_001, 0),
            (-90_000_001, 0),
            (0, 180_000_001),
            (0, -180_000_001),
            (i32::MAX, i32::MIN),
        ] {
            let position = FixedPositionWire {
                latitude_e6,
                longitude_e6,
                altitude_e2: None,
            };
            assert_eq!(
                classify_control_frame(&encode_fixed_position(Some(&position)), ACCEPTED),
                ControlAction::Refuse {
                    refused_type: TYPE_FIXED_POSITION,
                    reason: REFUSE_MALFORMED
                },
                "lat={latitude_e6} lon={longitude_e6} was accepted"
            );
        }
        for (latitude_e6, longitude_e6) in [(90_000_000, 180_000_000), (-90_000_000, -180_000_000)]
        {
            let position = FixedPositionWire {
                latitude_e6,
                longitude_e6,
                altitude_e2: None,
            };
            assert_eq!(
                classify_control_frame(&encode_fixed_position(Some(&position)), ACCEPTED),
                ControlAction::FixedPosition(Some(position))
            );
        }
    }

    #[test]
    fn the_alt_present_flag_is_explicit_never_length_guessing() {
        // Flag says absent but altitude bytes follow: malformed.
        let mut bytes = encode_fixed_position(Some(&berlin()));
        bytes[ENVELOPE_HEADER_LEN + 9] = 0x00;
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_FIXED_POSITION,
                reason: REFUSE_MALFORMED
            }
        );
        // Flag says present but the altitude is truncated: malformed.
        let mut truncated = encode_fixed_position(Some(&berlin()));
        truncated.pop();
        truncated[4] -= 1; // keep the strict length header honest
        assert_eq!(
            classify_control_frame(&truncated, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_FIXED_POSITION,
                reason: REFUSE_MALFORMED
            }
        );
        // A flag value that is neither 0 nor 1: malformed.
        let mut flagged = encode_fixed_position(Some(&FixedPositionWire {
            altitude_e2: None,
            ..berlin()
        }));
        flagged[ENVELOPE_HEADER_LEN + 9] = 0x02;
        assert_eq!(
            classify_control_frame(&flagged, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_FIXED_POSITION,
                reason: REFUSE_MALFORMED
            }
        );
        // A set flag that is neither clear nor set: malformed.
        let mut set_flag = encode_fixed_position(None);
        set_flag[ENVELOPE_HEADER_LEN] = 0x02;
        assert_eq!(
            classify_control_frame(&set_flag, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_FIXED_POSITION,
                reason: REFUSE_MALFORMED
            }
        );
    }

    #[test]
    fn firmware_without_the_fixed_position_refuses_it_by_name() {
        // A board flashed before the feature answers what is missing
        // instead of going quiet — the same detection path as #236's.
        assert_eq!(
            classify_control_frame(&encode_fixed_position(Some(&berlin())), ACCEPTED_PRE_236),
            ControlAction::Refuse {
                refused_type: TYPE_FIXED_POSITION,
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    #[test]
    fn a_binary_without_a_reporter_refuses_the_position_it_cannot_report() {
        // Ack honesty, the fc60b95 rule applied to the new frame: only the
        // reporter reads the fixed position, so a reporter-less binary
        // answers a named refusal whatever the channel would have taken.
        for delivered in [false, true] {
            assert_eq!(
                decode_refusal_payload(
                    &fixed_position_answer(false, delivered, Persist::Durable)
                        [ENVELOPE_HEADER_LEN..]
                ),
                Some((TYPE_FIXED_POSITION, REFUSE_UNSUPPORTED))
            );
        }
        assert_eq!(
            decode_ack_payload(
                &fixed_position_answer(true, true, Persist::Durable)[ENVELOPE_HEADER_LEN..]
            ),
            Some(TYPE_FIXED_POSITION)
        );
        assert_eq!(
            decode_refusal_payload(
                &fixed_position_answer(true, false, Persist::Durable)[ENVELOPE_HEADER_LEN..]
            ),
            Some((TYPE_FIXED_POSITION, REFUSE_BUSY))
        );
    }

    /// Codeberg #358 on the second record of the persist path.
    #[test]
    fn a_position_that_did_not_reach_flash_is_refused_by_its_own_name() {
        assert_eq!(
            decode_refusal_payload(
                &fixed_position_answer(true, true, Persist::Lost)[ENVELOPE_HEADER_LEN..]
            ),
            Some((TYPE_FIXED_POSITION, REFUSE_PERSIST))
        );
    }

    // -----------------------------------------------------------------
    // Transmit spacing (Codeberg #345)
    // -----------------------------------------------------------------

    #[test]
    fn the_tx_spacing_frame_round_trips_and_classifies() {
        let bytes = encode_tx_spacing(60);
        // Like every other pre-handshake-shaped command, it stays under
        // the 19-byte minimum Reticulum packet, so firmware that does not
        // know the type can never take it for one.
        assert!(bytes.len() < 19);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::TxSpacing(60)
        );
    }

    #[test]
    fn zero_and_the_largest_spacing_both_survive_the_wire() {
        // 0 is the default ("impose nothing"), not an absent value, so it
        // has to arrive as a value and be acted on like any other.
        for spacing_ms in [0u16, 1, 15, 76, 1_000, u16::MAX] {
            assert_eq!(
                decode_tx_spacing_payload(&spacing_ms.to_be_bytes()),
                Some(spacing_ms)
            );
            assert_eq!(
                classify_control_frame(&encode_tx_spacing(spacing_ms), ACCEPTED),
                ControlAction::TxSpacing(spacing_ms)
            );
        }
    }

    #[test]
    fn the_spacing_payload_is_two_bytes_big_endian() {
        // The byte order is the contract a non-Rust host would implement
        // against, so it is asserted on the bytes and not on a round trip.
        let bytes = encode_tx_spacing(0x1234);
        assert_eq!(&bytes[ENVELOPE_HEADER_LEN..], &[0x12, 0x34]);
    }

    #[test]
    fn a_spacing_payload_of_the_wrong_size_is_refused_as_malformed() {
        for payload in [vec![], vec![0x00], vec![0x00, 0x3C, 0x00]] {
            assert_eq!(
                classify_control_frame(&encode_frame(TYPE_TX_SPACING, &payload), ACCEPTED),
                ControlAction::Refuse {
                    refused_type: TYPE_TX_SPACING,
                    reason: REFUSE_MALFORMED
                },
                "payload {payload:?} was not refused"
            );
        }
    }

    #[test]
    fn the_spacing_frame_and_the_telemetry_frames_cannot_be_confused() {
        // The control at the one place the knob and the reporter meet: a
        // spacing frame must never turn into a telemetry command, and a
        // telemetry command must never turn into a spacing.
        assert!(matches!(
            classify_control_frame(&encode_tx_spacing(60), ACCEPTED),
            ControlAction::TxSpacing(_)
        ));
        assert!(matches!(
            classify_control_frame(&encode_telemetry_clear(), ACCEPTED),
            ControlAction::TelemetryTarget(_)
        ));
        // And the profile ids, which are the reporter's whole vocabulary,
        // are not spacing values in disguise: the two payloads differ in
        // length as well as in type.
        assert_ne!(TYPE_TX_SPACING, TYPE_TELEMETRY_TARGET);
        assert_ne!(
            encode_tx_spacing(TELEMETRY_PROFILE_STATION as u16).len(),
            encode_telemetry_clear().len()
        );
    }

    #[test]
    fn firmware_without_the_spacing_knob_refuses_it_by_name() {
        // A board flashed before #345 answers what is missing instead of
        // going quiet, so a sweep learns immediately that it is talking to
        // the wrong image.
        assert_eq!(
            classify_control_frame(&encode_tx_spacing(60), ACCEPTED_PRE_236),
            ControlAction::Refuse {
                refused_type: TYPE_TX_SPACING,
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    #[test]
    fn the_announce_frame_classifies_and_stays_under_a_packet() {
        let bytes = encode_announce();
        // Under the 19-byte minimum Reticulum packet, like every other
        // empty-payload command: firmware that does not know the type can
        // never take it for a packet.
        assert!(bytes.len() < 19);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::AnnounceNow
        );
    }

    #[test]
    fn an_announce_frame_with_a_payload_is_refused_as_malformed() {
        assert_eq!(
            classify_control_frame(&encode_frame(TYPE_ANNOUNCE, &[0x01]), ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_ANNOUNCE,
                reason: REFUSE_MALFORMED
            }
        );
    }

    #[test]
    fn firmware_without_the_announce_command_refuses_it_by_name() {
        assert_eq!(
            classify_control_frame(&encode_announce(), ACCEPTED_PRE_236),
            ControlAction::Refuse {
                refused_type: TYPE_ANNOUNCE,
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    #[test]
    fn the_ble_tx_gap_frame_round_trips_and_classifies() {
        let bytes = encode_ble_tx_gap(20);
        assert!(bytes.len() < 19);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::BleTxGap(20)
        );
    }

    #[test]
    fn the_ble_tx_gap_payload_is_two_bytes_big_endian() {
        // The byte order is the contract a non-Rust host would implement
        // against, so it is asserted on the bytes and not on a round trip.
        let bytes = encode_ble_tx_gap(0x1234);
        assert_eq!(&bytes[ENVELOPE_HEADER_LEN..], &[0x12, 0x34]);
    }

    #[test]
    fn zero_and_the_bound_are_legal_gaps_and_above_the_bound_is_refused_by_value() {
        // 0 is the default ("impose nothing"), not an absent value, and
        // the bound itself is still a legal experiment.
        for gap_ms in [0u16, 1, 20, 500, BLE_TX_GAP_MAX_MS] {
            assert_eq!(
                classify_control_frame(&encode_ble_tx_gap(gap_ms), ACCEPTED),
                ControlAction::BleTxGap(gap_ms)
            );
        }
        // One past the bound and the top of the wire type: both are a
        // typo, not an experiment, and both get the named value refusal
        // rather than a wedged-looking link.
        for gap_ms in [BLE_TX_GAP_MAX_MS + 1, u16::MAX] {
            assert_eq!(
                classify_control_frame(&encode_ble_tx_gap(gap_ms), ACCEPTED),
                ControlAction::Refuse {
                    refused_type: TYPE_BLE_TX_GAP,
                    reason: REFUSE_VALUE
                }
            );
        }
    }

    #[test]
    fn a_ble_tx_gap_payload_of_the_wrong_size_is_refused_as_malformed() {
        for payload in [vec![], vec![0x00], vec![0x00, 0x14, 0x00]] {
            assert_eq!(
                classify_control_frame(&encode_frame(TYPE_BLE_TX_GAP, &payload), ACCEPTED),
                ControlAction::Refuse {
                    refused_type: TYPE_BLE_TX_GAP,
                    reason: REFUSE_MALFORMED
                },
                "payload {payload:?} was not refused"
            );
        }
    }

    #[test]
    fn firmware_without_the_ble_tx_gap_knob_refuses_it_by_name() {
        assert_eq!(
            classify_control_frame(&encode_ble_tx_gap(20), ACCEPTED_PRE_236),
            ControlAction::Refuse {
                refused_type: TYPE_BLE_TX_GAP,
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    #[test]
    fn the_wall_time_frame_round_trips_and_classifies() {
        let bytes = encode_wall_time(1_790_000_000);
        // Every pre-handshake frame must be shorter than the 19-byte
        // minimum Reticulum packet, so old firmware can never take it
        // for one.
        assert!(bytes.len() < 19);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::WallTime(1_790_000_000)
        );
    }

    #[test]
    fn a_wall_time_payload_of_the_wrong_size_is_refused_as_malformed() {
        let bytes = encode_frame(TYPE_WALL_TIME, &[0u8; 4]);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_WALL_TIME,
                reason: REFUSE_MALFORMED
            }
        );
    }

    #[test]
    fn the_capability_query_and_report_round_trip() {
        let query = encode_capability_query();
        assert!(query.len() < 19);
        assert_eq!(
            classify_control_frame(&query, ACCEPTED),
            ControlAction::CapabilityQuery
        );
        let report = encode_capability_report(ACCEPTED);
        let frame = decode_frame(&report).unwrap();
        assert_eq!(frame.frame_type, TYPE_CAPABILITY_REPORT);
        let (version, accepted) = decode_capability_report_payload(frame.payload).unwrap();
        assert_eq!(version, ENVELOPE_VERSION);
        assert_eq!(accepted, ACCEPTED);
    }

    #[test]
    fn ack_and_refusal_payloads_round_trip() {
        let ack = encode_ack(TYPE_WALL_TIME);
        let frame = decode_frame(&ack).unwrap();
        assert_eq!(frame.frame_type, TYPE_ACK);
        assert_eq!(decode_ack_payload(frame.payload), Some(TYPE_WALL_TIME));
        assert_eq!(decode_ack_payload(&[]), None);

        let refusal = encode_refusal(TYPE_WALL_TIME, REFUSE_VALUE);
        let frame = decode_frame(&refusal).unwrap();
        assert_eq!(frame.frame_type, TYPE_REFUSAL);
        assert_eq!(
            decode_refusal_payload(frame.payload),
            Some((TYPE_WALL_TIME, REFUSE_VALUE))
        );
        assert_eq!(decode_refusal_payload(&[1]), None);
    }

    #[test]
    fn the_envelope_radio_config_and_the_legacy_magic_parse_to_the_same_config() {
        let cfg = sample_config();
        let enveloped = encode_radio_config(&cfg);
        let legacy = crate::rnode::build_radio_config_frame(&cfg);
        let from_envelope = match classify_control_frame(&enveloped, ACCEPTED) {
            ControlAction::RadioConfig(parsed) => parsed,
            other => panic!("envelope config classified as {other:?}"),
        };
        let from_legacy = match classify_control_frame(&legacy, ACCEPTED) {
            ControlAction::LegacyRadioConfig(parsed) => parsed,
            other => panic!("legacy config classified as {other:?}"),
        };
        assert_eq!(from_envelope, from_legacy);
        assert_eq!(from_envelope, cfg);
    }

    /// The query is short enough that pre-envelope firmware ignores it, and
    /// the report it is answered with decodes back to the settings that went
    /// in — the property `--set-tx-power` depends on when it changes one
    /// field and sends the rest back untouched.
    #[test]
    fn a_radio_query_is_answered_with_a_report_that_round_trips() {
        let query = encode_radio_query();
        assert!(query.len() < 19);
        assert_eq!(
            classify_control_frame(&query, ACCEPTED),
            ControlAction::RadioQuery
        );

        let cfg = sample_config();
        let report = encode_radio_report(&cfg);
        let frame = decode_frame(&report).unwrap();
        assert_eq!(frame.frame_type, TYPE_RADIO_REPORT);
        assert_eq!(decode_radio_report_payload(frame.payload), Some(cfg));
    }

    /// A report, with one field changed, is a valid config frame.
    ///
    /// This is the whole read-modify-write contract in one assertion: if the
    /// two codecs ever diverge, a sweep would set the power and move something
    /// else at the same time, and the numbers would look like power.
    #[test]
    fn a_report_with_one_field_changed_is_a_config_the_board_takes() {
        let mut cfg = sample_config();
        let report = encode_radio_report(&cfg);
        let mut echoed = decode_radio_report_payload(decode_frame(&report).unwrap().payload)
            .expect("the report decodes");
        echoed.tx_power_dbm = -9;
        let back = encode_radio_config(&echoed);
        match classify_control_frame(&back, ACCEPTED) {
            ControlAction::RadioConfig(parsed) => {
                assert_eq!(parsed.tx_power_dbm, -9);
                cfg.tx_power_dbm = -9;
                assert_eq!(parsed, cfg, "a field other than the power moved");
            }
            other => panic!("the echoed config classified as {other:?}"),
        }
    }

    /// A query carrying a payload is malformed, not a query with junk after
    /// it. Same rule as the capability query beside it.
    #[test]
    fn a_radio_query_with_a_payload_is_refused_by_name() {
        let framed = encode_frame(TYPE_RADIO_QUERY, &[0x00]);
        assert_eq!(
            classify_control_frame(&framed, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_RADIO_QUERY,
                reason: REFUSE_MALFORMED,
            }
        );
    }

    /// Firmware that does not list the type refuses it by name rather than
    /// timing out, so a host can tell "too old" from "not answering".
    #[test]
    fn a_board_without_the_query_refuses_it_by_name() {
        assert_eq!(
            classify_control_frame(&encode_radio_query(), ACCEPTED_PRE_236),
            ControlAction::Refuse {
                refused_type: TYPE_RADIO_QUERY,
                reason: REFUSE_UNKNOWN_TYPE,
            }
        );
    }

    #[test]
    fn the_legacy_reset_magic_still_classifies_as_reset() {
        assert_eq!(
            classify_control_frame(&RADIO_RESET_FRAME, ACCEPTED),
            ControlAction::LegacyReset
        );
        let enveloped = encode_reset();
        assert!(enveloped.len() < 19);
        assert_eq!(
            classify_control_frame(&enveloped, ACCEPTED),
            ControlAction::Reset
        );
    }

    #[test]
    fn an_invalid_legacy_config_keeps_its_legacy_silence() {
        // 21 bytes, right magic, impossible spreading factor.
        let mut bytes = crate::rnode::build_radio_config_frame(&sample_config());
        bytes[10] = 42; // sf byte
        assert_eq!(bytes.len(), RADIO_CONFIG_FRAME_LEN);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::LegacyRadioConfigInvalid
        );
    }

    #[test]
    fn an_invalid_envelope_config_is_refused_audibly_unlike_the_legacy_path() {
        let mut bytes = encode_radio_config(&sample_config());
        bytes[ENVELOPE_HEADER_LEN + 8] = 42; // sf byte inside the payload
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_RADIO_CONFIG,
                reason: REFUSE_MALFORMED
            }
        );
    }

    #[test]
    fn a_telemetry_target_without_a_key_round_trips() {
        let target = TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_STATION,
            dest_hash: [0xAB; TRUNCATED_HASHBYTES],
            public_key: None,
        };
        let bytes = encode_telemetry_target(&target);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.frame_type, TYPE_TELEMETRY_TARGET);
        assert_eq!(frame.payload.len(), 1 + TRUNCATED_HASHBYTES + 1);
        assert_eq!(decode_telemetry_target_payload(frame.payload), Some(target));
    }

    #[test]
    fn a_telemetry_target_with_a_key_round_trips() {
        let target = TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_TRACKER,
            dest_hash: [0xCD; TRUNCATED_HASHBYTES],
            public_key: Some([0x42; IDENTITY_KEY_SIZE]),
        };
        let bytes = encode_telemetry_target(&target);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(
            frame.payload.len(),
            1 + TRUNCATED_HASHBYTES + 1 + IDENTITY_KEY_SIZE
        );
        assert_eq!(decode_telemetry_target_payload(frame.payload), Some(target));
    }

    #[test]
    fn the_key_present_flag_is_explicit_never_length_guessing() {
        let target = TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_TRACKER,
            dest_hash: [0x01; TRUNCATED_HASHBYTES],
            public_key: Some([0x02; IDENTITY_KEY_SIZE]),
        };
        let bytes = encode_telemetry_target(&target);
        let mut payload = bytes[ENVELOPE_HEADER_LEN..].to_vec();

        // Flag says absent but a key follows: malformed.
        payload[1 + TRUNCATED_HASHBYTES] = 0x00;
        assert_eq!(decode_telemetry_target_payload(&payload), None);

        // Flag says present but the key is truncated: malformed.
        payload[1 + TRUNCATED_HASHBYTES] = 0x01;
        payload.pop();
        assert_eq!(decode_telemetry_target_payload(&payload), None);

        // A flag value that is neither 0 nor 1: malformed.
        let mut hash_only = vec![TELEMETRY_PROFILE_TRACKER];
        hash_only.extend_from_slice(&[0x01; TRUNCATED_HASHBYTES]);
        hash_only.push(0x02);
        assert_eq!(decode_telemetry_target_payload(&hash_only), None);
    }

    #[test]
    fn a_reader_skips_an_unknown_frame_without_losing_the_stream() {
        // Two frames back to back through the HDLC deframer: an unknown
        // type, then a wall time. The reader refuses the first by name and
        // still decodes the second — nothing about the unknown frame
        // desynchronised the stream.
        use crate::framing::hdlc::{frame as hdlc_frame, DeframeResult, Deframer};
        let mut stream = Vec::new();
        hdlc_frame(&encode_frame(0x5A, &[9, 9, 9]), &mut stream);
        let mut second = Vec::new();
        hdlc_frame(&encode_wall_time(1_790_000_000), &mut second);
        stream.extend_from_slice(&second);

        let mut deframer = Deframer::new();
        let actions: Vec<ControlAction> = deframer
            .process(&stream)
            .into_iter()
            .filter_map(|r| match r {
                DeframeResult::Frame(data) => Some(classify_control_frame(&data, ACCEPTED)),
                _ => None,
            })
            .collect();
        assert_eq!(
            actions,
            vec![
                ControlAction::Refuse {
                    refused_type: 0x5A,
                    reason: REFUSE_UNKNOWN_TYPE
                },
                ControlAction::WallTime(1_790_000_000)
            ]
        );
    }

    // -----------------------------------------------------------------
    // Media profile
    // -----------------------------------------------------------------

    /// Every profile the two bits can spell, so no test below can pass by
    /// accident on the one shape that happens to be the default.
    const EVERY_PROFILE: [MediaProfileWire; 4] = [
        MediaProfileWire {
            lora_enabled: true,
            ble_enabled: true,
        },
        MediaProfileWire {
            lora_enabled: true,
            ble_enabled: false,
        },
        MediaProfileWire {
            lora_enabled: false,
            ble_enabled: true,
        },
        MediaProfileWire {
            lora_enabled: false,
            ble_enabled: false,
        },
    ];

    #[test]
    fn every_media_profile_round_trips_and_classifies() {
        for profile in EVERY_PROFILE {
            let bytes = encode_media_profile(&profile);
            // Under the 19-byte minimum Reticulum packet, like every other
            // short command: firmware that does not know the type can
            // never take it for a packet.
            assert!(bytes.len() < 19);
            assert_eq!(
                classify_control_frame(&bytes, ACCEPTED),
                ControlAction::MediaProfile(profile)
            );
        }
    }

    #[test]
    fn the_default_profile_is_both_carriers_on() {
        // The whole compatibility claim of this feature: absence of a
        // profile must change nothing about a fielded board.
        const _: () = {
            assert!(MediaProfileWire::BOTH.lora_enabled);
            assert!(MediaProfileWire::BOTH.ble_enabled);
        };
        assert_eq!(MediaProfileWire::BOTH.flags(), MEDIA_FLAGS_KNOWN);
    }

    #[test]
    fn the_flag_bits_are_the_documented_ones() {
        // The bit positions are the interface periculum and the store
        // record both read; a swap here would silently invert a
        // measurement's declared medium.
        assert_eq!(
            MediaProfileWire {
                lora_enabled: true,
                ble_enabled: false
            }
            .flags(),
            0b01
        );
        assert_eq!(
            MediaProfileWire {
                lora_enabled: false,
                ble_enabled: true
            }
            .flags(),
            0b10
        );
    }

    #[test]
    fn a_flag_byte_with_an_unknown_carrier_is_malformed_not_reinterpreted() {
        // A frame from a host that knows a third carrier. Masking the
        // unknown bit away would answer "that carrier is off", which is a
        // claim about a medium this firmware cannot make.
        let mut bytes = encode_media_profile(&MediaProfileWire::BOTH);
        bytes[ENVELOPE_HEADER_LEN] |= 0b0000_0100;
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_MEDIA_PROFILE,
                reason: REFUSE_MALFORMED
            }
        );
        assert_eq!(decode_media_profile_payload(&[0b1000_0000]), None);
    }

    #[test]
    fn a_media_payload_of_the_wrong_length_is_malformed() {
        for payload in [vec![], vec![0x01, 0x02]] {
            assert_eq!(
                classify_control_frame(&encode_frame(TYPE_MEDIA_PROFILE, &payload), ACCEPTED),
                ControlAction::Refuse {
                    refused_type: TYPE_MEDIA_PROFILE,
                    reason: REFUSE_MALFORMED
                }
            );
        }
    }

    #[test]
    fn the_media_query_is_empty_and_classifies_read_only() {
        assert_eq!(
            classify_control_frame(&encode_media_query(), ACCEPTED),
            ControlAction::MediaQuery
        );
        assert_eq!(
            classify_control_frame(&encode_frame(TYPE_MEDIA_QUERY, &[0x00]), ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_MEDIA_QUERY,
                reason: REFUSE_MALFORMED
            }
        );
    }

    #[test]
    fn the_media_report_carries_running_and_configured_separately() {
        // The "takes effect at reboot" fact, on the wire: BLE is still up
        // because its task was spawned at boot, but a reboot would come up
        // without it.
        let running = MediaProfileWire::BOTH;
        let configured = MediaProfileWire {
            lora_enabled: true,
            ble_enabled: false,
        };
        let bytes = encode_media_report(&running, &configured);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.frame_type, TYPE_MEDIA_REPORT);
        assert_eq!(
            decode_media_report_payload(frame.payload),
            Some((running, configured))
        );
    }

    #[test]
    fn a_media_report_with_an_unknown_carrier_bit_is_not_decoded() {
        assert_eq!(decode_media_report_payload(&[0b0000_0100, 0b11]), None);
        assert_eq!(decode_media_report_payload(&[0b11]), None);
    }

    #[test]
    fn a_binary_without_the_media_gate_refuses_the_profile_it_would_ignore() {
        // The fc60b95 capability rule on the third frame. Acking a profile
        // a binary does not honour is worse here than anywhere else: the
        // ack is what a measurement run reads as "this node is now
        // single-medium", and a masked delivery is exactly what the
        // profile exists to prevent.
        for delivered in [false, true] {
            assert_eq!(
                decode_refusal_payload(
                    &media_profile_answer(
                        false,
                        delivered,
                        Persist::Durable,
                        MediaProfileWire::BOTH,
                        MediaProfileWire::BOTH
                    )[ENVELOPE_HEADER_LEN..]
                ),
                Some((TYPE_MEDIA_PROFILE, REFUSE_UNSUPPORTED))
            );
        }
        assert_eq!(
            decode_refusal_payload(
                &media_query_answer(false, MediaProfileWire::BOTH, MediaProfileWire::BOTH)
                    [ENVELOPE_HEADER_LEN..]
            ),
            Some((TYPE_MEDIA_QUERY, REFUSE_UNSUPPORTED))
        );
    }

    #[test]
    fn an_accepted_media_profile_is_answered_with_the_report_not_an_ack() {
        let running = MediaProfileWire {
            lora_enabled: true,
            ble_enabled: false,
        };
        let answer = media_profile_answer(true, true, Persist::Durable, running, running);
        let frame = decode_frame(&answer).unwrap();
        assert_eq!(frame.frame_type, TYPE_MEDIA_REPORT);
        assert_eq!(
            decode_media_report_payload(frame.payload),
            Some((running, running))
        );
        // An undelivered frame is a full channel, not a refusal of the
        // value: the host retries.
        assert_eq!(
            decode_refusal_payload(
                &media_profile_answer(true, false, Persist::Durable, running, running)
                    [ENVELOPE_HEADER_LEN..]
            ),
            Some((TYPE_MEDIA_PROFILE, REFUSE_BUSY))
        );
    }

    /// Codeberg #358 on the record that caught it. The report is the
    /// board's statement about what a reboot would come up with, so a
    /// profile that never reached the page must not be answered with one
    /// — that report was true about RAM and false about the reboot, three
    /// times on the rig.
    #[test]
    fn a_profile_that_did_not_reach_flash_is_refused_rather_than_reported() {
        let running = MediaProfileWire {
            lora_enabled: true,
            ble_enabled: false,
        };
        let answer = media_profile_answer(true, true, Persist::Lost, running, running);
        assert_eq!(decode_frame(&answer).unwrap().frame_type, TYPE_REFUSAL);
        assert_eq!(
            decode_refusal_payload(&answer[ENVELOPE_HEADER_LEN..]),
            Some((TYPE_MEDIA_PROFILE, REFUSE_PERSIST))
        );
    }

    #[test]
    fn firmware_without_the_media_frames_refuses_them_by_name() {
        // How a media-aware host detects an older board: named refusals,
        // not timeouts — the #236 detection path on the new types.
        for bytes in [
            encode_media_profile(&MediaProfileWire::BOTH),
            encode_media_query(),
        ] {
            let refused_type = decode_frame(&bytes).unwrap().frame_type;
            assert_eq!(
                classify_control_frame(&bytes, ACCEPTED_PRE_236),
                ControlAction::Refuse {
                    refused_type,
                    reason: REFUSE_UNKNOWN_TYPE
                }
            );
        }
    }

    #[test]
    fn the_media_store_record_carries_what_the_wire_carried() {
        // The boot activation path: what a host set over the envelope is
        // what the next boot reads back off the page. Two codecs, one
        // meaning — asserted here because the firmware that joins them is
        // not host-testable.
        use crate::media_profile_store::{decode_media_profile, encode_media_profile as store};
        for profile in EVERY_PROFILE {
            let arrived = match classify_control_frame(&encode_media_profile(&profile), ACCEPTED) {
                ControlAction::MediaProfile(p) => p,
                other => panic!("{other:?}"),
            };
            assert_eq!(decode_media_profile(&store(&arrived)), Some(profile));
        }
    }

    #[test]
    fn the_envelope_magic_is_disjoint_from_the_legacy_magic_and_packet_space() {
        // The legacy classifier keys on [0xA4, 0xA4]; the envelope must
        // never alias it, or a 21-byte envelope frame could be read as a
        // legacy config.
        assert_ne!(ENVELOPE_MAGIC, RADIO_CONFIG_MAGIC);
        assert_eq!(ENVELOPE_MAGIC[0], RADIO_CONFIG_MAGIC[0]);
        // First byte keeps the IFAC bit set — the property the legacy
        // magics rely on to stay out of packet space on a no-IFAC channel.
        assert_eq!(ENVELOPE_MAGIC[0] & 0x80, 0x80);
    }
    // -----------------------------------------------------------------
    // Position-source query (the telemetry send condition's second clause)
    // -----------------------------------------------------------------

    #[test]
    fn the_position_source_query_is_empty_and_classifies_read_only() {
        assert_eq!(
            classify_control_frame(&encode_position_source_query(), ACCEPTED),
            ControlAction::PositionSourceQuery
        );
        // Five bytes, so firmware from before the envelope drops it rather
        // than reading it as a Reticulum packet.
        let query = encode_position_source_query();
        assert_eq!(query.len(), ENVELOPE_HEADER_LEN);
        // A payload where none belongs is malformed, not ignored.
        assert_eq!(
            classify_control_frame(&encode_frame(TYPE_POSITION_SOURCE_QUERY, &[0x00]), ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_POSITION_SOURCE_QUERY,
                reason: REFUSE_MALFORMED
            }
        );
    }

    #[test]
    fn a_position_source_report_round_trips_every_flag_combination() {
        for flags in [
            0,
            POSITION_SOURCE_FIXED,
            POSITION_SOURCE_GNSS,
            POSITION_SOURCE_FIXED | POSITION_SOURCE_GNSS,
        ] {
            let bytes = encode_position_source_report(flags);
            let frame = decode_frame(&bytes).unwrap();
            assert_eq!(frame.frame_type, TYPE_POSITION_SOURCE_REPORT);
            assert_eq!(
                decode_position_source_report_payload(frame.payload),
                Some(flags)
            );
        }
    }

    /// A newer board with a third source must not read as malformed to an
    /// older host: it only ever asks "is anything set", and any non-zero
    /// value answers that.
    #[test]
    fn an_unknown_source_bit_is_carried_through_rather_than_refused() {
        let report = encode_position_source_report(0b0000_0100);
        let frame = decode_frame(&report).unwrap();
        assert_eq!(
            decode_position_source_report_payload(frame.payload),
            Some(0b0000_0100)
        );
        assert_eq!(decode_position_source_report_payload(&[]), None);
        assert_eq!(decode_position_source_report_payload(&[0x01, 0x02]), None);
    }

    /// A binary with no reporter refuses by name rather than answering
    /// "no sources" — which would read as a promise that setting one would
    /// make the board report. The [`telemetry_target_answer`] rule, on the
    /// query direction.
    #[test]
    fn a_reporterless_binary_refuses_the_query_instead_of_answering_zero() {
        let refusal = position_source_query_answer(false, 0);
        let frame = decode_frame(&refusal).unwrap();
        assert_eq!(frame.frame_type, TYPE_REFUSAL);
        assert_eq!(
            decode_refusal_payload(frame.payload),
            Some((TYPE_POSITION_SOURCE_QUERY, REFUSE_UNSUPPORTED))
        );

        // The control: with a reporter, the same zero is a report.
        let report = position_source_query_answer(true, 0);
        let frame = decode_frame(&report).unwrap();
        assert_eq!(frame.frame_type, TYPE_POSITION_SOURCE_REPORT);
        assert_eq!(
            decode_position_source_report_payload(frame.payload),
            Some(0)
        );
    }

    /// Firmware from before the query refuses it by name, which is how a
    /// host learns to say nothing about position sources at all.
    #[test]
    fn a_board_without_the_position_source_query_refuses_it_by_name() {
        assert_eq!(
            classify_control_frame(&encode_position_source_query(), ACCEPTED_PRE_236),
            ControlAction::Refuse {
                refused_type: TYPE_POSITION_SOURCE_QUERY,
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    // -----------------------------------------------------------------
    // Node name (Codeberg #235)
    // -----------------------------------------------------------------

    fn name(text: &str) -> NodeName {
        NodeName::parse(text.as_bytes()).unwrap()
    }

    #[test]
    fn a_node_name_round_trips_and_classifies() {
        for text in ["Balkon-Nord", "Küche", "a", &"x".repeat(NODE_NAME_MAX_LEN)] {
            let n = name(text);
            let bytes = encode_node_name(Some(&n));
            let frame = decode_frame(&bytes).unwrap();
            assert_eq!(frame.frame_type, TYPE_NODE_NAME);
            assert_eq!(decode_node_name_payload(frame.payload), Some(Some(n)));
            assert_eq!(
                classify_control_frame(&bytes, ACCEPTED),
                ControlAction::NodeName(Some(n)),
                "{text}"
            );
        }
    }

    #[test]
    fn the_clear_is_its_own_encoding_and_not_an_empty_name() {
        // "Back to the derived default" has to be sayable, and it must
        // not be spelled as a zero-length name: the board would then have
        // to guess whether a blank means "clear" or "a name I could not
        // read".
        let bytes = encode_node_name(None);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.payload, &[0x00]);
        assert_eq!(decode_node_name_payload(frame.payload), Some(None));
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::NodeName(None)
        );
    }

    #[test]
    fn a_name_the_board_could_not_render_is_refused_rather_than_stored() {
        // The host validates first, but the board is what a *different*
        // host talks to. Over-long, non-UTF-8, control characters and
        // surrounding whitespace are all malformed on the wire, so a
        // board never stores a name it would have to display differently
        // from how it arrived.
        for payload in [
            [&[0x01u8][..], &[b'x'; NODE_NAME_MAX_LEN + 1][..]].concat(),
            vec![0x01, b'a', 0xFF],
            vec![0x01, b'a', b'\n'],
            vec![0x01, b' ', b'a'],
            vec![0x01],       // set with no name
            vec![0x02, b'a'], // a flag byte that is neither
            vec![],           // no flag byte at all
        ] {
            assert_eq!(
                decode_node_name_payload(&payload),
                None,
                "payload {payload:02x?} was accepted"
            );
            assert_eq!(
                classify_control_frame(&encode_frame(TYPE_NODE_NAME, &payload), ACCEPTED),
                ControlAction::Refuse {
                    refused_type: TYPE_NODE_NAME,
                    reason: REFUSE_MALFORMED
                },
                "payload {payload:02x?}"
            );
        }
    }

    #[test]
    fn a_node_name_query_is_empty_and_classifies() {
        let bytes = encode_node_name_query();
        assert_eq!(bytes.len(), ENVELOPE_HEADER_LEN);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::NodeNameQuery
        );
        // A query with a payload is malformed, like every other query.
        assert_eq!(
            classify_control_frame(&encode_frame(TYPE_NODE_NAME_QUERY, &[0x00]), ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_NODE_NAME_QUERY,
                reason: REFUSE_MALFORMED
            }
        );
    }

    #[test]
    fn a_report_carries_both_surfaces_and_their_flags() {
        // The whole point of the report: the two names differ, because
        // the BLE one is truncated and adopted a boot later.
        let mesh = name("Balkon-Nord-Solar");
        let ble = NodeName::decode(b"Balkon-Nord").unwrap();
        let bytes = encode_node_name_report(
            NODE_NAME_FLAG_STORED | NODE_NAME_FLAG_BLE_PENDING,
            &mesh,
            &ble,
        );
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.frame_type, TYPE_NODE_NAME_REPORT);
        let state = decode_node_name_report_payload(frame.payload).unwrap();
        assert_eq!(
            state,
            NodeNameState {
                stored: true,
                ble_pending: true,
                mesh,
                ble,
            }
        );
    }

    #[test]
    fn a_report_of_the_derived_defaults_says_nothing_is_stored() {
        // The two derived defaults are different strings, not a
        // truncation of one another — which is why `ble_pending` is a
        // flag the board sets and not something a host can compute.
        let bytes = encode_node_name_report(
            0,
            &NodeName::decode(b"LNode-a1b2c3d4").unwrap(),
            &NodeName::decode(b"LN-a1b2c3d4").unwrap(),
        );
        let state = decode_node_name_report_payload(decode_frame(&bytes).unwrap().payload).unwrap();
        assert!(!state.stored);
        assert!(!state.ble_pending);
        assert_eq!(state.mesh.as_str(), "LNode-a1b2c3d4");
        assert_eq!(state.ble.as_str(), "LN-a1b2c3d4");
    }

    #[test]
    fn an_unknown_report_flag_is_kept_rather_than_refused() {
        // A newer board that grows a third name surface must not read as
        // malformed to this host, which only asks about two bits.
        let bytes = encode_node_name_report(
            NODE_NAME_FLAG_STORED | 0b1000_0000,
            &name("Balkon"),
            &name("Balkon"),
        );
        let state = decode_node_name_report_payload(decode_frame(&bytes).unwrap().payload).unwrap();
        assert!(state.stored);
        assert!(!state.ble_pending);
    }

    #[test]
    fn an_identity_query_is_empty_and_classifies() {
        let bytes = encode_identity_query();
        assert_eq!(bytes.len(), ENVELOPE_HEADER_LEN);
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED),
            ControlAction::IdentityQuery
        );
        assert_eq!(
            classify_control_frame(&encode_frame(TYPE_IDENTITY_QUERY, &[0x00]), ACCEPTED),
            ControlAction::Refuse {
                refused_type: TYPE_IDENTITY_QUERY,
                reason: REFUSE_MALFORMED
            }
        );
        // Firmware from before this type refuses it by name — how a host
        // detects an old board and prints nothing instead of guessing.
        assert_eq!(
            classify_control_frame(&bytes, ACCEPTED_PRE_236),
            ControlAction::Refuse {
                refused_type: TYPE_IDENTITY_QUERY,
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    #[test]
    fn an_identity_report_round_trips_with_every_flag_shape() {
        let full = IdentityReportWire {
            identity: [0x11; 16],
            probe: Some([0x22; 16]),
            lxmf: Some([0x33; 16]),
        };
        let bytes = encode_identity_report(&full);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.frame_type, TYPE_IDENTITY_REPORT);
        assert_eq!(frame.payload.len(), 49);
        assert_eq!(decode_identity_report_payload(frame.payload), Some(full));

        // A board without a probe responder or LXMF destination still
        // reports its identity; the absent hashes come back as None
        // regardless of what the zeroed field bytes contain.
        let bare = IdentityReportWire {
            identity: [0x44; 16],
            probe: None,
            lxmf: None,
        };
        let frame_bytes = encode_identity_report(&bare);
        let decoded =
            decode_identity_report_payload(decode_frame(&frame_bytes).unwrap().payload).unwrap();
        assert_eq!(decoded, bare);
    }

    #[test]
    fn an_identity_report_tolerates_longer_payloads_but_not_shorter() {
        let report = IdentityReportWire {
            identity: [0x55; 16],
            probe: Some([0x66; 16]),
            lxmf: None,
        };
        let bytes = encode_identity_report(&report);
        let payload = decode_frame(&bytes).unwrap().payload.to_vec();

        // A future firmware that appends a fourth hash must keep reading
        // here (see TYPE_IDENTITY_REPORT).
        let mut longer = payload.clone();
        longer.extend_from_slice(&[0x77; 16]);
        assert_eq!(decode_identity_report_payload(&longer), Some(report));

        let shorter = &payload[..48];
        assert_eq!(decode_identity_report_payload(shorter), None);
    }

    #[test]
    fn an_identity_query_before_the_node_exists_is_busy_not_zeroes() {
        let refusal = identity_query_answer(None);
        let frame = decode_frame(&refusal).unwrap();
        assert_eq!(frame.frame_type, TYPE_REFUSAL);
        assert_eq!(frame.payload, &[TYPE_IDENTITY_QUERY, REFUSE_BUSY]);

        let report = IdentityReportWire {
            identity: [0x01; 16],
            probe: Some([0x02; 16]),
            lxmf: Some([0x03; 16]),
        };
        let answer = identity_query_answer(Some(&report));
        assert_eq!(
            decode_frame(&answer).unwrap().frame_type,
            TYPE_IDENTITY_REPORT
        );
    }

    #[test]
    fn a_truncated_or_overlong_report_is_refused() {
        let good = encode_node_name_report(0, &name("ab"), &name("cd"));
        let payload = decode_frame(&good).unwrap().payload.to_vec();
        for bad in [
            &payload[..payload.len() - 1], // one byte of the ble name missing
            &payload[..1],                 // flags only
            &[][..],                       // nothing at all
        ] {
            assert_eq!(decode_node_name_report_payload(bad), None, "{bad:02x?}");
        }
        // Trailing bytes past the second name are not a name this host
        // can place, so the frame is refused rather than half-read.
        let mut long = payload.clone();
        long.push(0x00);
        assert_eq!(decode_node_name_report_payload(&long), None);
    }

    #[test]
    fn a_binary_without_the_name_gate_refuses_instead_of_acking() {
        // The fc60b95 capability rule on a fifth frame. An operator told
        // "the board is now Balkon-Nord" who then cannot find it under
        // that name on either surface has been lied to, and no retry
        // fixes it.
        let ab = name("ab");
        for answer in [
            node_name_answer(false, Persist::Durable, Some((0, &ab, &ab))),
            node_name_query_answer(false, Some((0, &ab, &ab))),
        ] {
            let frame = decode_frame(&answer).unwrap();
            assert_eq!(frame.frame_type, TYPE_REFUSAL);
            assert_eq!(frame.payload[1], REFUSE_UNSUPPORTED);
        }
        // Each frame is refused under its own type, so a host can tell
        // which of the two conversations was turned down.
        assert_eq!(
            decode_refusal_payload(
                decode_frame(&node_name_answer(
                    false,
                    Persist::Durable,
                    Some((0, &ab, &ab))
                ))
                .unwrap()
                .payload
            ),
            Some((TYPE_NODE_NAME, REFUSE_UNSUPPORTED))
        );
        assert_eq!(
            decode_refusal_payload(
                decode_frame(&node_name_query_answer(false, Some((0, &ab, &ab))))
                    .unwrap()
                    .payload
            ),
            Some((TYPE_NODE_NAME_QUERY, REFUSE_UNSUPPORTED))
        );
    }

    #[test]
    fn a_board_that_cannot_state_its_names_yet_says_busy_and_not_unsupported() {
        // The boot-order window: USB is up several statements into main,
        // the identity only after the LoRa bring-up's awaited SPI
        // transactions, and the derived defaults are built from the
        // identity hash. `busy` is a retry; `unsupported` would tell the
        // host to go and flash different firmware.
        assert_eq!(
            decode_refusal_payload(
                decode_frame(&node_name_answer(true, Persist::Durable, None))
                    .unwrap()
                    .payload
            ),
            Some((TYPE_NODE_NAME, REFUSE_BUSY))
        );
        assert_eq!(
            decode_refusal_payload(
                decode_frame(&node_name_query_answer(true, None))
                    .unwrap()
                    .payload
            ),
            Some((TYPE_NODE_NAME_QUERY, REFUSE_BUSY))
        );
    }

    #[test]
    fn a_name_that_did_not_reach_flash_is_not_reported_as_set() {
        // #358 on the name: the report says what the next boot brings up
        // on both surfaces, so it may not go out before the record is on
        // the page.
        let ab = name("ab");
        let answer = node_name_answer(true, Persist::Lost, Some((0, &ab, &ab)));
        assert_eq!(
            decode_refusal_payload(decode_frame(&answer).unwrap().payload),
            Some((TYPE_NODE_NAME, REFUSE_PERSIST))
        );
    }

    #[test]
    fn a_board_without_the_name_frames_refuses_them_by_name() {
        for bytes in [encode_node_name(None), encode_node_name_query()] {
            let refused = match classify_control_frame(&bytes, ACCEPTED_PRE_236) {
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => (refused_type, reason),
                other => panic!("{other:?}"),
            };
            assert_eq!(refused.1, REFUSE_UNKNOWN_TYPE);
        }
    }
}
