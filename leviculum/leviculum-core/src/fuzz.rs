//! Fuzzing-only wrappers that expose crate-internal parsers to the detached
//! `fuzz/` harness. Compiled only under `--cfg fuzzing` (set by cargo-fuzz),
//! so nothing here reaches the normal public API. See `fuzz/README.md`.

use crate::announce::ReceivedAnnounce;
use crate::destination::DestinationType;
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};

/// Drive the announce field-slicer (`ReceivedAnnounce::from_packet`) from
/// arbitrary bytes.
///
/// Two paths, so the parser is reached both as it is on the wire and directly:
///
/// 1. Realistic: `data` is unpacked by `Packet::unpack` and, when that yields
///    an announce, handed to `from_packet` — the exact path an inbound packet
///    takes.
/// 2. Direct: `data` is used verbatim as the announce payload with both the
///    ratcheted and non-ratcheted framings, so the length/offset logic in
///    `from_packet` is exercised even when the outer frame would be rejected.
///    Anything that parses is pushed through `validate` and the accessors to
///    widen coverage.
pub fn announce_from_bytes(data: &[u8]) {
    if let Ok(pkt) = Packet::unpack(data) {
        let _ = ReceivedAnnounce::from_packet(&pkt);
    }

    for context_flag in [false, true] {
        let pkt = Packet {
            flags: PacketFlags {
                ifac_flag: false,
                header_type: HeaderType::Type1,
                context_flag,
                transport_type: TransportType::Broadcast,
                dest_type: DestinationType::Single,
                packet_type: PacketType::Announce,
            },
            hops: 0,
            transport_id: None,
            destination_hash: [0u8; crate::constants::TRUNCATED_HASHBYTES],
            context: PacketContext::None,
            data: PacketData::Owned(data.to_vec()),
        };
        if let Ok(announce) = ReceivedAnnounce::from_packet(&pkt) {
            let _ = announce.validate();
            let _ = announce.app_data_string();
            let _ = announce.computed_destination_hash();
        }
    }
}
