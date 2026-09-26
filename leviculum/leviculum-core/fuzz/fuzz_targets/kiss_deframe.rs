#![no_main]
//! Fuzz the KISS deframer, which reassembles frames from a raw serial byte
//! stream (RNode / KISSInterface). Fed arbitrary bytes including FEND/FESC
//! escape sequences; must never panic, overflow, or grow memory without bound.
use leviculum_core::framing::kiss::KissDeframer;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // RNode default payload cap.
    let mut deframer = KissDeframer::with_max_payload(508);
    let _ = deframer.process(data);
});
