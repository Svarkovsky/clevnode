#![no_main]
//! Fuzz `parse_announce_app_data`, the discovery-announce `app_data` decoder.
//! Network-reachable (announces arrive from any peer). `required_value = 0`
//! makes the proof-of-work stamp check permissive so the fuzzer reaches the
//! integer-keyed msgpack map parse (and `skip_msgpack_value` on unknown keys)
//! rather than bouncing off the stamp gate. Must never panic / overflow / hang.
use leviculum_core::discovery::parse_announce_app_data;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let network_id = [0u8; 16];
    let _ = parse_announce_app_data(data, &network_id, 0);
});
