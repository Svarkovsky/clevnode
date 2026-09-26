use crate::msgpack;
use alloc::vec::Vec;
use leviculum_core::{crypto::full_hash, Identity};

/// An LXMF extension field. Values are retained as one complete MessagePack
/// value so unknown extensions round-trip byte-for-byte. Python accepts signed
/// integer keys, including negative fixints.
pub type Field = (i64, Vec<u8>);

/// A decoded payload, plus the exact bytes the message ID and signature cover.
struct Payload {
    timestamp: f64,
    title: Vec<u8>,
    content: Vec<u8>,
    fields: Vec<Field>,
    stamp: Option<Vec<u8>>,
    /// What `hashed_part` is built from. The reference keeps the received
    /// bytes for an unstamped payload and re-packs `unpacked_payload[:4]` for
    /// a stamped one (`LXMessage.py:753-762`); this field is that distinction.
    signed: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DeliveryMethod {
    Opportunistic = 1,
    Direct = 2,
    Propagated = 3,
    Paper = 5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    Unverified,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageError {
    TooShort,
    InvalidFormat,
    InvalidField,
    Identity,
    WrongDestination,
    /// `payload[0]` was not a MessagePack number we can carry as an `f64`.
    InvalidTimestamp,
    /// A caller asked us to sign a timestamp that is `NaN` or infinite.
    NonFiniteTimestamp,
}
impl core::fmt::Display for MessageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort => write!(f, "message is shorter than the LXMF header"),
            Self::InvalidFormat => write!(f, "malformed message"),
            Self::InvalidField => write!(f, "malformed message field"),
            Self::Identity => write!(f, "message identity operation failed"),
            Self::WrongDestination => write!(f, "message is for another destination"),
            Self::InvalidTimestamp => write!(f, "message timestamp is not a number"),
            Self::NonFiniteTimestamp => write!(f, "message timestamp is not finite"),
        }
    }
}

impl core::error::Error for MessageError {}

impl From<msgpack::Error> for MessageError {
    fn from(_: msgpack::Error) -> Self {
        Self::InvalidFormat
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub destination_hash: [u8; 16],
    pub source_hash: [u8; 16],
    pub signature: [u8; 64],
    pub timestamp: f64,
    pub title: Vec<u8>,
    pub content: Vec<u8>,
    pub fields: Vec<Field>,
    /// Delivery stamp. PoW stamps are 32 bytes; ticket-derived stamps are the
    /// 16-byte Reticulum truncated hash used by the Python implementation.
    pub stamp: Option<Vec<u8>>,
    pub message_id: [u8; 32],
    pub verification: Verification,
    pub method: DeliveryMethod,
    /// The bytes this message arrived as, when it came off the wire.
    ///
    /// The reference caches the same thing (`message.packed = lxmf_bytes`,
    /// `LXMessage.py:799`) and `pack()` returns it untouched (`if not
    /// self.packed`, `:355`). Without the cache, re-packing a message composed
    /// by a writer whose MessagePack encoder differs from ours would emit
    /// bytes its signature no longer covers.
    packed: Option<Vec<u8>>,
}

impl Message {
    /// Construct and sign a complete LXMF message from its protocol fields.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        destination_hash: [u8; 16],
        source_hash: [u8; 16],
        source: &Identity,
        timestamp: f64,
        title: Vec<u8>,
        content: Vec<u8>,
        fields: Vec<Field>,
        method: DeliveryMethod,
    ) -> Result<Self, MessageError> {
        if !timestamp.is_finite() {
            return Err(MessageError::NonFiniteTimestamp);
        }
        for (_, v) in &fields {
            let mut p = 0;
            msgpack::skip(v, &mut p).map_err(|_| MessageError::InvalidField)?;
            if p != v.len() {
                return Err(MessageError::InvalidField);
            }
        }
        let payload = encode_payload(
            msgpack::Number::Float(timestamp),
            &title,
            &content,
            &fields,
            None,
        );
        let mut hashed = Vec::with_capacity(32 + payload.len());
        hashed.extend_from_slice(&destination_hash);
        hashed.extend_from_slice(&source_hash);
        hashed.extend_from_slice(&payload);
        let message_id = full_hash(&hashed);
        let mut signed = hashed;
        signed.extend_from_slice(&message_id);
        let signature = source.sign(&signed).map_err(|_| MessageError::Identity)?;
        Ok(Self {
            destination_hash,
            source_hash,
            signature,
            timestamp,
            title,
            content,
            fields,
            stamp: None,
            message_id,
            verification: Verification::Valid,
            method,
            packed: None,
        })
    }
    pub fn set_stamp(&mut self, stamp: Vec<u8>) -> Result<(), MessageError> {
        if stamp.len() != 16 && stamp.len() != 32 {
            return Err(MessageError::InvalidFormat);
        }
        self.stamp = Some(stamp);
        // The wire form changed, so the bytes we came in as no longer describe
        // this message.
        self.packed = None;
        Ok(())
    }
    pub fn pack(&self) -> Vec<u8> {
        if let Some(packed) = &self.packed {
            return packed.clone();
        }
        let payload = encode_payload(
            msgpack::Number::Float(self.timestamp),
            &self.title,
            &self.content,
            &self.fields,
            self.stamp.as_deref(),
        );
        let mut o = Vec::with_capacity(96 + payload.len());
        o.extend_from_slice(&self.destination_hash);
        o.extend_from_slice(&self.source_hash);
        o.extend_from_slice(&self.signature);
        o.extend_from_slice(&payload);
        o
    }
    pub fn on_air(&self) -> Result<Vec<u8>, MessageError> {
        let p = self.pack();
        match self.method {
            DeliveryMethod::Opportunistic => Ok(p[16..].to_vec()),
            DeliveryMethod::Direct => Ok(p),
            // These methods require destination encryption and dedicated
            // framing; returning plaintext here would be a confidentiality bug.
            DeliveryMethod::Propagated | DeliveryMethod::Paper => Err(MessageError::InvalidFormat),
        }
    }
    pub fn unpack(
        data: &[u8],
        inferred_destination: Option<[u8; 16]>,
        source: Option<&Identity>,
        method: DeliveryMethod,
    ) -> Result<Self, MessageError> {
        let owned;
        let d = if method == DeliveryMethod::Opportunistic {
            let dst = inferred_destination.ok_or(MessageError::WrongDestination)?;
            owned = {
                let mut x = Vec::with_capacity(data.len() + 16);
                x.extend_from_slice(&dst);
                x.extend_from_slice(data);
                x
            };
            owned.as_slice()
        } else {
            data
        };
        if d.len() < 97 {
            return Err(MessageError::TooShort);
        }
        let destination_hash: [u8; 16] = d[0..16].try_into().map_err(|_| MessageError::TooShort)?;
        let source_hash: [u8; 16] = d[16..32].try_into().map_err(|_| MessageError::TooShort)?;
        let signature: [u8; 64] = d[32..96].try_into().map_err(|_| MessageError::TooShort)?;
        let Payload {
            timestamp,
            title,
            content,
            fields,
            stamp,
            signed: signed_payload,
        } = decode_payload(&d[96..])?;
        let mut hashed = Vec::new();
        hashed.extend_from_slice(&destination_hash);
        hashed.extend_from_slice(&source_hash);
        hashed.extend_from_slice(&signed_payload);
        let message_id = full_hash(&hashed);
        let mut signed = hashed;
        signed.extend_from_slice(&message_id);
        let verification = match source {
            None => Verification::Unverified,
            Some(i) => {
                if i.verify(&signed, &signature)
                    .map_err(|_| MessageError::Identity)?
                {
                    Verification::Valid
                } else {
                    Verification::Invalid
                }
            }
        };
        Ok(Self {
            destination_hash,
            source_hash,
            signature,
            timestamp,
            title,
            content,
            fields,
            stamp,
            message_id,
            verification,
            method,
            packed: Some(d.to_vec()),
        })
    }
}

fn encode_payload(
    ts: msgpack::Number,
    title: &[u8],
    content: &[u8],
    fields: &[Field],
    stamp: Option<&[u8]>,
) -> Vec<u8> {
    let mut o = Vec::new();
    msgpack::array(&mut o, if stamp.is_some() { 5 } else { 4 });
    match ts {
        msgpack::Number::Int(v) => msgpack::int(&mut o, v),
        msgpack::Number::Float(v) => msgpack::f64(&mut o, v),
    }
    msgpack::bin(&mut o, title);
    msgpack::bin(&mut o, content);
    msgpack::map(&mut o, fields.len());
    for (k, v) in fields {
        msgpack::int(&mut o, *k);
        o.extend_from_slice(v)
    }
    if let Some(s) = stamp {
        msgpack::bin(&mut o, s)
    }
    o
}
/// Decode `[timestamp, title, content, fields (, stamp)]`.
///
/// `payload[0]` is read as any MessagePack number, not only float64. The
/// reference takes `unpacked_payload[0]` with no type check at all
/// (`LXMessage.py:766`), so a writer that packs an integer second — LXMF
/// itself never does, but a third-party implementation may — produces a
/// message Python delivers. Refusing it here dropped the whole message with
/// no diagnostic (Codeberg #183). Non-numeric types are refused, because
/// `Message::timestamp` is an `f64` and nothing downstream of the reference
/// can use a string or `nil` as a time either.
fn decode_payload(d: &[u8]) -> Result<Payload, MessageError> {
    let mut p = 0;
    let n = msgpack::array_len(d, &mut p)?;
    if !(4..=5).contains(&n) {
        return Err(MessageError::InvalidFormat);
    }
    let ts = msgpack::read_number(d, &mut p).map_err(|_| MessageError::InvalidTimestamp)?;
    let title = msgpack::read_bin(d, &mut p)?.to_vec();
    let content = msgpack::read_bin(d, &mut p)?.to_vec();
    let count = msgpack::map_len(d, &mut p)?;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let k = msgpack::read_int(d, &mut p)?;
        fields.push((k, msgpack::raw(d, &mut p)?.to_vec()))
    }
    let stamp = if n == 5 {
        let s = msgpack::read_bin(d, &mut p)?;
        if s.len() != 16 && s.len() != 32 {
            return Err(MessageError::InvalidFormat);
        }
        Some(s.to_vec())
    } else {
        None
    };
    if p != d.len() {
        return Err(MessageError::InvalidFormat);
    }
    // The reference hashes `packed_payload`, and only replaces it with a
    // re-pack of the first four elements when a stamp had to be stripped
    // (`LXMessage.py:753-762`). Mirroring both branches keeps our accept set
    // equal to Python's: an unstamped payload verifies whatever its writer's
    // encoder produced, and a stamped one survives only if the writer's
    // encoding is what `msgpack.packb` would emit — which is why the numeric
    // family, not just the value, is carried through the re-pack.
    let signed = if n == 5 {
        encode_payload(ts, &title, &content, &fields, None)
    } else {
        d.to_vec()
    };
    Ok(Payload {
        timestamp: ts.as_f64(),
        title,
        content,
        fields,
        stamp,
        signed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opportunistic_message_round_trips() {
        let private: Vec<u8> = (0..64).collect();
        let id = Identity::from_private_key_bytes(&private).unwrap();
        let destination_hash = [1; 16];
        let m = Message::create(
            destination_hash,
            [2; 16],
            &id,
            1_700_000_000.0,
            b"Hi".to_vec(),
            b"Hello".to_vec(),
            Vec::new(),
            DeliveryMethod::Opportunistic,
        )
        .unwrap();
        let u = Message::unpack(
            &m.on_air().unwrap(),
            Some(destination_hash),
            Some(&id),
            DeliveryMethod::Opportunistic,
        )
        .unwrap();
        assert_eq!(u.verification, Verification::Valid);
    }
}
