//! SAM v3 client protocol for the I2P interface.
//!
//! This speaks the SAM (Simple Anonymous Messaging) bridge protocol that an
//! I2P router (i2pd, Java I2P) exposes on TCP 127.0.0.1:7656. It is the exact
//! wire dialect Python Reticulum drives through its bundled `i2plib`
//! (`RNS.vendor.i2plib`): text commands terminated by `\n`, single-line
//! replies, and I2P base64 / base32 destination addressing.
//!
//! Only the STREAM style is implemented, which is all `I2PInterface` needs.
//! The handshake sequence we speak, per role:
//!
//! * client (initiator, one per `peers` entry):
//!   `HELLO VERSION` -> `SESSION CREATE STYLE=STREAM ID=<nick> DESTINATION=TRANSIENT`
//!   on a control socket kept open for the session lifetime, then
//!   `NAMING LOOKUP NAME=<b32>.b32.i2p` to resolve the peer to a full
//!   destination, then `STREAM CONNECT ID=<nick> DESTINATION=<dest> SILENT=false`
//!   on a fresh socket that becomes the raw bidirectional I2P stream.
//!
//! * server (`connectable = yes`):
//!   `HELLO VERSION` -> `SESSION CREATE STYLE=STREAM ID=<nick> DESTINATION=<key>`
//!   with a persistent private key (TRANSIENT on first run, then persisted),
//!   then a loop of `STREAM ACCEPT ID=<nick> SILENT=false` sockets. With
//!   `SILENT=false` SAM sends the connecting peer's full destination as the
//!   first line once a connection arrives, after which the socket carries the
//!   raw stream.
//!
//! Reticulum packets are HDLC framed on the stream in both directions,
//! identical to Python `I2PInterface` (`I2PInterface.py` `process_outgoing` /
//! `read_loop`), so an lnsd I2P link interoperates with an rnsd one.

use std::collections::HashMap;
use std::io;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Default SAM bridge address (matches i2plib `sam.DEFAULT_ADDRESS`).
pub(crate) const DEFAULT_SAM_ADDRESS: &str = "127.0.0.1:7656";

/// SAM protocol version range we advertise (matches i2plib `DEFAULT_MIN_VER` /
/// `DEFAULT_MAX_VER`).
pub(crate) const SAM_MIN_VERSION: &str = "3.1";
pub(crate) const SAM_MAX_VERSION: &str = "3.1";

/// Sentinel destination that asks SAM to generate a fresh transient key.
pub(crate) const TRANSIENT_DESTINATION: &str = "TRANSIENT";

/// Upper bound on a single SAM reply line. SAM replies are short; a
/// `SESSION STATUS` carrying a freshly generated private key is the longest
/// (~900 base64 chars). 8 KiB leaves generous headroom without letting a
/// misbehaving bridge stream unbounded data into a line buffer.
const MAX_SAM_LINE: usize = 8192;

/// Errors from the SAM client layer.
#[derive(Debug)]
pub(crate) enum SamError {
    /// Underlying socket I/O failure.
    Io(io::Error),
    /// The SAM bridge returned a non-OK `RESULT`, carrying the raw result code.
    Result(String),
    /// A reply could not be parsed as a SAM message.
    Protocol(String),
    /// A base64 / base32 / destination decode failure.
    Encoding(String),
}

impl std::fmt::Display for SamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SamError::Io(e) => write!(f, "SAM I/O error: {e}"),
            SamError::Result(r) => write!(f, "SAM returned RESULT={r}"),
            SamError::Protocol(m) => write!(f, "SAM protocol error: {m}"),
            SamError::Encoding(m) => write!(f, "SAM encoding error: {m}"),
        }
    }
}

impl std::error::Error for SamError {}

impl From<io::Error> for SamError {
    fn from(e: io::Error) -> Self {
        SamError::Io(e)
    }
}

// --- I2P base64 / base32 -----------------------------------------------------

/// I2P base64 replaces the standard alphabet's index 62 (`+`) with `-` and
/// index 63 (`/`) with `~` (i2plib `sam.I2P_B64_CHARS`).
const B64_I2P_62: u8 = b'-';
const B64_I2P_63: u8 = b'~';

/// Standard base64 alphabet, with the two I2P substitutions applied. Only the
/// encode path (test-only) needs the forward table; decoding uses `b64_value`.
#[cfg(test)]
const B64_STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg(test)]
fn b64_symbol(idx: u8) -> u8 {
    match idx {
        62 => B64_I2P_62,
        63 => B64_I2P_63,
        _ => B64_STD[idx as usize],
    }
}

fn b64_value(sym: u8) -> Option<u8> {
    match sym {
        B64_I2P_62 => Some(62),
        B64_I2P_63 => Some(63),
        b'+' => Some(62),
        b'/' => Some(63),
        b'A'..=b'Z' => Some(sym - b'A'),
        b'a'..=b'z' => Some(sym - b'a' + 26),
        b'0'..=b'9' => Some(sym - b'0' + 52),
        _ => None,
    }
}

/// Encode bytes with the I2P base64 alphabet (with `=` padding). Only used by
/// the destination round-trip tests and the live loopback test; production only
/// ever decodes destinations (from NAMING LOOKUP / the key file) and re-sends
/// them verbatim.
#[cfg(test)]
pub(crate) fn i2p_b64encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(b64_symbol(((n >> 18) & 0x3f) as u8) as char);
        out.push(b64_symbol(((n >> 12) & 0x3f) as u8) as char);
        if chunk.len() > 1 {
            out.push(b64_symbol(((n >> 6) & 0x3f) as u8) as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(b64_symbol((n & 0x3f) as u8) as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Decode an I2P base64 string (accepts both the `-~` and `+/` alphabets).
pub(crate) fn i2p_b64decode(s: &str) -> Result<Vec<u8>, SamError> {
    let bytes: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        let mut bits = 0u32;
        for &sym in chunk {
            let v = b64_value(sym)
                .ok_or_else(|| SamError::Encoding(format!("invalid base64 symbol {sym:#x}")))?;
            n = (n << 6) | v as u32;
            bits += 6;
        }
        // Each 4-symbol group carries 3 bytes; a trailing 3/2 symbol group
        // carries 2/1 bytes. Shift the accumulated bits down to byte
        // boundaries and emit the high bytes.
        let nbytes = bits / 8;
        n <<= 24 - bits; // left-align into a 24-bit window
        for i in 0..nbytes {
            out.push(((n >> (16 - i * 8)) & 0xff) as u8);
        }
    }
    Ok(out)
}

/// Encode bytes as lowercase RFC 4648 base32 without padding.
///
/// For a 32-byte SHA-256 digest this yields the 52 significant characters that
/// form an I2P `.b32.i2p` label (i2plib `Destination.base32` does
/// `b32encode(hash)[:52].lower()`; the trailing `====` padding it strips is
/// exactly what "without padding" omits here).
pub(crate) fn base32_lower(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for &b in data {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

// --- Destination -------------------------------------------------------------

/// An I2P destination, represented by its binary public destination (keys +
/// certificate). This is all we need to derive a `.b32.i2p` address; the
/// private key, when we have one, is kept as an opaque base64 string by the
/// interface (persisted to the key file) rather than re-encoded from here.
#[derive(Debug, Clone)]
pub(crate) struct Destination {
    /// Binary public destination (keys + certificate).
    pub(crate) data: Vec<u8>,
}

impl Destination {
    /// Parse a public destination from its base64 form (i2plib
    /// `Destination(data)`).
    pub(crate) fn from_public_base64(b64: &str) -> Result<Self, SamError> {
        Ok(Self {
            data: i2p_b64decode(b64)?,
        })
    }

    /// Parse a destination from a base64 private key (i2plib
    /// `Destination(data, has_private_key=True)`).
    ///
    /// The public destination is the leading `387 + cert_len` bytes of the
    /// private key blob, where `cert_len` is the big-endian u16 at offset
    /// 385..387 (the certificate length inside the KeysAndCert structure).
    pub(crate) fn from_private_base64(b64: &str) -> Result<Self, SamError> {
        let raw = i2p_b64decode(b64)?;
        if raw.len() < 387 {
            return Err(SamError::Encoding(format!(
                "private key too short: {} bytes",
                raw.len()
            )));
        }
        let cert_len = u16::from_be_bytes([raw[385], raw[386]]) as usize;
        let dest_len = 387 + cert_len;
        if raw.len() < dest_len {
            return Err(SamError::Encoding(format!(
                "private key shorter than declared destination: {} < {dest_len}",
                raw.len()
            )));
        }
        Ok(Self {
            data: raw[..dest_len].to_vec(),
        })
    }

    /// Full public destination in base64 (what `STREAM CONNECT DESTINATION=`
    /// expects). Only needed by the live-tunnel loopback test, which connects
    /// by full destination instead of a NAMING LOOKUP.
    #[cfg(test)]
    pub(crate) fn public_base64(&self) -> String {
        i2p_b64encode(&self.data)
    }

    /// The 52-character `.b32.i2p` label (without the `.b32.i2p` suffix),
    /// `base32(sha256(dest))` lowercased.
    pub(crate) fn base32(&self) -> String {
        let digest = Sha256::digest(&self.data);
        base32_lower(&digest)
    }
}

// --- SAM message parsing -----------------------------------------------------

/// A parsed SAM reply line, e.g. `SESSION STATUS RESULT=OK DESTINATION=...`.
///
/// The client dispatches on the options (`RESULT`, `DESTINATION`, `VALUE`); the
/// leading `cmd`/`action` verbs are retained for diagnostics and asserted by
/// the parser tests, but the current consumers do not branch on them.
#[derive(Debug, Clone)]
pub(crate) struct Message {
    #[allow(dead_code)]
    pub(crate) cmd: String,
    #[allow(dead_code)]
    pub(crate) action: String,
    pub(crate) opts: HashMap<String, String>,
}

impl Message {
    /// Parse a SAM reply line (i2plib `sam.Message`). Splits into a command
    /// verb, an action, and `KEY=VALUE` options; a bare token becomes a key
    /// mapped to the empty string.
    pub(crate) fn parse(line: &str) -> Result<Self, SamError> {
        let mut parts = line.split_whitespace();
        let cmd = parts
            .next()
            .ok_or_else(|| SamError::Protocol(format!("empty SAM reply: {line:?}")))?
            .to_string();
        let action = parts
            .next()
            .ok_or_else(|| SamError::Protocol(format!("SAM reply missing action: {line:?}")))?
            .to_string();
        let mut opts = HashMap::new();
        for token in parts {
            match token.split_once('=') {
                Some((k, v)) => {
                    opts.insert(k.to_string(), v.to_string());
                }
                None => {
                    opts.insert(token.to_string(), String::new());
                }
            }
        }
        Ok(Self { cmd, action, opts })
    }

    /// Whether `RESULT=OK`.
    pub(crate) fn ok(&self) -> bool {
        self.opts.get("RESULT").map(|r| r == "OK").unwrap_or(false)
    }

    /// The `RESULT` code, or `"UNKNOWN"` if absent.
    pub(crate) fn result(&self) -> &str {
        self.opts
            .get("RESULT")
            .map(|s| s.as_str())
            .unwrap_or("UNKNOWN")
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.opts.get(key).map(|s| s.as_str())
    }
}

// --- SAM command builders ----------------------------------------------------

pub(crate) fn hello() -> String {
    format!("HELLO VERSION MIN={SAM_MIN_VERSION} MAX={SAM_MAX_VERSION}\n")
}

pub(crate) fn session_create(
    style: &str,
    session_id: &str,
    destination: &str,
    options: &str,
) -> String {
    // Trailing space before options matches i2plib `sam.session_create`; an
    // empty options string leaves a harmless trailing space the bridge ignores.
    format!("SESSION CREATE STYLE={style} ID={session_id} DESTINATION={destination} {options}\n")
}

pub(crate) fn stream_connect(session_id: &str, destination: &str, silent: bool) -> String {
    format!(
        "STREAM CONNECT ID={session_id} DESTINATION={destination} SILENT={}\n",
        if silent { "true" } else { "false" }
    )
}

pub(crate) fn stream_accept(session_id: &str, silent: bool) -> String {
    format!(
        "STREAM ACCEPT ID={session_id} SILENT={}\n",
        if silent { "true" } else { "false" }
    )
}

pub(crate) fn naming_lookup(name: &str) -> String {
    format!("NAMING LOOKUP NAME={name}\n")
}

// --- Async line I/O ----------------------------------------------------------

/// Read a single `\n`-terminated line from a SAM socket, one byte at a time.
///
/// Byte-at-a-time is deliberate: after a `STREAM CONNECT` / `STREAM ACCEPT`
/// reply the *same* socket carries the raw Reticulum stream, so a buffered
/// reader would swallow stream bytes past the reply line. SAM reply lines are
/// short and infrequent (handshake only), so the per-byte reads are cheap.
pub(crate) async fn read_line<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<String, SamError> {
    let mut buf = Vec::with_capacity(64);
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Err(SamError::Protocol(
                "SAM bridge closed the connection".to_string(),
            ));
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > MAX_SAM_LINE {
            return Err(SamError::Protocol("SAM reply line too long".to_string()));
        }
    }
    // Tolerate a trailing CR just in case a bridge emits CRLF.
    while buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Write a SAM command and read the single-line reply.
pub(crate) async fn command<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
    cmd: &str,
) -> Result<Message, SamError> {
    stream.write_all(cmd.as_bytes()).await?;
    stream.flush().await?;
    let line = read_line(stream).await?;
    Message::parse(&line)
}

/// Perform the `HELLO VERSION` handshake on a freshly connected SAM socket.
pub(crate) async fn handshake<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    stream: &mut S,
) -> Result<(), SamError> {
    let reply = command(stream, &hello()).await?;
    if reply.ok() {
        Ok(())
    } else {
        Err(SamError::Result(reply.result().to_string()))
    }
}

// Golden destination captured from a live i2pd 2.56 `DEST GENERATE
// SIGNATURE_TYPE=7` and cross-checked against Python i2plib
// `Destination(...).base32`. Locks our base64 / base32 / destination parsing to
// the reference implementation, and doubles as a valid destination for the
// mock-SAM interface tests. Test-only, so it is not compiled into shipping
// binaries.
#[cfg(test)]
pub(crate) const GOLDEN_PUB: &str = "tg7j9nn88dvpnAGPJ8JveiF96oh1QD3Fcib5kdW5jza2DuP2efzx2-mcAY8nwm96IX3qiHVAPcVyJvmR1bmPNrYO4~Z5~PHb6ZwBjyfCb3ohfeqIdUA9xXIm-ZHVuY82tg7j9nn88dvpnAGPJ8JveiF96oh1QD3Fcib5kdW5jza2DuP2efzx2-mcAY8nwm96IX3qiHVAPcVyJvmR1bmPNrYO4~Z5~PHb6ZwBjyfCb3ohfeqIdUA9xXIm-ZHVuY82tg7j9nn88dvpnAGPJ8JveiF96oh1QD3Fcib5kdW5jza2DuP2efzx2-mcAY8nwm96IX3qiHVAPcVyJvmR1bmPNrYO4~Z5~PHb6ZwBjyfCb3ohfeqIdUA9xXIm-ZHVuY82tg7j9nn88dvpnAGPJ8JveiF96oh1QD3Fcib5kdW5jza2DuP2efzx2-mcAY8nwm96IX3qiHVAPcVyJvmR1bmPNqwVlNzWUvjPWqu6tqgU~YVN84ze7ogk0C0Z4nH7Hi0iBQAEAAcAAA==";
#[cfg(test)]
pub(crate) const GOLDEN_PRIV: &str = "tg7j9nn88dvpnAGPJ8JveiF96oh1QD3Fcib5kdW5jza2DuP2efzx2-mcAY8nwm96IX3qiHVAPcVyJvmR1bmPNrYO4~Z5~PHb6ZwBjyfCb3ohfeqIdUA9xXIm-ZHVuY82tg7j9nn88dvpnAGPJ8JveiF96oh1QD3Fcib5kdW5jza2DuP2efzx2-mcAY8nwm96IX3qiHVAPcVyJvmR1bmPNrYO4~Z5~PHb6ZwBjyfCb3ohfeqIdUA9xXIm-ZHVuY82tg7j9nn88dvpnAGPJ8JveiF96oh1QD3Fcib5kdW5jza2DuP2efzx2-mcAY8nwm96IX3qiHVAPcVyJvmR1bmPNrYO4~Z5~PHb6ZwBjyfCb3ohfeqIdUA9xXIm-ZHVuY82tg7j9nn88dvpnAGPJ8JveiF96oh1QD3Fcib5kdW5jza2DuP2efzx2-mcAY8nwm96IX3qiHVAPcVyJvmR1bmPNqwVlNzWUvjPWqu6tqgU~YVN84ze7ogk0C0Z4nH7Hi0iBQAEAAcAAMjDkMJJOF0tbojTo4tGveLVs6rnmEQnVMjtknnclpbRFAeoIPIA~CkQLN~VsbplR35lLNnsV-7ZJdg-IJjnomewDI38~Hy4~QKWpTL-QuVrDsL9D~rBnDRZyZnmqr-q0pBczA8VR6CpkkgKJeT9k-ks5QYZO8fOKV7ddVsJwlYhd4xn8mS-q~~CU3HSlanyubzLIJCAw1sbq86Tg-6aZPkEXqiNsSxFy6Vxvfv~ypvKVETivt69ShBZQU5P2ePGHLdBTbkd-Zf3mnqvSpmMo7alrcj3i0oiD~VgCI4JuMT0iwPk0TyqRJZWP6TFdW1pnwRdSeIc~yT4SR7l-AOdOoXLr6sg8oAHX09djgn1LixT1DUnllEsAq3IfqZrCpR15w==";
#[cfg(test)]
pub(crate) const GOLDEN_B32: &str = "iryr3x4k4sgz6tnhraseru5edf5a4byikgajg44sow4w5xlaj34q";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip_all_byte_values() {
        let data: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let encoded = i2p_b64encode(&data);
        let decoded = i2p_b64decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn b64_lengths() {
        assert_eq!(i2p_b64encode(b""), "");
        assert_eq!(i2p_b64decode("").unwrap(), b"");
        // 1 byte -> 2 symbols + 2 pad, 2 bytes -> 3 symbols + 1 pad.
        assert_eq!(i2p_b64encode(b"M").len(), 4);
        assert_eq!(i2p_b64decode(&i2p_b64encode(b"Ma")).unwrap(), b"Ma");
        assert_eq!(i2p_b64decode(&i2p_b64encode(b"Man")).unwrap(), b"Man");
    }

    #[test]
    fn b64_uses_i2p_altchars() {
        // 0xFB,0xF0 -> bits 111110 111100 00 -> indices 62, 60, ... contains
        // index 62 which must render as '-' (not '+') and 63 as '~'.
        let enc = i2p_b64encode(&[0xfb, 0xff]);
        assert!(enc.starts_with("-~"), "got {enc}");
        // Decoding the standard-alphabet form must also work.
        let std = enc.replace('-', "+").replace('~', "/");
        assert_eq!(i2p_b64decode(&std).unwrap(), i2p_b64decode(&enc).unwrap());
    }

    #[test]
    fn base32_known_vector() {
        // RFC 4648: base32("foobar") = "MZXW6YTBOI======"; lowercased and
        // unpadded that is "mzxw6ytboi".
        assert_eq!(base32_lower(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn base32_of_sha256_is_52_chars() {
        let digest = Sha256::digest(b"anything");
        assert_eq!(base32_lower(&digest).len(), 52);
    }

    #[test]
    fn message_parse_session_status() {
        let m = Message::parse("SESSION STATUS RESULT=OK DESTINATION=abc==").unwrap();
        assert_eq!(m.cmd, "SESSION");
        assert_eq!(m.action, "STATUS");
        assert!(m.ok());
        assert_eq!(m.get("DESTINATION"), Some("abc=="));
    }

    #[test]
    fn message_parse_error_result() {
        let m = Message::parse("STREAM STATUS RESULT=CANT_REACH_PEER").unwrap();
        assert!(!m.ok());
        assert_eq!(m.result(), "CANT_REACH_PEER");
    }

    #[test]
    fn message_parse_hello_reply() {
        let m = Message::parse("HELLO REPLY RESULT=OK VERSION=3.1").unwrap();
        assert!(m.ok());
        assert_eq!(m.get("VERSION"), Some("3.1"));
    }

    #[test]
    fn message_value_with_equals_sign() {
        // NAMING REPLY values are base64 with '=' padding; splitn(2,'=') keeps
        // the padding as part of the value.
        let m = Message::parse("NAMING REPLY RESULT=OK NAME=x VALUE=AAAA==").unwrap();
        assert_eq!(m.get("VALUE"), Some("AAAA=="));
    }

    #[test]
    fn command_builders_match_reference() {
        assert_eq!(hello(), "HELLO VERSION MIN=3.1 MAX=3.1\n");
        assert_eq!(
            session_create("STREAM", "nick", "TRANSIENT", ""),
            "SESSION CREATE STYLE=STREAM ID=nick DESTINATION=TRANSIENT \n"
        );
        assert_eq!(
            stream_connect("nick", "DEST", false),
            "STREAM CONNECT ID=nick DESTINATION=DEST SILENT=false\n"
        );
        assert_eq!(
            stream_accept("nick", false),
            "STREAM ACCEPT ID=nick SILENT=false\n"
        );
        assert_eq!(naming_lookup("x.b32.i2p"), "NAMING LOOKUP NAME=x.b32.i2p\n");
    }

    #[tokio::test]
    async fn read_line_stops_at_newline_without_over_reading() {
        use tokio::io::AsyncWriteExt;
        let (mut a, mut b) = tokio::io::duplex(256);
        a.write_all(b"HELLO REPLY RESULT=OK\nLEFTOVER")
            .await
            .unwrap();
        let line = read_line(&mut b).await.unwrap();
        assert_eq!(line, "HELLO REPLY RESULT=OK");
        // The bytes after the newline must still be readable (not swallowed).
        let mut rest = [0u8; 8];
        let n = b.read(&mut rest).await.unwrap();
        assert_eq!(&rest[..n], b"LEFTOVER");
    }

    #[test]
    fn golden_destination_public_base32() {
        let dest = Destination::from_public_base64(GOLDEN_PUB).unwrap();
        // The public destination decodes to 391 bytes (384 keys + 3-byte
        // certificate header + 4-byte cert payload).
        assert_eq!(dest.data.len(), 391);
        assert_eq!(dest.base32(), GOLDEN_B32);
        // Re-encoding must reproduce the exact base64 (round-trip identity).
        assert_eq!(dest.public_base64(), GOLDEN_PUB);
    }

    #[test]
    fn golden_destination_from_private_key() {
        let dest = Destination::from_private_base64(GOLDEN_PRIV).unwrap();
        // The public destination extracted from the private key must match the
        // one derived from the public base64, and yield the same b32.
        let pubdest = Destination::from_public_base64(GOLDEN_PUB).unwrap();
        assert_eq!(dest.data, pubdest.data);
        assert_eq!(dest.base32(), GOLDEN_B32);
    }

    #[test]
    fn destination_from_short_private_key_errs() {
        assert!(Destination::from_private_base64("AAAA").is_err());
    }

    /// Regression guard (Codeberg #108, fuzz `sam_parse`): the reply-line
    /// parser and the base64/destination decoders it feeds run on bytes from
    /// the SAM socket; a panic there takes the interface down. Exercise the
    /// adversarial shapes the fuzzer explored (empty/partial lines, every
    /// base64 group remainder, invalid symbols, an under-length destination)
    /// and require graceful handling, never a panic. The fuzzer found no
    /// crash here; this locks that in.
    #[test]
    fn sam_parse_path_never_panics_on_adversarial_input() {
        // Reply-line parser: empty, verb-only, key-only, and `=`-heavy tokens.
        for line in [
            "",
            " ",
            "A",
            "A B",
            "A B C=",
            "A B =v",
            "A B ==",
            "A B x=y=z",
        ] {
            let _ = Message::parse(line);
        }

        // base64 decoder: every group-remainder length (1..=4 symbols),
        // all-invalid symbols, and stray padding.
        for s in [
            "", "A", "AB", "ABC", "ABCD", "ABCDE", "====", "!!!!", "-~-~", "A===",
        ] {
            let _ = i2p_b64decode(s);
        }

        // Destination decoders on short / malformed base64 (the private-key
        // path reads a big-endian cert length after a 387-byte gate).
        for s in ["", "AAAA", "-~-~-~-~"] {
            let _ = Destination::from_public_base64(s);
            let _ = Destination::from_private_base64(s);
        }
    }
}
