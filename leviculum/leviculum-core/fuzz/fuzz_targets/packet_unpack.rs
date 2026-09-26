#![no_main]
//! Fuzz `Packet::unpack`, the top-level wire-packet parser every interface
//! feeds attacker-controllable bytes into. Must return `Err` on malformed
//! input, never panic / overflow / hang.
use leviculum_core::packet::{self, Packet};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(pkt) = Packet::unpack(data) {
        // Round-trip and hashing must also stay panic-free on anything that
        // parsed successfully.
        let _ = pkt.data.as_slice();
        let _ = packet::packet_hash(data);
        let _ = packet::get_hashable_part(data);
    }
});
