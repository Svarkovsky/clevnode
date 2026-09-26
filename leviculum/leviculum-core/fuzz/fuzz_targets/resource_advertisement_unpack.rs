#![no_main]
//! Fuzz `ResourceAdvertisement::unpack`, a network-reachable msgpack parser
//! (a peer's resource advertisement arrives over an established link, with no
//! proof-of-work gate). Exercises the hand-rolled msgpack readers and, via the
//! unknown-key branch, `skip_msgpack_value`. Must never panic, overflow, or
//! stack-overflow on nested containers.
use leviculum_core::resource::ResourceAdvertisement;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ResourceAdvertisement::unpack(data);
});
