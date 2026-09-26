//! What this node reads out of a *peer's* advertisement, and the rule
//! that decides which of the two connects (#255 phase B).
//!
//! [`crate::adv`] is the sending half: the AD structures we put on the
//! air. This is the receiving half: the scanner's parse of a peer's AD
//! structures, and the connection decision built on it. Both halves are
//! pure byte- and integer-work, so they live here and are table-tested
//! on the host; the firmware's scanner task only performs what
//! [`should_initiate`] decides.
//!
//! # The rule (BLE_PROTOCOL_v2.2.md, "Connection Direction (MAC
//! Sorting)", plus the v0.3.0 capability override)
//!
//! v2.2 breaks the "who initiates?" tie deterministically: **the node
//! with the numerically lower address initiates** (v2.2 §Connection
//! Direction: `if my_mac_int < peer_mac_int: connect_to_peer()`), the
//! higher one keeps advertising and waits. v0.3.0 §3.1 overrides the
//! sort when capability flags say one side has no central role:
//!
//! 1. peer is `PERIPHERAL_ONLY`, we are not → **we** initiate,
//!    regardless of how the addresses compare (the peer cannot),
//! 2. we are `PERIPHERAL_ONLY`, the peer is not → the peer initiates,
//! 3. both are `PERIPHERAL_ONLY` → nobody can; log it and stand down,
//! 4. neither is → the v2.2 address sort.
//!
//! Case 1 is also the **address-rotation bypass**: the sort compares
//! whatever address the peer currently advertises, and a phone's RPA
//! rotates on a timescale of minutes, so which way a phone sorts is a
//! coin toss that re-flips every rotation (see [`crate::adv`]'s module
//! docs for the failure this produced). The capability override does
//! not depend on the address at all, which is exactly why it exists.
//!
//! What the sort *cannot* be immune to: a peer we are **already linked
//! to** can rotate its address and reappear in the scanner as a
//! seemingly new device (v2.2 keys everything durable by identity, not
//! address, for this exact reason — §"Why Not Use MAC Addresses as
//! Keys?"). No pre-connection check can catch that, because the
//! advertisement carries no identity; the firmware closes the hole
//! post-connect by reading the Identity characteristic and dropping the
//! link if that identity is already live.
//!
//! # The fallback mode (Codeberg #375)
//!
//! The sort alone strands the high addresses of a room: a board that
//! outranks every advertising neighbour gets only `wait` verdicts, and
//! once the boards above it have gone dark (all incoming slots in
//! sessions — a full board does not advertise, #372) it scans forever
//! with no permitted target, so ten boards form one connected BLE graph
//! only ~79 % of the time. [`ScanMode::Fallback`] is the escape: after
//! the central task has searched for a bounded time without one
//! `initiate` verdict, the sort's `wait` becomes
//! [`ConnectDecision::InitiateFallback`] and any advertising Reticulum
//! peer is a target. The cycles this can close are harmless (Reticulum
//! dedups by packet hash); a duplicate link to an already-linked
//! identity is refused post-connect by the registry, exactly like a
//! rotated-address reconnect. The rule stays pure: whether the bound
//! has elapsed is measured by the caller and handed in as the mode,
//! never measured here.

use crate::adv::{COMPANY_ID, MANUFACTURER_DATA_LEN, PROTOCOL_VERSION};
use crate::CAP_PERIPHERAL_ONLY;

/// AD type 0x06: Incomplete List of 128-bit Service Class UUIDs.
pub const AD_TYPE_SERVICE_UUID128_INCOMPLETE: u8 = 0x06;
/// AD type 0x07: Complete List of 128-bit Service Class UUIDs — what
/// our own advertisement carries (`ServiceList::Complete`).
pub const AD_TYPE_SERVICE_UUID128_COMPLETE: u8 = 0x07;
/// AD type 0xFF: Manufacturer Specific Data — the v0.3.0 capability
/// record's carrier.
pub const AD_TYPE_MANUFACTURER_DATA: u8 = 0xFF;

/// What the scanner learned from one advertising PDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerAdvertisement {
    /// The PDU lists the Reticulum service UUID: this is a Columba
    /// peer, connectable under the protocol.
    pub offers_service: bool,
    /// The v0.3.0 capability flags, when a record with our company ID
    /// and a version we know was present. `None` is a statement too —
    /// a v2.2 peer that never spoke v0.3.0 — and per v0.3.0 §3.2 it
    /// means "assume full capability, fall back to the address sort".
    pub caps: Option<u8>,
}

/// Walk one advertising PDU's AD structures.
///
/// Layout per Core Spec Vol 3 Part C §11: `[len][type][len-1 data
/// bytes]` repeated; a zero length byte terminates early. Anything
/// malformed — a length running past the buffer — ends the walk at the
/// last whole structure rather than guessing: the radio hands us
/// whatever was in the air, and a truncated PDU must parse as "less",
/// never panic.
///
/// `service_uuid_le` is the 128-bit service UUID in the same
/// little-endian byte order the AD structure carries (for ours see
/// `ble::columba`).
#[must_use]
pub fn parse_peer_advertisement(data: &[u8], service_uuid_le: &[u8; 16]) -> PeerAdvertisement {
    let mut parsed = PeerAdvertisement::default();
    let mut at = 0usize;
    while at < data.len() {
        let len = data[at] as usize;
        if len == 0 {
            break;
        }
        let Some(structure) = data.get(at + 1..at + 1 + len) else {
            break;
        };
        let (ad_type, payload) = (structure[0], &structure[1..]);
        match ad_type {
            AD_TYPE_SERVICE_UUID128_COMPLETE | AD_TYPE_SERVICE_UUID128_INCOMPLETE => {
                // The list holds zero or more whole UUIDs; a trailing
                // partial one is ignored, same as any other truncation.
                if payload
                    .chunks_exact(service_uuid_le.len())
                    .any(|uuid| uuid == service_uuid_le)
                {
                    parsed.offers_service = true;
                }
            }
            // v0.3.0 §3.2: company ID must match, version must be at
            // least ours; otherwise the record is someone else's (or
            // from a future we cannot read) and contributes nothing.
            // First matching record wins.
            AD_TYPE_MANUFACTURER_DATA
                if parsed.caps.is_none()
                    && payload.len() >= MANUFACTURER_DATA_LEN
                    && payload[..2] == COMPANY_ID.to_le_bytes()
                    && payload[2] >= PROTOCOL_VERSION =>
            {
                parsed.caps = Some(payload[3]);
            }
            _ => {}
        }
        at += 1 + len;
    }
    parsed
}

/// A BLE address as the number the v2.2 sort compares.
///
/// The spec compares `int(mac.replace(":", ""), 16)` — the displayed
/// `AA:BB:CC:DD:EE:FF` read as one big-endian number. On the wire and
/// in `ble_gap_addr_t::addr` the same six bytes are stored LSB first
/// (`FF` is byte 0), so the numeric value is simply the little-endian
/// integer of the raw bytes. Address *type* bits are not part of the
/// comparison; the spec sorts on the 48-bit value alone.
#[must_use]
pub fn addr_value(addr_le: &[u8; 6]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes[..6].copy_from_slice(addr_le);
    u64::from_le_bytes(bytes)
}

/// Which acceptance regime the caller's search is in (#375).
///
/// The mode is an *input*: the rule never measures time. The firmware's
/// central task tracks how long it has scanned without a single
/// `initiate` verdict and switches to [`Self::Fallback`] when that
/// exceeds its bound; the host simulation does the same in rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    /// The v2.2 sort with the v0.3.0 override, unmodified.
    Strict,
    /// The strict rule has yielded no target for the caller's bound:
    /// the address sort's `wait` becomes
    /// [`ConnectDecision::InitiateFallback`]. Everything else is
    /// unchanged — the capability overrides are physical facts about
    /// who *can* connect, not tie-breaks, and equal addresses stay a
    /// stand-down (that is the self/collision guard, and dialling
    /// yourself is never a fallback).
    Fallback,
}

/// The connection decision, with the rule that produced it — the rule
/// is what the `BLE_SCAN_DECISION` log line and the table tests hold
/// on to, so "right answer, wrong reason" cannot pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectDecision {
    /// v0.3.0 §3.1 case 1: the peer cannot act as central, so the sort
    /// is bypassed and we initiate no matter how the addresses compare.
    InitiatePeripheralOnlyPeer,
    /// v2.2 address sort: our address is the lower one, we initiate.
    InitiateLowerAddress,
    /// v2.2 address sort: the peer's address is the lower one; it
    /// initiates, we keep advertising.
    WaitPeerHasLowerAddress,
    /// #375: the sort says wait, but the caller is in
    /// [`ScanMode::Fallback`] — the strict rule produced no target for
    /// the whole bound, so any advertising Reticulum peer is dialled
    /// rather than none. Its own rule name, so a capture can tell a
    /// fallback dial from a sort win.
    InitiateFallback,
    /// v0.3.0 §3.1 case 2: we are the peripheral-only side, the peer
    /// must come to us.
    WaitWeArePeripheralOnly,
    /// v0.3.0 §3.1 case 3: both sides peripheral-only — no link is
    /// possible and the spec wants that logged, not retried.
    NobodyBothPeripheralOnly,
    /// Equal 48-bit addresses. The v2.2 reference raises an exception
    /// ("should never happen"); a board logs it and stands down.
    NobodyEqualAddresses,
}

impl ConnectDecision {
    /// Whether this side opens the connection.
    #[must_use]
    pub fn initiate(self) -> bool {
        matches!(
            self,
            Self::InitiatePeripheralOnlyPeer | Self::InitiateLowerAddress | Self::InitiateFallback
        )
    }

    /// The rule for the structured log line (no whitespace, stable).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InitiatePeripheralOnlyPeer => "initiate_peer_peripheral_only",
            Self::InitiateLowerAddress => "initiate_lower_address",
            Self::WaitPeerHasLowerAddress => "wait_peer_lower_address",
            Self::InitiateFallback => "initiate_fallback",
            Self::WaitWeArePeripheralOnly => "wait_we_are_peripheral_only",
            Self::NobodyBothPeripheralOnly => "nobody_both_peripheral_only",
            Self::NobodyEqualAddresses => "nobody_equal_addresses",
        }
    }
}

/// Decide who connects, per BLE_PROTOCOL_v2.2.md §"Connection Direction
/// (MAC Sorting)" with the v0.3.0 §3.1 capability override. See the
/// module docs for the four cases and their order.
///
/// `peer_caps` comes from [`parse_peer_advertisement`]; `None` (no
/// readable capability record) is full capability per v0.3.0 §3.2.
/// Addresses are [`addr_value`]s of the two *current* addresses — ours
/// as configured in the SoftDevice, the peer's as it advertises now.
///
/// `mode` is the #375 fallback switch (see [`ScanMode`] and the module
/// docs). It reshapes exactly one outcome: the address sort's
/// [`ConnectDecision::WaitPeerHasLowerAddress`] becomes
/// [`ConnectDecision::InitiateFallback`]. The capability cases are
/// untouched — a peripheral-only side still cannot dial no matter how
/// long it has waited — and equal addresses still stand down, because
/// that case guards against dialling ourselves, not against a tie.
#[must_use]
pub fn should_initiate(
    local_caps: u8,
    local_addr: u64,
    peer_caps: Option<u8>,
    peer_addr: u64,
    mode: ScanMode,
) -> ConnectDecision {
    let local_po = local_caps & CAP_PERIPHERAL_ONLY != 0;
    let peer_po = peer_caps.unwrap_or(0) & CAP_PERIPHERAL_ONLY != 0;
    match (local_po, peer_po) {
        (false, true) => ConnectDecision::InitiatePeripheralOnlyPeer,
        (true, false) => ConnectDecision::WaitWeArePeripheralOnly,
        (true, true) => ConnectDecision::NobodyBothPeripheralOnly,
        (false, false) => {
            if local_addr < peer_addr {
                ConnectDecision::InitiateLowerAddress
            } else if local_addr > peer_addr {
                match mode {
                    ScanMode::Strict => ConnectDecision::WaitPeerHasLowerAddress,
                    ScanMode::Fallback => ConnectDecision::InitiateFallback,
                }
            } else {
                ConnectDecision::NobodyEqualAddresses
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adv::{ad_structure_len, manufacturer_data, ADV_BYTES_USED};

    /// The service UUID in AD byte order, as `ble::columba` carries it.
    /// Restated here from the spec (37145b00-442d-4a94-917f-8f42c5da28e3,
    /// reversed) rather than imported: if the firmware constant ever
    /// drifted, this test would still hold the parser to the wire truth.
    const SERVICE_UUID_LE: [u8; 16] = [
        0xe3, 0x28, 0xda, 0xc5, 0x42, 0x8f, 0x7f, 0x91, 0x94, 0x4a, 0x2d, 0x44, 0x00, 0x5b, 0x14,
        0x37,
    ];

    /// Build the exact PDU our own firmware advertises: flags, complete
    /// 128-bit service list, v0.3.0 capability record.
    fn own_advertisement(caps: u8) -> Vec<u8> {
        let mut pdu = vec![0x02, 0x01, 0x06];
        pdu.extend_from_slice(&[0x11, AD_TYPE_SERVICE_UUID128_COMPLETE]);
        pdu.extend_from_slice(&SERVICE_UUID_LE);
        pdu.extend_from_slice(&[0x05, AD_TYPE_MANUFACTURER_DATA]);
        pdu.extend_from_slice(&manufacturer_data(caps));
        pdu
    }

    #[test]
    fn our_own_advertisement_parses_back_to_what_we_meant() {
        let pdu = own_advertisement(CAP_PERIPHERAL_ONLY);
        assert_eq!(pdu.len(), ADV_BYTES_USED, "the budget adv.rs accounts");
        let parsed = parse_peer_advertisement(&pdu, &SERVICE_UUID_LE);
        assert!(parsed.offers_service);
        assert_eq!(parsed.caps, Some(CAP_PERIPHERAL_ONLY));

        let cleared = parse_peer_advertisement(&own_advertisement(0), &SERVICE_UUID_LE);
        assert_eq!(cleared.caps, Some(0), "phase B: record stays, bit clears");
    }

    #[test]
    fn a_v2_2_peer_without_the_record_reads_as_no_caps() {
        // Flags + service list only — what a pre-v0.3.0 peer advertises.
        let pdu = &own_advertisement(0)[..3 + ad_structure_len(16)];
        let parsed = parse_peer_advertisement(pdu, &SERVICE_UUID_LE);
        assert!(parsed.offers_service);
        assert_eq!(parsed.caps, None);
    }

    #[test]
    fn a_foreign_manufacturer_record_is_not_a_capability_record() {
        // Same shape, Nordic's company ID: v0.3.0 §3.2 requires 0xFFFF.
        let pdu = [0x05, AD_TYPE_MANUFACTURER_DATA, 0x59, 0x00, 0x03, 0x01];
        let parsed = parse_peer_advertisement(&pdu, &SERVICE_UUID_LE);
        assert_eq!(parsed.caps, None);
    }

    #[test]
    fn an_older_version_byte_is_ignored_a_newer_one_is_read() {
        let old = [0x05, AD_TYPE_MANUFACTURER_DATA, 0xFF, 0xFF, 0x02, 0x01];
        assert_eq!(parse_peer_advertisement(&old, &SERVICE_UUID_LE).caps, None);
        // §3.2 says version >= 0x03: a v0.4 record still carries bit 0.
        let newer = [0x05, AD_TYPE_MANUFACTURER_DATA, 0xFF, 0xFF, 0x04, 0x01];
        assert_eq!(
            parse_peer_advertisement(&newer, &SERVICE_UUID_LE).caps,
            Some(0x01)
        );
    }

    #[test]
    fn the_incomplete_uuid_list_type_also_matches() {
        let mut pdu = vec![0x11, AD_TYPE_SERVICE_UUID128_INCOMPLETE];
        pdu.extend_from_slice(&SERVICE_UUID_LE);
        assert!(parse_peer_advertisement(&pdu, &SERVICE_UUID_LE).offers_service);
    }

    #[test]
    fn a_second_uuid_in_the_list_is_still_found() {
        let mut pdu = vec![0x21, AD_TYPE_SERVICE_UUID128_COMPLETE];
        pdu.extend_from_slice(&[0xAA; 16]);
        pdu.extend_from_slice(&SERVICE_UUID_LE);
        assert!(parse_peer_advertisement(&pdu, &SERVICE_UUID_LE).offers_service);
    }

    #[test]
    fn truncated_and_degenerate_pdus_parse_as_less_never_panic() {
        // A length byte running past the end: the walk stops.
        let truncated = [0x11, AD_TYPE_SERVICE_UUID128_COMPLETE, 0xe3, 0x28];
        assert_eq!(
            parse_peer_advertisement(&truncated, &SERVICE_UUID_LE),
            PeerAdvertisement::default()
        );
        // A zero length byte terminates; structures after it are dead.
        let mut early_end = vec![0x00];
        early_end.extend_from_slice(&own_advertisement(0));
        assert_eq!(
            parse_peer_advertisement(&early_end, &SERVICE_UUID_LE),
            PeerAdvertisement::default()
        );
        // Empty and single-byte inputs.
        assert_eq!(
            parse_peer_advertisement(&[], &SERVICE_UUID_LE),
            PeerAdvertisement::default()
        );
        assert_eq!(
            parse_peer_advertisement(&[0x01], &SERVICE_UUID_LE),
            PeerAdvertisement::default()
        );
        // A capability record one byte short contributes nothing.
        let short = [0x04, AD_TYPE_MANUFACTURER_DATA, 0xFF, 0xFF, 0x03];
        assert_eq!(
            parse_peer_advertisement(&short, &SERVICE_UUID_LE).caps,
            None
        );
    }

    #[test]
    fn addr_value_reads_the_raw_bytes_as_the_spec_reads_the_hex_string() {
        // The spec's own example pair (v2.2 §Connection Direction):
        // B8:27:EB:A8:A7:22 = 0xB827EBA8A722, raw storage is LSB first.
        let pi1_raw = [0x22, 0xA7, 0xA8, 0xEB, 0x27, 0xB8];
        let pi2_raw = [0xCD, 0x28, 0x10, 0xEB, 0x27, 0xB8];
        assert_eq!(addr_value(&pi1_raw), 0xB827_EBA8_A722);
        assert_eq!(addr_value(&pi2_raw), 0xB827_EB10_28CD);
        assert!(addr_value(&pi2_raw) < addr_value(&pi1_raw), "Pi2 initiates");
    }

    /// The v2.2 §Connection Direction sort and every v0.3.0 §3.1
    /// override, as one table: local caps × peer caps × address order,
    /// in both scan modes (#375). Fallback rows restate every strict
    /// row: exactly one cell may differ, the sort's wait.
    #[test]
    fn the_decision_table_matches_v2_2_and_the_v0_3_0_override() {
        use ConnectDecision::*;
        use ScanMode::{Fallback, Strict};
        const LOWER: u64 = 0xB827_EB10_28CD;
        const HIGHER: u64 = 0xB827_EBA8_A722;
        const PO: u8 = CAP_PERIPHERAL_ONLY;
        type Row = (u8, u64, Option<u8>, u64, ScanMode, ConnectDecision);
        #[rustfmt::skip]
        let table: &[Row] = &[
            // v2.2 sort, both fully capable (v0.3.0 case 4)…
            (0, LOWER,  Some(0), HIGHER, Strict, InitiateLowerAddress),
            (0, HIGHER, Some(0), LOWER,  Strict, WaitPeerHasLowerAddress),
            // …and identically for a v2.2 peer with no record (§3.2).
            (0, LOWER,  None,    HIGHER, Strict, InitiateLowerAddress),
            (0, HIGHER, None,    LOWER,  Strict, WaitPeerHasLowerAddress),
            // v0.3.0 case 1: peripheral-only peer — the sort says wait,
            // the override says initiate. THE address-rotation bypass.
            (0, HIGHER, Some(PO), LOWER, Strict, InitiatePeripheralOnlyPeer),
            (0, LOWER,  Some(PO), HIGHER, Strict, InitiatePeripheralOnlyPeer),
            // v0.3.0 case 2: we are the peripheral-only side, even
            // where the sort would have had us initiate.
            (PO, LOWER,  Some(0), HIGHER, Strict, WaitWeArePeripheralOnly),
            (PO, HIGHER, None,    LOWER,  Strict, WaitWeArePeripheralOnly),
            // v0.3.0 case 3: deadlock, named as such.
            (PO, LOWER,  Some(PO), HIGHER, Strict, NobodyBothPeripheralOnly),
            // The spec's "should never happen" MAC collision.
            (0, LOWER,  Some(0), LOWER, Strict, NobodyEqualAddresses),
            // Reserved capability bits do not read as PERIPHERAL_ONLY.
            (0, HIGHER, Some(0x02), LOWER, Strict, WaitPeerHasLowerAddress),
            // #375 fallback: THE changed cell — the sort's wait becomes
            // a dial, with its own rule name…
            (0, HIGHER, Some(0), LOWER,  Fallback, InitiateFallback),
            (0, HIGHER, None,    LOWER,  Fallback, InitiateFallback),
            (0, HIGHER, Some(0x02), LOWER, Fallback, InitiateFallback),
            // …a sort win keeps its honest strict name…
            (0, LOWER,  Some(0), HIGHER, Fallback, InitiateLowerAddress),
            (0, LOWER,  Some(PO), HIGHER, Fallback, InitiatePeripheralOnlyPeer),
            // …and no fallback overrides physics or the self-guard: a
            // peripheral-only side still cannot dial, equal addresses
            // still stand down.
            (PO, LOWER,  Some(0), HIGHER, Fallback, WaitWeArePeripheralOnly),
            (PO, HIGHER, Some(PO), LOWER, Fallback, NobodyBothPeripheralOnly),
            (0, LOWER,  Some(0), LOWER, Fallback, NobodyEqualAddresses),
        ];
        for &(lc, la, pc, pa, mode, want) in table {
            assert_eq!(
                should_initiate(lc, la, pc, pa, mode),
                want,
                "local_caps={lc:#x} local={la:#x} peer_caps={pc:?} peer={pa:#x} mode={mode:?}"
            );
        }
    }

    /// Positive control: one flipped address byte flips the decision.
    /// If the comparison stopped comparing (or compared the wrong
    /// operands), the table above could still pass by accident; this
    /// cannot.
    #[test]
    fn control_flipping_one_address_byte_flips_the_decision() {
        let ours = [0x22, 0xA7, 0xA8, 0xEB, 0x27, 0xB8];
        let mut peer = ours;
        peer[5] = 0xB9; // most significant displayed octet: peer higher
        assert_eq!(
            should_initiate(
                0,
                addr_value(&ours),
                Some(0),
                addr_value(&peer),
                ScanMode::Strict
            ),
            ConnectDecision::InitiateLowerAddress
        );
        peer[5] = 0xB7; // now peer lower
        assert_eq!(
            should_initiate(
                0,
                addr_value(&ours),
                Some(0),
                addr_value(&peer),
                ScanMode::Strict
            ),
            ConnectDecision::WaitPeerHasLowerAddress
        );
    }

    /// The rotation scenario from the module docs, end to end: the same
    /// fully-capable peer sorts differently before and after rotating
    /// its address, while the peripheral-only override never moves.
    /// (The local address here is mid-range on purpose so a rotation
    /// can cross it; see the next test for what our real address class
    /// pins down.)
    #[test]
    fn rotation_reflips_the_sort_but_never_the_capability_override() {
        let ours = addr_value(&[0x22, 0xA7, 0xA8, 0xEB, 0x27, 0x50]);
        let rpa_before = addr_value(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x40]);
        let rpa_after = addr_value(&[0x01, 0x00, 0x00, 0x00, 0x00, 0x7F]);
        let sort_before = should_initiate(0, ours, Some(0), rpa_before, ScanMode::Strict);
        let sort_after = should_initiate(0, ours, Some(0), rpa_after, ScanMode::Strict);
        assert_ne!(sort_before, sort_after, "the sort is rotation-unstable");
        assert_eq!(
            should_initiate(
                0,
                ours,
                Some(CAP_PERIPHERAL_ONLY),
                rpa_before,
                ScanMode::Strict
            ),
            should_initiate(
                0,
                ours,
                Some(CAP_PERIPHERAL_ONLY),
                rpa_after,
                ScanMode::Strict
            ),
            "the override is not"
        );
    }

    /// A structural fact the rig acceptance can lean on: an RPA's two
    /// top bits are `01` (Core Spec Vol 6 Part B §1.3.2.2, so its most
    /// significant displayed octet is at most 0x7F), while the
    /// SoftDevice's default static random address has `11` (at least
    /// 0xC0). A rotating phone therefore ALWAYS sorts below an LNode:
    /// under the pure v2.2 sort the phone initiates and the LNode
    /// waits, on every rotation. An LNode only ever initiates toward a
    /// phone through the PERIPHERAL_ONLY override.
    #[test]
    fn any_rpa_sorts_below_any_static_random_address() {
        let static_random_floor = addr_value(&[0x00, 0x00, 0x00, 0x00, 0x00, 0xC0]);
        let rpa_ceiling = addr_value(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]);
        assert!(rpa_ceiling < static_random_floor);
        assert_eq!(
            should_initiate(
                0,
                static_random_floor,
                Some(0),
                rpa_ceiling,
                ScanMode::Strict
            ),
            ConnectDecision::WaitPeerHasLowerAddress,
            "an LNode never wins the sort against a phone's RPA"
        );
    }

    #[test]
    fn initiate_maps_exactly_the_three_initiating_rules() {
        use ConnectDecision::*;
        for d in [
            InitiatePeripheralOnlyPeer,
            InitiateLowerAddress,
            InitiateFallback,
            WaitPeerHasLowerAddress,
            WaitWeArePeripheralOnly,
            NobodyBothPeripheralOnly,
            NobodyEqualAddresses,
        ] {
            assert_eq!(
                d.initiate(),
                matches!(
                    d,
                    InitiatePeripheralOnlyPeer | InitiateLowerAddress | InitiateFallback
                )
            );
            assert!(!d.as_str().contains(char::is_whitespace), "log-safe");
        }
    }
}
