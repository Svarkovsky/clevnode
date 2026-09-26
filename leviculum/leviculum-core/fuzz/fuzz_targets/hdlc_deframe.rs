#![no_main]
//! Fuzz the HDLC deframer, which reassembles frames from a raw serial/TCP byte
//! stream (KISS/HDLC interfaces). Fed arbitrary bytes including flag/escape
//! sequences; must never panic, overflow, or grow memory without bound.
use leviculum_core::framing::hdlc::Deframer;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut deframer = Deframer::new();
    let _ = deframer.process(data);
});
