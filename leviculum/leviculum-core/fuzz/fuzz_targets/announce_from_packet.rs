#![no_main]
//! Fuzz `announce::ReceivedAnnounce::from_packet`, the announce field-slicer
//! that runs on every inbound announce packet (network-reachable from any
//! peer). It slices fixed-size fields (public key, name/random hash, ratchet,
//! signature) out of the payload after an `ANNOUNCE_MIN_SIZE` gate; an
//! off-by-one in those bounds is a remote panic. Must return `Err` on
//! malformed input, never panic / overflow / hang.
use leviculum_core::fuzz::announce_from_bytes;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    announce_from_bytes(data);
});
