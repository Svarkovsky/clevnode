#![no_main]
//! Fuzz the I2P SAM reply-line parser (`interfaces::i2p::sam::Message::parse`)
//! and the base64/destination decoders it feeds. The SAM bridge socket carries
//! bytes from the I2P router (and, through it, the network); a panic in this
//! parse path takes the interface and the daemon down. Must return `Err` on
//! malformed input, never panic / overflow / hang.
use leviculum_std::fuzz::sam_parse_reply;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    sam_parse_reply(data);
});
