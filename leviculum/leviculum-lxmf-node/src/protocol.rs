//! The line protocol periculum drives an LXMF helper with.
//!
//! The contract is `periculum/assets/scripts/lxmf_node.py:1-32` — commands on
//! stdin, `EVENT …` lines on stdout, diagnostics on stderr. Everything in this
//! module exists so the Rust helper is byte-compatible with the Python one at
//! that boundary, because the point of having two helpers is that the *driver*
//! is held constant across them (CLAUDE.md, "In comparison tests between the
//! two stacks, exploit it: the test harness points the same driver at either
//! daemon, never a parallel per-stack driver").
//!
//! Where the Python header and the Python code disagree, the code wins. Two
//! places it does:
//!
//! * The header lists six event names; the code emits a seventh,
//!   `lxmf_shutdown`, from the `quit` handler (`periculum/assets/scripts/lxmf_node.py:185`). It is
//!   implemented here.
//! * The header documents `wait_for_peer <hex> <timeout_secs>` with an integer
//!   look; the code parses `float(parts[2])` (`periculum/assets/scripts/lxmf_node.py:137`). Fractional
//!   timeouts are accepted here too.

use std::fmt::Write as _;

/// One command line from the driver, already validated.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `announce`
    Announce,
    /// `wait_for_peer <hex> <timeout_secs>`
    WaitForPeer { peer: [u8; 16], timeout_secs: f64 },
    /// `send <hex> <body_b64>`
    ///
    /// `body_b64` is retained verbatim rather than re-encoded: the driver
    /// matches `lxmf_msg_sent`'s `body_b64` field against the exact string it
    /// sent (`periculum/src/executor.rs`, `lxmf_send_verdict`), and Python
    /// echoes `parts[2]` (`periculum/assets/scripts/lxmf_node.py:181`). Re-encoding would be correct
    /// base64 and a failed step.
    Send {
        peer: [u8; 16],
        body: Vec<u8>,
        body_b64: String,
    },
    /// `quit`
    Quit,
}

/// A command the helper could not act on. Becomes `EVENT lxmf_error detail=…`,
/// which fails the step and names the reason — the same disposition Python
/// gives an exception out of `handle_command` (`periculum/assets/scripts/lxmf_node.py:114-117`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError(pub String);

impl CommandError {
    fn new(detail: impl Into<String>) -> Self {
        Self(detail.into())
    }
}

/// Parse one stdin line.
///
/// `Ok(None)` is a blank line, which Python skips without comment
/// (`periculum/assets/scripts/lxmf_node.py:110-111`).
pub fn parse_command(line: &str) -> Result<Option<Command>, CommandError> {
    let mut parts = line.split_whitespace();
    let Some(cmd) = parts.next() else {
        return Ok(None);
    };
    match cmd {
        "announce" => Ok(Some(Command::Announce)),
        "wait_for_peer" => {
            let (Some(hash), Some(timeout)) = (parts.next(), parts.next()) else {
                return Err(CommandError::new(
                    "usage: wait_for_peer <hex> <timeout_secs>",
                ));
            };
            let peer = parse_destination_hash(hash)?;
            let timeout_secs: f64 = timeout
                .parse()
                .map_err(|_| CommandError::new(format!("could not convert to float: {timeout}")))?;
            if !timeout_secs.is_finite() || timeout_secs < 0.0 {
                return Err(CommandError::new(format!(
                    "timeout must be a non-negative finite number of seconds: {timeout}"
                )));
            }
            Ok(Some(Command::WaitForPeer { peer, timeout_secs }))
        }
        "send" => {
            let (Some(hash), Some(body_b64)) = (parts.next(), parts.next()) else {
                return Err(CommandError::new("usage: send <hex> <body_b64>"));
            };
            let peer = parse_destination_hash(hash)?;
            let body = b64_decode(body_b64)
                .map_err(|e| CommandError::new(format!("invalid base64 body: {e}")))?;
            Ok(Some(Command::Send {
                peer,
                body,
                body_b64: body_b64.to_string(),
            }))
        }
        "quit" => Ok(Some(Command::Quit)),
        other => Err(CommandError::new(format!("unknown command: {other}"))),
    }
}

/// Parse a Reticulum destination hash: exactly 16 bytes of hex.
///
/// Python takes `bytes.fromhex(parts[1])` at any length and only discovers a
/// wrong one several calls later, as a recall miss or a `Destination` error.
/// The length is checked here because a truncated hash in a scenario file is a
/// typo, and "not 16 bytes" says that where "identity not known; call
/// wait_for_peer first" does not.
fn parse_destination_hash(hex: &str) -> Result<[u8; 16], CommandError> {
    let bytes = hex_decode(hex)
        .ok_or_else(|| CommandError::new(format!("non-hexadecimal destination hash: {hex}")))?;
    bytes.as_slice().try_into().map_err(|_| {
        CommandError::new(format!(
            "destination hash must be 16 bytes (32 hex chars), got {}: {hex}",
            bytes.len()
        ))
    })
}

/// Render one `EVENT` line.
///
/// Shape: `EVENT <name> <k>=<v> … t=<ms>`, one per line, `t` always last —
/// `periculum/assets/scripts/lxmf_node.py:50-57`. The driver tokenises on whitespace and splits each
/// token on the first `=` (`periculum/src/lxmf.rs`, `EventLine::parse`), so a
/// value containing a space would silently become a field of its own. Values
/// are sanitised by [`detail`] before they get here; the other emitters pass
/// hex, base64 and fixed words, none of which can contain whitespace.
pub fn format_event(name: &str, fields: &[(&str, String)], t_ms: u64) -> String {
    let mut line = String::from("EVENT ");
    line.push_str(name);
    for (key, value) in fields {
        let _ = write!(line, " {key}={value}");
    }
    let _ = write!(line, " t={t_ms}");
    line
}

/// Maximum length of an `lxmf_error` detail, from `periculum/assets/scripts/lxmf_node.py:115`.
const DETAIL_MAX: usize = 200;

/// Sanitise arbitrary prose into one `detail=` field value.
///
/// Exactly Python's `str(exc).replace(" ", "_")[:200]` (`periculum/assets/scripts/lxmf_node.py:115`),
/// extended to the other ASCII whitespace a Rust error string can contain:
/// the driver's tokeniser splits on all of it, not just `' '`, so a message
/// with a newline in it would truncate the field and invent new ones.
pub fn detail(message: &str) -> String {
    let mut out: String = message
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .collect();
    // Truncate on a char boundary; a detail is arbitrary prose and may be
    // non-ASCII, where Python's `[:200]` slices codepoints.
    if out.chars().count() > DETAIL_MAX {
        out = out.chars().take(DETAIL_MAX).collect();
    }
    out
}

// --- base64 -----------------------------------------------------------------
//
// RFC 4648 standard alphabet with `=` padding, which is what both ends already
// speak: periculum encodes with `base64::engine::general_purpose::STANDARD`
// (`periculum/src/executor.rs:9`) and Python with `base64.b64encode`. The
// in-tree I2P codec (`leviculum-std`, `interfaces::i2p::sam`) is `pub(crate)`
// and uses the `-~` alphabet, so it is neither reachable nor correct here.

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode with the standard alphabet and `=` padding.
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(B64_ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

fn b64_value(symbol: u8) -> Option<u32> {
    match symbol {
        b'A'..=b'Z' => Some(u32::from(symbol - b'A')),
        b'a'..=b'z' => Some(u32::from(symbol - b'a') + 26),
        b'0'..=b'9' => Some(u32::from(symbol - b'0') + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decode standard base64. Padding is required, as `base64.b64decode` and the
/// `STANDARD` engine both require it for the strings this helper is handed.
pub fn b64_decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(format!("length {} is not a multiple of 4", bytes.len()));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for quad in bytes.chunks(4) {
        let pad = quad.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 {
            return Err("more than two padding characters in one quad".into());
        }
        let mut n = 0u32;
        for (i, symbol) in quad.iter().enumerate() {
            let value = if i >= 4 - pad {
                0
            } else {
                b64_value(*symbol)
                    .ok_or_else(|| format!("invalid base64 symbol {:#04x}", symbol))?
            };
            n = (n << 6) | value;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

// --- hex --------------------------------------------------------------------

/// Lowercase hex, matching Python's `bytes.hex()`.
pub fn hex_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2);
    for byte in data {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- commands: every verb, and the failure of every verb ---------------

    #[test]
    fn blank_and_whitespace_only_lines_are_skipped() {
        assert_eq!(parse_command(""), Ok(None));
        assert_eq!(parse_command("   \t "), Ok(None));
    }

    #[test]
    fn every_command_the_driver_sends_parses() {
        let hash = "0102030405060708090a0b0c0d0e0f10";
        let peer = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

        assert_eq!(parse_command("announce"), Ok(Some(Command::Announce)));
        assert_eq!(
            parse_command(&format!("wait_for_peer {hash} 30")),
            Ok(Some(Command::WaitForPeer {
                peer,
                timeout_secs: 30.0
            }))
        );
        // The code parses a float even though the header reads like an int.
        assert_eq!(
            parse_command(&format!("wait_for_peer {hash} 2.5")),
            Ok(Some(Command::WaitForPeer {
                peer,
                timeout_secs: 2.5
            }))
        );
        assert_eq!(
            parse_command(&format!("send {hash} aGVsbG8=")),
            Ok(Some(Command::Send {
                peer,
                body: b"hello".to_vec(),
                body_b64: "aGVsbG8=".to_string(),
            }))
        );
        assert_eq!(parse_command("quit"), Ok(Some(Command::Quit)));
    }

    /// The driver builds `send <hash> <b64>` and then matches the echoed
    /// `body_b64` byte for byte, so the parser must not normalise it.
    #[test]
    fn send_echoes_the_body_b64_verbatim() {
        let hash = "0102030405060708090a0b0c0d0e0f10";
        let Ok(Some(Command::Send { body_b64, body, .. })) =
            parse_command(&format!("send {hash} aGVsbG8gYm9i"))
        else {
            panic!("send should parse");
        };
        assert_eq!(body_b64, "aGVsbG8gYm9i");
        assert_eq!(body, b"hello bob");
    }

    #[test]
    fn unknown_command_is_an_error_naming_it() {
        let err = parse_command("frobnicate x").expect_err("unknown command");
        assert!(err.0.contains("frobnicate"), "{}", err.0);
    }

    /// Each of the two multi-argument verbs, missing each of its arguments.
    #[test]
    fn short_commands_are_errors() {
        for line in [
            "wait_for_peer",
            "wait_for_peer 0102030405060708090a0b0c0d0e0f10",
            "send",
            "send 0102030405060708090a0b0c0d0e0f10",
        ] {
            assert!(
                parse_command(line).is_err(),
                "'{line}' must not parse as a complete command"
            );
        }
    }

    /// Both verbs that take a hash, and all three ways a hash can be wrong.
    #[test]
    fn malformed_destination_hashes_are_errors() {
        let cases = [
            ("zz02030405060708090a0b0c0d0e0f10", "non-hexadecimal"),
            ("0102030405060708090a0b0c0d0e0f", "16 bytes"),
            ("0102030405060708090a0b0c0d0e0f1011", "16 bytes"),
        ];
        for (hash, expected) in cases {
            for line in [
                format!("wait_for_peer {hash} 5"),
                format!("send {hash} aGVsbG8="),
            ] {
                let err = parse_command(&line).expect_err(&line);
                assert!(err.0.contains(expected), "{line}: {}", err.0);
            }
        }
    }

    #[test]
    fn malformed_timeouts_and_bodies_are_errors() {
        let hash = "0102030405060708090a0b0c0d0e0f10";
        assert!(parse_command(&format!("wait_for_peer {hash} soon")).is_err());
        assert!(parse_command(&format!("wait_for_peer {hash} -1")).is_err());
        assert!(parse_command(&format!("wait_for_peer {hash} nan")).is_err());
        assert!(parse_command(&format!("send {hash} not!base64")).is_err());
    }

    // --- events -------------------------------------------------------------

    #[test]
    fn event_line_shape_matches_the_python_emitter() {
        let line = format_event(
            "lxmf_msg_received",
            &[
                ("src", "aabb".into()),
                ("body_b64", "aGVsbG8=".into()),
                ("sig_valid", "true".into()),
                ("transport_encryption", "Curve25519".into()),
            ],
            1234,
        );
        assert_eq!(
            line,
            "EVENT lxmf_msg_received src=aabb body_b64=aGVsbG8= sig_valid=true \
             transport_encryption=Curve25519 t=1234"
        );
    }

    #[test]
    fn event_with_no_fields_still_carries_t() {
        assert_eq!(
            format_event("lxmf_shutdown", &[], 7),
            "EVENT lxmf_shutdown t=7"
        );
    }

    /// Every kind of whitespace, not just `' '` — the driver's tokeniser is
    /// `split_whitespace`, so a newline in a detail invents fields.
    #[test]
    fn detail_has_no_whitespace_and_is_bounded() {
        let sanitised = detail("identity for aabb not known\ncall wait_for_peer\tfirst");
        assert!(!sanitised.chars().any(char::is_whitespace), "{sanitised}");
        assert_eq!(
            sanitised,
            "identity_for_aabb_not_known_call_wait_for_peer_first"
        );

        let long = detail(&"x ".repeat(500));
        assert_eq!(long.chars().count(), DETAIL_MAX);
    }

    #[test]
    fn detail_truncation_survives_multibyte_input() {
        let sanitised = detail(&"ü".repeat(500));
        assert_eq!(sanitised.chars().count(), DETAIL_MAX);
    }

    // --- codecs -------------------------------------------------------------

    /// The vectors the two ends actually exchange, plus every padding class.
    #[test]
    fn base64_matches_the_standard_alphabet_with_padding() {
        let cases: [(&[u8], &str); 6] = [
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"hello bob from alice", "aGVsbG8gYm9iIGZyb20gYWxpY2U="),
            (&[0xfb, 0xff, 0xfe], "+//+"),
        ];
        for (raw, encoded) in cases {
            assert_eq!(b64_encode(raw), encoded, "encode {raw:?}");
            assert_eq!(b64_decode(encoded).as_deref(), Ok(raw), "decode {encoded}");
        }
    }

    #[test]
    fn base64_round_trips_every_byte_value() {
        let all: Vec<u8> = (0..=255).collect();
        for len in 0..=all.len() {
            let slice = &all[..len];
            assert_eq!(b64_decode(&b64_encode(slice)).unwrap(), slice, "len {len}");
        }
    }

    #[test]
    fn base64_rejects_malformed_input() {
        assert!(b64_decode("Zm9").is_err(), "unpadded");
        assert!(b64_decode("Zm9v=").is_err(), "not a multiple of 4");
        assert!(b64_decode("Zm-v").is_err(), "url-safe alphabet");
        assert!(b64_decode("====").is_err(), "all padding");
    }

    #[test]
    fn hex_encode_is_lowercase_and_zero_padded() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex_encode(&[]), "");
    }
}
