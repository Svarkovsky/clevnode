//! Fuzzing-only wrappers that expose crate-internal parsers to the detached
//! `fuzz/` harness. Compiled only under `--cfg fuzzing` (set by cargo-fuzz),
//! so nothing here reaches the normal public API. See `fuzz/README.md`.

use crate::interfaces::i2p::sam;

/// Drive the I2P SAM reply-line parser and the base64/destination decoders it
/// feeds from arbitrary bytes.
///
/// The SAM bridge socket is a local trust boundary, but the bytes on it
/// originate from the I2P router and, through it, the wider network; a panic
/// in this parse path takes the interface (and the daemon) down. The input is
/// treated as one reply line: it is parsed with [`sam::Message::parse`], any
/// `DESTINATION=` option is pushed through both destination decoders, and the
/// raw bytes are also handed straight to the base64 decoder. Every path must
/// return `Err` on malformed input, never panic / overflow / hang.
pub fn sam_parse_reply(data: &[u8]) {
    let line = String::from_utf8_lossy(data);

    if let Ok(msg) = sam::Message::parse(&line) {
        if let Some(dest) = msg.get("DESTINATION") {
            let _ = sam::Destination::from_public_base64(dest);
            let _ = sam::Destination::from_private_base64(dest);
        }
    }

    let _ = sam::i2p_b64decode(&line);
}
