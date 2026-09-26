//! LXMF paper-message encryption and `lxm://` URI representation.
//!
//! A paper message keeps the 16-byte delivery destination hash in the clear
//! and encrypts the rest of the fully packed LXMF message with the Reticulum
//! destination:
//!
//! ```text
//! destination_hash || destination.encrypt(packed_lxmf[16..])
//! ```
//!
//! The resulting bytes are encoded with URL-safe base64 without padding and
//! prefixed with `lxm://`. This is the format emitted by the vendored Python
//! LXMF reference implementation.

use crate::constants::{DESTINATION_LENGTH, LXMF_OVERHEAD, PAPER_MDU};
use alloc::{string::String, vec::Vec};
use leviculum_core::{Destination, DestinationError};
use rand_core::CryptoRngCore;

/// URI prefix used by LXMF paper messages.
pub const URI_PREFIX: &str = "lxm://";

/// Errors returned while constructing or ingesting paper messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperError {
    /// The supplied bytes cannot contain an LXMF destination hash and payload.
    TooShort,
    /// The encrypted paper representation exceeds Python LXMF's QR payload cap.
    TooLarge,
    /// The clear destination hash does not match the supplied destination.
    WrongDestination,
    /// The URI did not use the `lxm://` scheme.
    InvalidScheme,
    /// The URI contained malformed URL-safe base64.
    InvalidBase64,
    /// Reticulum destination encryption failed.
    Encryption,
    /// Reticulum destination decryption failed.
    Decryption,
}

impl core::fmt::Display for PaperError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::TooShort => "paper message is too short",
            Self::TooLarge => "paper message exceeds the LXMF paper MDU",
            Self::WrongDestination => "paper message destination does not match",
            Self::InvalidScheme => "paper message URI does not use the lxm scheme",
            Self::InvalidBase64 => "paper message URI contains invalid base64",
            Self::Encryption => "paper message encryption failed",
            Self::Decryption => "paper message decryption failed",
        };
        f.write_str(text)
    }
}

impl core::error::Error for PaperError {}

/// The binary representation carried by a paper-message URI or QR code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaperMessage {
    destination_hash: [u8; DESTINATION_LENGTH],
    encrypted_payload: Vec<u8>,
}

impl PaperMessage {
    /// Encrypt a fully packed LXMF message for a Reticulum destination.
    ///
    /// `packed_lxmf` must start with the same destination hash as
    /// `destination`. `ratchet_public` is the optional ratchet public key
    /// learned from the destination's announce.
    pub fn encrypt(
        packed_lxmf: &[u8],
        destination: &Destination,
        ratchet_public: Option<&[u8; 32]>,
        rng: &mut impl CryptoRngCore,
    ) -> Result<Self, PaperError> {
        if packed_lxmf.len() <= DESTINATION_LENGTH {
            return Err(PaperError::TooShort);
        }

        let destination_hash: [u8; DESTINATION_LENGTH] = packed_lxmf
            .get(..DESTINATION_LENGTH)
            .ok_or(PaperError::TooShort)?
            .try_into()
            .map_err(|_| PaperError::TooShort)?;
        if destination_hash != *destination.hash().as_bytes() {
            return Err(PaperError::WrongDestination);
        }

        let encrypted_payload = destination
            .encrypt(&packed_lxmf[DESTINATION_LENGTH..], ratchet_public, rng)
            .map_err(map_encrypt_error)?;
        let paper_len = DESTINATION_LENGTH
            .checked_add(encrypted_payload.len())
            .ok_or(PaperError::TooLarge)?;
        if paper_len > PAPER_MDU {
            return Err(PaperError::TooLarge);
        }

        Ok(Self {
            destination_hash,
            encrypted_payload,
        })
    }

    /// Parse the raw bytes obtained from a paper URI or QR code.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PaperError> {
        // Python's ingest path accepts URI payloads independent of the local
        // QR generation cap, but rejects anything below LXMF_OVERHEAD in
        // `lxmf_propagation()`.
        if bytes.len() < LXMF_OVERHEAD {
            return Err(PaperError::TooShort);
        }

        Ok(Self {
            destination_hash: bytes[..DESTINATION_LENGTH]
                .try_into()
                .map_err(|_| PaperError::TooShort)?,
            encrypted_payload: bytes[DESTINATION_LENGTH..].to_vec(),
        })
    }

    /// Decode an unpadded, URL-safe `lxm://` paper-message URI.
    ///
    /// Slash characters after the scheme are ignored for parity with the
    /// Python ingest path, which permits visually wrapped/scanned URIs.
    pub fn from_uri(uri: &str) -> Result<Self, PaperError> {
        let encoded = uri
            .strip_prefix(URI_PREFIX)
            .ok_or(PaperError::InvalidScheme)?;
        let mut compact = Vec::with_capacity(encoded.len());
        compact.extend(encoded.bytes().filter(|byte| *byte != b'/'));
        let bytes = decode_base64url(&compact)?;
        Self::from_bytes(&bytes)
    }

    /// Decrypt this paper message and reconstruct the fully packed LXMF bytes.
    pub fn decrypt(&self, destination: &Destination) -> Result<Vec<u8>, PaperError> {
        if self.destination_hash != *destination.hash().as_bytes() {
            return Err(PaperError::WrongDestination);
        }

        let plaintext = destination
            .decrypt(&self.encrypted_payload)
            .map_err(map_decrypt_error)?;
        let mut packed = Vec::with_capacity(DESTINATION_LENGTH + plaintext.len());
        packed.extend_from_slice(&self.destination_hash);
        packed.extend_from_slice(&plaintext);
        Ok(packed)
    }

    /// Return the clear delivery destination hash.
    pub const fn destination_hash(&self) -> &[u8; DESTINATION_LENGTH] {
        &self.destination_hash
    }

    /// Return the destination-encrypted portion of the paper message.
    pub fn encrypted_payload(&self) -> &[u8] {
        &self.encrypted_payload
    }

    /// Serialize the binary paper representation.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(DESTINATION_LENGTH + self.encrypted_payload.len());
        bytes.extend_from_slice(&self.destination_hash);
        bytes.extend_from_slice(&self.encrypted_payload);
        bytes
    }

    /// Serialize this paper message as Python-compatible `lxm://` text.
    pub fn to_uri(&self) -> String {
        let bytes = self.to_bytes();
        let encoded = encode_base64url(&bytes);
        let mut uri = String::with_capacity(URI_PREFIX.len() + encoded.len());
        uri.push_str(URI_PREFIX);
        uri.push_str(&encoded);
        uri
    }
}

fn map_encrypt_error(_: DestinationError) -> PaperError {
    PaperError::Encryption
}

fn map_decrypt_error(_: DestinationError) -> PaperError {
    PaperError::Decryption
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_base64url(input: &[u8]) -> String {
    let encoded_len = input.len().saturating_add(2) / 3 * 4;
    let mut output = String::with_capacity(encoded_len);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let value = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32;
        output.push(BASE64URL[((value >> 18) & 0x3f) as usize] as char);
        output.push(BASE64URL[((value >> 12) & 0x3f) as usize] as char);
        output.push(BASE64URL[((value >> 6) & 0x3f) as usize] as char);
        output.push(BASE64URL[(value & 0x3f) as usize] as char);
    }

    match chunks.remainder() {
        [a] => {
            output.push(BASE64URL[(a >> 2) as usize] as char);
            output.push(BASE64URL[((a & 0x03) << 4) as usize] as char);
        }
        [a, b] => {
            output.push(BASE64URL[(a >> 2) as usize] as char);
            output.push(BASE64URL[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
            output.push(BASE64URL[((b & 0x0f) << 2) as usize] as char);
        }
        _ => {}
    }
    output
}

fn decode_base64url(input: &[u8]) -> Result<Vec<u8>, PaperError> {
    let mut unpadded_len = input.len();
    while unpadded_len > 0 && input[unpadded_len - 1] == b'=' {
        unpadded_len -= 1;
    }
    if input[..unpadded_len].contains(&b'=') || unpadded_len % 4 == 1 {
        return Err(PaperError::InvalidBase64);
    }

    let decoded_len = unpadded_len / 4 * 3
        + match unpadded_len % 4 {
            2 => 1,
            3 => 2,
            _ => 0,
        };
    let mut output = Vec::with_capacity(decoded_len);
    let mut index = 0;
    while index + 4 <= unpadded_len {
        let a = decode_digit(input[index])?;
        let b = decode_digit(input[index + 1])?;
        let c = decode_digit(input[index + 2])?;
        let d = decode_digit(input[index + 3])?;
        output.push((a << 2) | (b >> 4));
        output.push((b << 4) | (c >> 2));
        output.push((c << 6) | d);
        index += 4;
    }

    match unpadded_len - index {
        0 => {}
        2 => {
            let a = decode_digit(input[index])?;
            let b = decode_digit(input[index + 1])?;
            if b & 0x0f != 0 {
                return Err(PaperError::InvalidBase64);
            }
            output.push((a << 2) | (b >> 4));
        }
        3 => {
            let a = decode_digit(input[index])?;
            let b = decode_digit(input[index + 1])?;
            let c = decode_digit(input[index + 2])?;
            if c & 0x03 != 0 {
                return Err(PaperError::InvalidBase64);
            }
            output.push((a << 2) | (b >> 4));
            output.push((b << 4) | (c >> 2));
        }
        _ => return Err(PaperError::InvalidBase64),
    }
    Ok(output)
}

fn decode_digit(byte: u8) -> Result<u8, PaperError> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'-' => Ok(62),
        b'_' => Ok(63),
        _ => Err(PaperError::InvalidBase64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leviculum_core::{DestinationType, Direction, Identity};
    use rand_core::{CryptoRng, Error as RandError, RngCore};

    const PACKED: &str = "cf0b2a4a8d2a0b6978b71290da7cc80e\
        fae321c442e3c9bdcd7a3e79d850e03c\
        fb321978105a4c709c3b86930ff15a9d7b53b3485517ec19e2083b39f7661e6\
        e531c78fb71d932f0baf13794c42234ab9320f1ab5b7688e93eaf5960810ece00\
        94cb41d954fc40000000c4024869c40548656c6c6f80";
    const PAPER_BYTES: &str = "cf0b2a4a8d2a0b6978b71290da7cc80e\
        605a725d2a4adfeeb1a29e17edd621c1b7593ee8cdbc44ac6c4ab6e2f805d23c\
        c0c1c2c3c4c5c6c7c8c9cacbcccdcecf5f9b139c1648beda6ec1d5fff14e3d9\
        ef1ca40ea2df3a2293b23677c8eeda54354795d7e5ebe236ca1c63ffd42d6f56\
        0d3aeeda553ea80b4b98110aaac04813018d5a29f8de579401088b9ac5a71b5f\
        28655f455c9dba0a05fcc900de2ec7a80b803ef415d812d0c1d928508bbe30ab\
        d3c499c98e3e7ad60e7f3ffecea68cba92194662a17928582799df013ca56b39c";
    const PAPER_URI: &str = "lxm://zwsqSo0qC2l4txKQ2nzIDmBacl0qSt_usaKeF-3WIcG3WT7ozbxErGxKtuL4BdI8wMHCw8TFxsfIycrLzM3Oz1-bE5wWSL7absHV__FOPZ7xykDqLfOiKTsjZ3yO7aVDVHldfl6-I2yhxj_9Qtb1YNOu7aVT6oC0uYEQqqwEgTAY1aKfjeV5QBCIuaxacbXyhlX0VcnboKBfzJAN4ux6gLgD70FdgS0MHZKFCLvjCr08SZyY4-etYOfz_-zqaMupIZRmKheShYJ5nfATylaznA";

    struct FixedRng {
        bytes: [u8; 48],
        position: usize,
    }

    impl FixedRng {
        fn python_vector() -> Self {
            let mut bytes = [0u8; 48];
            for (index, byte) in bytes.iter_mut().enumerate() {
                *byte = 0xa0 + index as u8;
            }
            Self { bytes, position: 0 }
        }

        fn take(&mut self) -> u8 {
            let byte = self.bytes[self.position];
            self.position += 1;
            byte
        }
    }

    impl RngCore for FixedRng {
        fn next_u32(&mut self) -> u32 {
            u32::from_le_bytes([self.take(), self.take(), self.take(), self.take()])
        }

        fn next_u64(&mut self) -> u64 {
            u64::from_le_bytes([
                self.take(),
                self.take(),
                self.take(),
                self.take(),
                self.take(),
                self.take(),
                self.take(),
                self.take(),
            ])
        }

        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for byte in dest {
                *byte = self.take();
            }
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), RandError> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    impl CryptoRng for FixedRng {}

    fn target_identity() -> Identity {
        let private: Vec<u8> = (64u8..128).collect();
        Identity::from_private_key_bytes(&private).unwrap()
    }

    fn destination(direction: Direction) -> Destination {
        Destination::new(
            Some(target_identity()),
            direction,
            DestinationType::Single,
            "lxmf",
            &["delivery"],
        )
        .unwrap()
    }

    #[test]
    fn python_paper_encryption_and_uri_vector() {
        let packed = hex::decode(PACKED.replace(char::is_whitespace, "")).unwrap();
        let expected = hex::decode(PAPER_BYTES.replace(char::is_whitespace, "")).unwrap();
        let mut rng = FixedRng::python_vector();
        let paper =
            PaperMessage::encrypt(&packed, &destination(Direction::Out), None, &mut rng).unwrap();

        assert_eq!(paper.to_bytes(), expected);
        assert_eq!(paper.to_uri(), PAPER_URI);
        assert_eq!(PaperMessage::from_uri(PAPER_URI).unwrap(), paper);
        assert_eq!(paper.decrypt(&destination(Direction::In)).unwrap(), packed);
    }

    #[test]
    fn uri_parser_matches_python_padding_and_slash_tolerance() {
        let paper = PaperMessage::from_uri(PAPER_URI).unwrap();
        let padded = alloc::format!("{}==", PAPER_URI);
        assert_eq!(PaperMessage::from_uri(&padded).unwrap(), paper);

        let split = alloc::format!("{}/{}", &PAPER_URI[..40], &PAPER_URI[40..]);
        assert_eq!(PaperMessage::from_uri(&split).unwrap(), paper);
    }

    #[test]
    fn paper_destination_and_mdu_are_enforced() {
        let expected = hex::decode(PAPER_BYTES.replace(char::is_whitespace, "")).unwrap();
        let paper = PaperMessage::from_bytes(&expected).unwrap();

        let other_private: Vec<u8> = (128u8..192).collect();
        let other = Identity::from_private_key_bytes(&other_private).unwrap();
        let other_destination = Destination::new(
            Some(other),
            Direction::In,
            DestinationType::Single,
            "lxmf",
            &["delivery"],
        )
        .unwrap();
        assert_eq!(
            paper.decrypt(&other_destination),
            Err(PaperError::WrongDestination)
        );

        let undersized = alloc::vec![0u8; LXMF_OVERHEAD - 1];
        assert_eq!(
            PaperMessage::from_bytes(&undersized),
            Err(PaperError::TooShort)
        );

        // PAPER_MDU is an outbound QR-generation constraint. Python's URI
        // ingest path still accepts a larger syntactically valid byte string.
        let oversized = alloc::vec![0u8; PAPER_MDU + 1];
        assert!(PaperMessage::from_bytes(&oversized).is_ok());
    }
}
