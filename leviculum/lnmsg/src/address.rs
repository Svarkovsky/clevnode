//! Parsing an LXMF destination address.
//!
//! An address on the wire is a 16-byte destination hash, so on a command line
//! it is 32 hex characters. The one accepted decoration is the `lxmf@` prefix
//! `lnomad` puts on the clipboard when a page links a correspondent
//! (`follow_link`, `lnomad/src/tui.rs:2768-2777`): that handoff exists
//! precisely so the address can be pasted here, and refusing the form the
//! browser produces would break it for the sake of strictness.

/// Length of an LXMF destination hash in bytes.
pub const ADDRESS_BYTES: usize = 16;

/// The prefix `lnomad` copies with an address, and NomadNet pages carry.
const URI_PREFIX: &str = "lxmf@";

/// Why an address on the command line is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressError {
    /// Nothing was given where an address was expected.
    Empty,
    /// The right characters, the wrong count.
    WrongLength { got: usize },
    /// A character that is not a hex digit.
    NotHex { position: usize, character: char },
}

impl std::fmt::Display for AddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "an LXMF address is required"),
            Self::WrongLength { got } => write!(
                f,
                "an LXMF address is {} hex characters ({ADDRESS_BYTES} bytes); this one has {got}",
                ADDRESS_BYTES * 2
            ),
            Self::NotHex {
                position,
                character,
            } => write!(
                f,
                "an LXMF address is hex; character {} is {character:?}",
                position + 1
            ),
        }
    }
}

impl std::error::Error for AddressError {}

/// Parse `text` into a destination hash.
///
/// Case-insensitive, tolerant of surrounding whitespace (a pasted address
/// often carries a newline) and of the `lxmf@` prefix. Deliberately intolerant
/// of everything else: an address is the one argument whose typo cannot be
/// caught later, because a wrong-but-well-formed hash is a destination that
/// simply never answers.
pub fn parse(text: &str) -> Result<[u8; ADDRESS_BYTES], AddressError> {
    let trimmed = text.trim();
    let hex = trimmed.strip_prefix(URI_PREFIX).unwrap_or(trimmed);
    if hex.is_empty() {
        return Err(AddressError::Empty);
    }
    if let Some((position, character)) = hex.char_indices().find(|(_, c)| !c.is_ascii_hexdigit()) {
        return Err(AddressError::NotHex {
            position,
            character,
        });
    }
    if hex.len() != ADDRESS_BYTES * 2 {
        return Err(AddressError::WrongLength { got: hex.len() });
    }
    let mut out = [0u8; ADDRESS_BYTES];
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        let digit = |c: u8| match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            // The hex-digit scan above already rejected everything else.
            _ => c.to_ascii_lowercase() - b'a' + 10,
        };
        *byte = (digit(pair[0]) << 4) | digit(pair[1]);
    }
    Ok(out)
}

/// Lowercase hex, the form every other tool in the tree prints a hash in.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('?'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('?'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_32_character_hash_parses() {
        let parsed = parse("000102030405060708090a0b0c0d0e0f").expect("well-formed address");
        assert_eq!(
            parsed,
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        );
    }

    #[test]
    fn upper_case_and_surrounding_whitespace_are_accepted() {
        let upper = parse("  AABBCCDDEEFF00112233445566778899\n");
        let lower = parse("aabbccddeeff00112233445566778899");
        assert_eq!(upper, lower);
        assert!(upper.is_ok());
    }

    /// The lnomad clipboard handoff pastes this form; it has to work.
    #[test]
    fn the_lxmf_uri_prefix_is_accepted() {
        assert_eq!(
            parse("lxmf@aabbccddeeff00112233445566778899"),
            parse("aabbccddeeff00112233445566778899")
        );
    }

    #[test]
    fn a_short_hash_is_rejected_with_its_length() {
        assert_eq!(
            parse("aabbccdd"),
            Err(AddressError::WrongLength { got: 8 }),
            "a truncated address must not be silently padded"
        );
    }

    #[test]
    fn a_non_hex_character_is_named() {
        assert_eq!(
            parse("aabbccddeeff001122334455667788zz"),
            Err(AddressError::NotHex {
                position: 30,
                character: 'z'
            })
        );
    }

    #[test]
    fn an_empty_address_is_its_own_error() {
        assert_eq!(parse(""), Err(AddressError::Empty));
        assert_eq!(parse("lxmf@"), Err(AddressError::Empty));
    }

    #[test]
    fn to_hex_round_trips_parse() {
        let text = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
        assert_eq!(to_hex(&parse(text).expect("parse")), text);
    }
}
