//! What this node puts in its connectable advertisement, and how many
//! of the 31 legacy bytes that costs.
//!
//! The advertisement is the only thing a Columba peer sees before it
//! decides whether to connect, so a capability it has to know *before*
//! connecting belongs here and nowhere else. Today there is exactly one:
//! `PERIPHERAL_ONLY` (BLE protocol v0.3.0).
//!
//! # Why the flag exists
//!
//! Columba v2.2 breaks the "who initiates?" tie between two nodes that
//! can both see each other by comparing BLE addresses: the **lower**
//! address connects, the higher one waits (v2.2 §"Connection Direction
//! (MAC Sorting)": `if my_mac_int < peer_mac_int: connect_to_peer()`;
//! an earlier revision of this comment had the direction backwards). A
//! node that has no central role cannot honour that rule — when its own
//! address sorts below the peer's, the peer waits for it to initiate
//! and it never does, so the link is never made and neither side
//! reports an error. Which way a peer's (random, rotating) address
//! sorts against a mid-range address re-flips on rotation, so the hole
//! shows up as an intermittent "sometimes it just doesn't connect".
//! The full decision rule, and what rotation can and cannot disturb,
//! lives in [`crate::peer`].
//!
//! `PERIPHERAL_ONLY` closes it by saying so in the advertisement: a peer
//! that sees the bit skips the sort and initiates unconditionally. The
//! bit is *cleared* in phase B, when the firmware actually gains the
//! central role and can hold up its end of the sort (#255).
//!
//! # The byte budget
//!
//! A legacy advertising PDU carries 31 bytes of AD structures, and every
//! structure costs 2 bytes of overhead (a length byte and an AD-type
//! byte) on top of its data — see [`ad_structure_len`]. Ours:
//!
//! | AD structure                   | data | total |
//! |--------------------------------|------|-------|
//! | Flags                          |  1   |  3    |
//! | Complete 128-bit service UUIDs | 16   | 18    |
//! | Manufacturer specific data     |  4   |  6    |
//! | **sum**                        |      | **27** |
//!
//! The `LN-<hex8>` device name ([`crate::device_name`]) does **not**
//! compete for these bytes: it goes in the scan response, a second
//! 31-byte PDU (`ble_task`'s `ScannableUndirected` advertisement carries
//! `adv_data` and `scan_data` separately). [`ADV_BYTES_USED`] and the
//! tests below hold the sum against the limit, so the next AD structure
//! anyone adds either fits or fails on the host instead of at
//! `LegacyAdvertisementBuilder::build`'s panic on a board.

/// Bytes of AD structures a legacy advertising or scan-response PDU can
/// carry (`BLE_GAP_ADV_SET_DATA_SIZE_MAX`).
pub const LEGACY_AD_CAPACITY: usize = 31;

/// Total size of an AD structure carrying `data_len` bytes: the data
/// plus a length byte and an AD-type byte.
#[must_use]
pub const fn ad_structure_len(data_len: usize) -> usize {
    data_len + 2
}

/// Bluetooth SIG company identifier `0xFFFF`, reserved for internal and
/// interoperability testing.
///
/// The Columba protocol uses it deliberately: neither project holds an
/// assigned identifier, and `0xFFFF` is the value the SIG set aside for
/// exactly this. It is *not* a placeholder to be replaced later — a
/// change here is a wire-format change on both sides.
pub const COMPANY_ID: u16 = 0xFFFF;

/// Version byte of the advertised capability record: BLE protocol
/// v0.3.0, the version that introduced it.
///
/// A peer that does not recognise the version must ignore the record
/// rather than guess at the bits behind it, which is why the version
/// leads the payload instead of trailing it.
pub const PROTOCOL_VERSION: u8 = 0x03;

/// Capability bit 0: this node has no BLE central role, so the v2.2
/// address-sort rule does not apply to it — connect regardless of how
/// the addresses compare.
pub const CAP_PERIPHERAL_ONLY: u8 = 1 << 0;

/// Length of the manufacturer-data payload: company ID (2, little
/// endian) + version (1) + capability bits (1).
pub const MANUFACTURER_DATA_LEN: usize = 4;

/// The manufacturer-specific data payload advertised with `caps`.
///
/// This is the AD structure's *data*; the length and AD-type bytes in
/// front of it are added by the advertisement builder, so on the wire
/// the structure reads `05 FF FF FF 03 <caps>` — a length field of 5 and
/// [`ad_structure_len`]`(4)` = 6 bytes consumed.
#[must_use]
pub const fn manufacturer_data(caps: u8) -> [u8; MANUFACTURER_DATA_LEN] {
    let [cid_lo, cid_hi] = COMPANY_ID.to_le_bytes();
    [cid_lo, cid_hi, PROTOCOL_VERSION, caps]
}

/// Bytes of the advertising PDU the firmware's three AD structures use.
/// Held against [`LEGACY_AD_CAPACITY`] by the tests below and asserted
/// again at build time in `ble::columba`.
pub const ADV_BYTES_USED: usize =
    ad_structure_len(1) + ad_structure_len(16) + ad_structure_len(MANUFACTURER_DATA_LEN);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DEVICE_NAME_LEN;

    #[test]
    fn the_advertisement_fits_the_legacy_pdu_with_room_to_spare() {
        assert_eq!(ADV_BYTES_USED, 27);
        // Both constants: a const block fails the build, not one test run.
        const { assert!(ADV_BYTES_USED <= LEGACY_AD_CAPACITY) };
        assert_eq!(LEGACY_AD_CAPACITY - ADV_BYTES_USED, 4, "bytes left over");
    }

    #[test]
    fn the_capability_record_costs_six_of_them() {
        // The reviewer's "+5" is the AD *length field* (type byte plus
        // four data bytes); the structure on the wire is one byte more.
        let structure = ad_structure_len(MANUFACTURER_DATA_LEN);
        assert_eq!(structure, 6);
        assert_eq!(MANUFACTURER_DATA_LEN + 1, 5, "the length field's value");
        assert_eq!(ADV_BYTES_USED - structure, 21, "adv before this batch");
    }

    #[test]
    fn the_payload_is_company_id_le_then_version_then_caps() {
        assert_eq!(
            manufacturer_data(CAP_PERIPHERAL_ONLY),
            [0xFF, 0xFF, 0x03, 0x01]
        );
    }

    #[test]
    fn clearing_the_flag_leaves_the_record_and_its_size_alone() {
        // Phase B clears bit 0 rather than dropping the record: a peer
        // that sees no manufacturer data at all cannot tell a v0.3.0
        // node that is central-capable from a pre-v0.3.0 node that never
        // spoke the version, and would sort against the wrong rule.
        let central_capable = manufacturer_data(0);
        assert_eq!(central_capable, [0xFF, 0xFF, 0x03, 0x00]);
        assert_eq!(central_capable[3] & CAP_PERIPHERAL_ONLY, 0);
        assert_eq!(central_capable.len(), MANUFACTURER_DATA_LEN);
    }

    #[test]
    fn the_scan_response_carries_the_name_and_does_not_compete() {
        // Both PDUs are 31 bytes and they are separate; the name's cost
        // is charged to the scan response, which is why the adv budget
        // above does not include it.
        let scan_response = ad_structure_len(DEVICE_NAME_LEN);
        assert_eq!(scan_response, 13);
        assert!(scan_response <= LEGACY_AD_CAPACITY);
    }

    #[test]
    fn every_capability_byte_round_trips_into_the_payload() {
        for caps in 0..=255u8 {
            let data = manufacturer_data(caps);
            assert_eq!(u16::from_le_bytes([data[0], data[1]]), COMPANY_ID);
            assert_eq!(data[2], PROTOCOL_VERSION);
            assert_eq!(data[3], caps);
        }
    }
}
