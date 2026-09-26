//! Python-compatible LXMF propagation-client wire envelopes.
//!
//! The public API covers propagation-node discovery, client uploads and the
//! `/get` mailbox exchange: submitting one recipient-encrypted message,
//! listing available transient IDs, downloading selected messages, and
//! acknowledging received messages. It performs no I/O and owns no links or
//! resources.
//!
//! A client first sends [`MessageGetRequest::list`], decodes the returned
//! [`MessageListResponse`], and sends a second request whose `wants` contains
//! the selected IDs. Each downloaded entry in [`MessageGetResponse`] is an
//! unstamped, destination-encrypted LXMF message. After accepting an entry,
//! the client acknowledges `SHA-256(entry)` with
//! [`MessageGetRequest::acknowledge`], matching Python `LXMRouter`'s purge
//! exchange.
//!
use crate::{
    constants::{DESTINATION_LENGTH, LXMF_OVERHEAD, STAMP_SIZE},
    msgpack,
};
use alloc::vec::Vec;
use leviculum_core::{crypto::full_hash, Destination, DestinationError};

/// Reticulum link request path used by a propagation client.
pub const MESSAGE_GET_PATH: &str = "/get";

/// SHA-256 identifier used to list, request, and acknowledge messages.
pub type TransientId = [u8; 32];
/// `(key, raw_msgpack_value)` entry in propagation-node announce metadata.
pub type MetadataEntry = (u64, Vec<u8>);

/// One Python-compatible propagation-node upload.
///
/// The encoded value is exactly
/// `[timestamp, [unstamped_lxmf || propagation_stamp]]`. `unstamped_lxmf` is
/// `destination_hash || destination.encrypt(packed_message[16..])`. The
/// transient ID deliberately hashes those bytes *before* the independent
/// 32-byte propagation-node stamp is appended.
#[derive(Debug, Clone, PartialEq)]
pub struct PropagationUpload {
    timestamp: f64,
    unstamped_lxmf: Vec<u8>,
    propagation_stamp: [u8; STAMP_SIZE],
    transient_id: TransientId,
}

impl PropagationUpload {
    /// Construct the singleton envelope used by an originating LXMF client.
    pub fn single(
        timestamp: f64,
        unstamped_lxmf: Vec<u8>,
        propagation_stamp: [u8; STAMP_SIZE],
    ) -> Self {
        let transient_id = full_hash(&unstamped_lxmf);
        Self {
            timestamp,
            unstamped_lxmf,
            propagation_stamp,
            transient_id,
        }
    }

    pub const fn timestamp(&self) -> f64 {
        self.timestamp
    }

    pub fn unstamped_lxmf(&self) -> &[u8] {
        &self.unstamped_lxmf
    }

    pub const fn propagation_stamp(&self) -> &[u8; STAMP_SIZE] {
        &self.propagation_stamp
    }

    pub const fn transient_id(&self) -> &TransientId {
        &self.transient_id
    }

    /// Encode the exact MessagePack body sent as raw Link data or a Resource.
    pub fn encode(&self) -> Vec<u8> {
        let mut stamped = Vec::with_capacity(self.unstamped_lxmf.len() + STAMP_SIZE);
        stamped.extend_from_slice(&self.unstamped_lxmf);
        stamped.extend_from_slice(&self.propagation_stamp);

        let mut output = Vec::new();
        msgpack::array(&mut output, 2);
        msgpack::f64(&mut output, self.timestamp);
        msgpack::array(&mut output, 1);
        msgpack::bin(&mut output, &stamped);
        output
    }

    /// Decode an upload envelope received off the wire (raw Link data or a
    /// Resource body) — the propagation-node HOST side (leviculum#38).
    ///
    /// Exact inverse of [`encode`](Self::encode): `[timestamp, [stamped]]`
    /// where `stamped = unstamped_lxmf || propagation_stamp`. The transient ID
    /// is recomputed from the unstamped bytes, exactly as
    /// [`single`](Self::single) does, so a host validates the stamp against
    /// the same transient ID an honest client computed.
    ///
    /// Only the singleton envelope an originating client sends is accepted.
    /// The multi-message form belongs to the node-to-node `/offer` sync path,
    /// which this crate does not implement — see [the crate docs](crate) for
    /// that split — and is reported as
    /// [`PropagationError::MultipleMessages`] rather than as a bad length.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropagationError> {
        let mut position = 0;
        if msgpack::array_len(bytes, &mut position)? != 2 {
            return Err(PropagationError::InvalidLength);
        }
        let timestamp = msgpack::read_number_f64(bytes, &mut position)?;
        match msgpack::array_len(bytes, &mut position)? {
            1 => {}
            // Not a malformed length — a different message. Python parses
            // client uploads and peer syncs with one parser over one wire
            // shape (reference/LXMF/LXMF/LXMRouter.py:2336-2345) and tells
            // them apart afterwards by peering-key state (:2377-2385): more
            // than one message in a transfer is the `/offer` sync form and
            // requires a validated peering key. Reporting that distinctly
            // lets the `/offer` layer branch on it when it lands
            // (leviculum#209), instead of forcing a second public parser for
            // a wire shape identical to this one.
            0 => return Err(PropagationError::InvalidLength),
            _ => return Err(PropagationError::MultipleMessages),
        }
        let stamped = msgpack::read_bin(bytes, &mut position)?;
        // `validate_pn_stamp` (reference/LXMF/LXMF/LXStamper.py:86) discards a
        // transient body of `LXMF_OVERHEAD + STAMP_SIZE` bytes or fewer — 112
        // + 32 = 144. Guarding at hash+stamp (48) instead would accept the
        // band 49..=144, which every Python propagation node throws away, and
        // which `PropagatedMessage::from_unstamped_bytes` below then refuses
        // anyway: the host would store what it cannot index.
        if stamped.len() <= LXMF_OVERHEAD + STAMP_SIZE {
            return Err(PropagationError::InvalidLength);
        }
        let (unstamped, stamp) = stamped.split_at(stamped.len() - STAMP_SIZE);
        let mut propagation_stamp = [0u8; STAMP_SIZE];
        propagation_stamp.copy_from_slice(stamp);
        // No `finish()` here, unlike every other decoder in this module, and
        // that is deliberate rather than an omission. `LXMRouter` unpacks this
        // envelope with `RNS.vendor.umsgpack.unpackb`
        // (reference/LXMF/LXMF/LXMRouter.py:14), which ignores bytes after the
        // decoded value; pip `msgpack` would raise, but LXMRouter does not use
        // it. So the leniency is what matches Python here, and the strict
        // siblings are the divergence — do not "fix" this by adding the call.
        Ok(Self::single(
            timestamp,
            unstamped.to_vec(),
            propagation_stamp,
        ))
    }
}

/// Errors returned by LXMF propagation codecs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropagationError {
    Truncated,
    InvalidType,
    InvalidLength,
    /// The upload carried more than one message — the peer `/offer` sync
    /// form, not a client upload. Distinct from [`Self::InvalidLength`]
    /// because the bytes are well-formed and belong to another endpoint.
    MultipleMessages,
    InvalidValue,
    Overflow,
    TrailingData,
    WrongDestination,
    Decryption,
}

impl core::fmt::Display for PropagationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::Truncated => "truncated propagation data",
            Self::InvalidType => "invalid propagation msgpack type",
            Self::InvalidLength => "invalid propagation value length",
            Self::MultipleMessages => "multi-message propagation upload (peer /offer form)",
            Self::InvalidValue => "invalid propagation value",
            Self::Overflow => "propagation value exceeds codec limits",
            Self::TrailingData => "trailing propagation data",
            Self::WrongDestination => "propagation destination does not match",
            Self::Decryption => "propagation decryption failed",
        };
        f.write_str(text)
    }
}

impl core::error::Error for PropagationError {}

impl From<msgpack::Error> for PropagationError {
    fn from(error: msgpack::Error) -> Self {
        match error {
            msgpack::Error::Truncated => Self::Truncated,
            msgpack::Error::Type => Self::InvalidType,
            msgpack::Error::Overflow => Self::Overflow,
            msgpack::Error::Trailing => Self::TrailingData,
            msgpack::Error::Depth => Self::InvalidType,
        }
    }
}

/// Error values returned by a Python-compatible propagation endpoint.
///
/// A propagation client can receive these in either `/get` response phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PeerError {
    NoIdentity = 0xf0,
    NoAccess = 0xf1,
    InvalidKey = 0xf3,
    InvalidData = 0xf4,
    InvalidStamp = 0xf5,
    Throttled = 0xf6,
    NotFound = 0xfd,
    Timeout = 0xfe,
}

impl PeerError {
    /// The on-wire error code a propagation node sends in an error response.
    pub const fn code(self) -> u8 {
        self as u8
    }
}

impl core::fmt::Display for PeerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::NoIdentity => "propagation node has no identity for us",
            Self::NoAccess => "propagation node refused access",
            Self::InvalidKey => "propagation node rejected the key",
            Self::InvalidData => "propagation node rejected the data",
            Self::InvalidStamp => "propagation node rejected the stamp",
            Self::Throttled => "propagation node is throttling us",
            Self::NotFound => "propagation node has no such message",
            Self::Timeout => "propagation node timed out",
        };
        f.write_str(text)
    }
}

impl core::error::Error for PeerError {}

impl TryFrom<u64> for PeerError {
    type Error = PropagationError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            0xf0 => Ok(Self::NoIdentity),
            0xf1 => Ok(Self::NoAccess),
            0xf3 => Ok(Self::InvalidKey),
            0xf4 => Ok(Self::InvalidData),
            0xf5 => Ok(Self::InvalidStamp),
            0xf6 => Ok(Self::Throttled),
            0xfd => Ok(Self::NotFound),
            0xfe => Ok(Self::Timeout),
            _ => Err(PropagationError::InvalidValue),
        }
    }
}

/// Unsolicited Link signalling sent by a propagation node for an upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropagationSignal {
    /// The node rejected one or more messages because the outer propagation
    /// stamp did not satisfy its advertised cost.
    InvalidStamp,
}

impl PropagationSignal {
    /// Serialize the signalling packet a host sends when it refuses an upload
    /// — the propagation-node HOST side (leviculum#38).
    ///
    /// Python answers a client upload carrying an invalid propagation stamp
    /// by packing `LXMPeer.ERROR_INVALID_STAMP`
    /// (reference/LXMF/LXMF/LXMRouter.py:2257-2260) into a raw Link packet and
    /// then tearing the link down. Tearing down is the caller's business;
    /// these are the bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        msgpack::array(&mut output, 1);
        match self {
            Self::InvalidStamp => msgpack::uint(&mut output, PeerError::InvalidStamp.code() as u64),
        }
        output
    }

    /// Decode the Python signalling packet `[LXMPeer.ERROR_INVALID_STAMP]`.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropagationError> {
        let mut position = 0;
        if msgpack::array_len(bytes, &mut position)? != 1 {
            return Err(PropagationError::InvalidLength);
        }
        let signal = match PeerError::try_from(msgpack::read_uint(bytes, &mut position)?)? {
            PeerError::InvalidStamp => Self::InvalidStamp,
            _ => return Err(PropagationError::InvalidValue),
        };
        finish(bytes, position)?;
        Ok(signal)
    }
}

/// Unstamped, destination-encrypted LXMF bytes downloaded from a propagation
/// node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropagatedMessage {
    destination_hash: [u8; DESTINATION_LENGTH],
    encrypted_payload: Vec<u8>,
}

impl PropagatedMessage {
    /// Parse unstamped `destination_hash || encrypted_payload` bytes.
    ///
    /// `LXMF_OVERHEAD` bytes exactly is refused, not accepted: Python holds a
    /// propagated body only when `validate_pn_stamp`
    /// (reference/LXMF/LXMF/LXStamper.py:86) finds the stamped form strictly
    /// longer than `LXMF_OVERHEAD + STAMP_SIZE`, so the unstamped remainder it
    /// keeps is strictly longer than `LXMF_OVERHEAD`.
    pub fn from_unstamped_bytes(bytes: &[u8]) -> Result<Self, PropagationError> {
        if bytes.len() <= LXMF_OVERHEAD {
            return Err(PropagationError::InvalidLength);
        }
        let destination_hash = bytes[..DESTINATION_LENGTH]
            .try_into()
            .map_err(|_| PropagationError::InvalidLength)?;
        Ok(Self {
            destination_hash,
            encrypted_payload: bytes[DESTINATION_LENGTH..].to_vec(),
        })
    }

    /// Decrypt and reconstruct the complete packed LXMF message.
    pub fn decrypt(&self, destination: &Destination) -> Result<Vec<u8>, PropagationError> {
        if self.destination_hash != *destination.hash().as_bytes() {
            return Err(PropagationError::WrongDestination);
        }
        let plaintext = destination
            .decrypt(&self.encrypted_payload)
            .map_err(map_decrypt_error)?;
        let mut packed = Vec::with_capacity(DESTINATION_LENGTH + plaintext.len());
        packed.extend_from_slice(&self.destination_hash);
        packed.extend_from_slice(&plaintext);
        Ok(packed)
    }

    pub const fn destination_hash(&self) -> &[u8; DESTINATION_LENGTH] {
        &self.destination_hash
    }
}

fn map_decrypt_error(_: DestinationError) -> PropagationError {
    PropagationError::Decryption
}

/// Numeric representation accepted for the optional `/get` transfer limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransferLimit {
    Integer(u64),
    Float(f64),
}

/// `/get` request: `[wants, haves]` or `[wants, haves, limit_kb]`.
///
/// Use [`Self::list`] for the initial mailbox listing and
/// [`Self::acknowledge`] after successfully receiving messages. To download
/// messages, set `wants` to the transient IDs selected from
/// [`MessageListResponse`] and `haves` to IDs already held locally.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageGetRequest {
    pub wants: Option<Vec<TransientId>>,
    pub haves: Option<Vec<TransientId>>,
    pub transfer_limit_kb: Option<TransferLimit>,
}

impl MessageGetRequest {
    pub fn list() -> Self {
        Self {
            wants: None,
            haves: None,
            transfer_limit_kb: None,
        }
    }

    pub fn acknowledge(haves: Vec<TransientId>) -> Self {
        Self {
            wants: None,
            haves: Some(haves),
            transfer_limit_kb: None,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, PropagationError> {
        let mut output = Vec::new();
        msgpack::array(
            &mut output,
            if self.transfer_limit_kb.is_some() {
                3
            } else {
                2
            },
        );
        encode_optional_ids(&mut output, self.wants.as_deref());
        encode_optional_ids(&mut output, self.haves.as_deref());
        if let Some(limit) = self.transfer_limit_kb {
            match limit {
                TransferLimit::Integer(value) => msgpack::uint(&mut output, value),
                TransferLimit::Float(value) => msgpack::f64(&mut output, value),
            }
        }
        Ok(output)
    }

    /// Decode a client's `/get` request — the propagation-node HOST side of
    /// the exchange (leviculum#38). Exact inverse of the encoders used by
    /// [`MessageGetRequest::list`] / [`MessageGetRequest::acknowledge`].
    pub fn decode(bytes: &[u8]) -> Result<Self, PropagationError> {
        let mut position = 0;
        let count = msgpack::array_len(bytes, &mut position)?;
        if !(2..=3).contains(&count) {
            return Err(PropagationError::InvalidLength);
        }
        let wants = decode_optional_ids(bytes, &mut position)?;
        let haves = decode_optional_ids(bytes, &mut position)?;
        let transfer_limit_kb = if count == 3 {
            Some(decode_transfer_limit(bytes, &mut position)?)
        } else {
            None
        };
        finish(bytes, position)?;
        Ok(Self {
            wants,
            haves,
            transfer_limit_kb,
        })
    }
}

/// Initial `/get` response: available transient IDs or a propagation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageListResponse {
    TransientIds(Vec<TransientId>),
    Error(PeerError),
}

impl MessageListResponse {
    /// Serialize the node's transient-ID list (or error) response — the
    /// propagation-node HOST side (leviculum#38); clients parse it with
    /// [`MessageListResponse::decode`].
    pub fn encode(&self) -> Result<Vec<u8>, PropagationError> {
        let mut output = Vec::new();
        match self {
            Self::TransientIds(ids) => encode_ids(&mut output, ids),
            Self::Error(error) => msgpack::uint(&mut output, error.code() as u64),
        }
        Ok(output)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PropagationError> {
        let mut position = 0;
        let response = match msgpack::peek_kind(bytes, position)? {
            msgpack::Kind::Array => Self::TransientIds(decode_ids(bytes, &mut position)?),
            _ => Self::Error(PeerError::try_from(msgpack::read_uint(
                bytes,
                &mut position,
            )?)?),
        };
        finish(bytes, position)?;
        Ok(response)
    }
}

/// Download `/get` response: unstamped encrypted LXMF messages or an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageGetResponse {
    Messages(Vec<Vec<u8>>),
    Error(PeerError),
}

impl MessageGetResponse {
    /// Serialize the node's message-download (or error) response — the
    /// propagation-node HOST side (leviculum#38); clients parse it with
    /// [`MessageGetResponse::decode`].
    pub fn encode(&self) -> Result<Vec<u8>, PropagationError> {
        let mut output = Vec::new();
        match self {
            Self::Messages(messages) => encode_binary_list(&mut output, messages),
            Self::Error(error) => msgpack::uint(&mut output, error.code() as u64),
        }
        Ok(output)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PropagationError> {
        let mut position = 0;
        let response = match msgpack::peek_kind(bytes, position)? {
            msgpack::Kind::Array => Self::Messages(decode_binary_list(bytes, &mut position)?),
            _ => Self::Error(PeerError::try_from(msgpack::read_uint(
                bytes,
                &mut position,
            )?)?),
        };
        finish(bytes, position)?;
        Ok(response)
    }
}

/// Seven-field app-data payload announced by propagation nodes.
///
/// LXMF 1.1.0 can encode the transfer and sync limits as MessagePack floats
/// when they originate from configured `lxmd` values. Its announce handler
/// normalizes both fields with Python's `int()`, so decoding accepts integer
/// and floating-point representations and stores the truncated values here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropagationNodeAnnounce {
    pub legacy_support: bool,
    pub timebase: u64,
    pub enabled: bool,
    pub transfer_limit_kb: u64,
    pub sync_limit_kb: u64,
    pub stamp_cost: u64,
    pub stamp_cost_flexibility: u64,
    pub peering_cost: u64,
    /// Msgpack map values are retained as raw msgpack so custom metadata can
    /// round-trip without imposing a schema.
    pub metadata: Vec<MetadataEntry>,
}

impl PropagationNodeAnnounce {
    /// Serialize this node's own announce app-data — the propagation-node
    /// HOST side (leviculum#38); clients parse it with
    /// [`PropagationNodeAnnounce::decode`].
    pub fn encode(&self) -> Result<Vec<u8>, PropagationError> {
        let mut output = Vec::new();
        msgpack::array(&mut output, 7);
        msgpack::bool(&mut output, self.legacy_support);
        msgpack::uint(&mut output, self.timebase);
        msgpack::bool(&mut output, self.enabled);
        msgpack::uint(&mut output, self.transfer_limit_kb);
        msgpack::uint(&mut output, self.sync_limit_kb);
        msgpack::array(&mut output, 3);
        msgpack::uint(&mut output, self.stamp_cost);
        msgpack::uint(&mut output, self.stamp_cost_flexibility);
        msgpack::uint(&mut output, self.peering_cost);
        msgpack::map(&mut output, self.metadata.len());
        for (key, raw_value) in &self.metadata {
            validate_raw_value(raw_value)?;
            msgpack::uint(&mut output, *key);
            msgpack::append_raw(&mut output, raw_value);
        }
        Ok(output)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PropagationError> {
        let mut position = 0;
        let field_count = msgpack::array_len(bytes, &mut position)?;
        if field_count < 7 {
            return Err(PropagationError::InvalidLength);
        }
        let legacy_support = msgpack::read_bool(bytes, &mut position)?;
        let timebase = msgpack::read_uint(bytes, &mut position)?;
        let enabled = msgpack::read_bool(bytes, &mut position)?;
        let transfer_limit_kb = decode_announce_limit(bytes, &mut position)?;
        let sync_limit_kb = decode_announce_limit(bytes, &mut position)?;
        let cost_count = msgpack::array_len(bytes, &mut position)?;
        if cost_count < 3 {
            return Err(PropagationError::InvalidLength);
        }
        let stamp_cost = msgpack::read_uint(bytes, &mut position)?;
        let stamp_cost_flexibility = msgpack::read_uint(bytes, &mut position)?;
        let peering_cost = msgpack::read_uint(bytes, &mut position)?;
        for _ in 3..cost_count {
            msgpack::skip(bytes, &mut position)?;
        }
        let metadata_count = msgpack::map_len(bytes, &mut position)?;
        reserve_guard(bytes, position, metadata_count)?;
        let mut metadata = Vec::with_capacity(metadata_count);
        for _ in 0..metadata_count {
            let key = msgpack::read_uint(bytes, &mut position)?;
            metadata.push((key, msgpack::raw(bytes, &mut position)?.to_vec()));
        }
        for _ in 7..field_count {
            msgpack::skip(bytes, &mut position)?;
        }
        finish(bytes, position)?;
        Ok(Self {
            legacy_support,
            timebase,
            enabled,
            transfer_limit_kb,
            sync_limit_kb,
            stamp_cost,
            stamp_cost_flexibility,
            peering_cost,
            metadata,
        })
    }
}

fn decode_announce_limit(bytes: &[u8], position: &mut usize) -> Result<u64, PropagationError> {
    if msgpack::peek_kind(bytes, *position)? != msgpack::Kind::Float {
        return Ok(msgpack::read_uint(bytes, position)?);
    }

    let value = msgpack::read_number_f64(bytes, position)?;
    if !value.is_finite() || value < 0.0 {
        return Err(PropagationError::InvalidValue);
    }
    if value >= u64::MAX as f64 {
        return Err(PropagationError::Overflow);
    }
    Ok(value as u64)
}

fn validate_raw_value(bytes: &[u8]) -> Result<(), PropagationError> {
    let mut position = 0;
    msgpack::skip(bytes, &mut position)?;
    finish(bytes, position)
}

fn finish(bytes: &[u8], position: usize) -> Result<(), PropagationError> {
    if position == bytes.len() {
        Ok(())
    } else {
        Err(PropagationError::TrailingData)
    }
}

fn reserve_guard(bytes: &[u8], position: usize, count: usize) -> Result<(), PropagationError> {
    if count > bytes.len().saturating_sub(position) {
        Err(PropagationError::InvalidLength)
    } else {
        Ok(())
    }
}

fn encode_ids(output: &mut Vec<u8>, ids: &[TransientId]) {
    msgpack::array(output, ids.len());
    for id in ids {
        msgpack::bin(output, id);
    }
}

fn decode_fixed_bin<const N: usize>(
    bytes: &[u8],
    position: &mut usize,
) -> Result<[u8; N], PropagationError> {
    msgpack::read_bin(bytes, position)?
        .try_into()
        .map_err(|_| PropagationError::InvalidLength)
}

fn decode_ids(bytes: &[u8], position: &mut usize) -> Result<Vec<TransientId>, PropagationError> {
    let count = msgpack::array_len(bytes, position)?;
    reserve_guard(bytes, *position, count)?;
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        ids.push(decode_fixed_bin::<32>(bytes, position)?);
    }
    Ok(ids)
}

fn encode_optional_ids(output: &mut Vec<u8>, ids: Option<&[TransientId]>) {
    if let Some(ids) = ids {
        encode_ids(output, ids);
    } else {
        msgpack::nil(output);
    }
}

fn decode_optional_ids(
    bytes: &[u8],
    position: &mut usize,
) -> Result<Option<Vec<TransientId>>, PropagationError> {
    if msgpack::peek_kind(bytes, *position)? == msgpack::Kind::Nil {
        msgpack::read_nil(bytes, position)?;
        Ok(None)
    } else {
        Ok(Some(decode_ids(bytes, position)?))
    }
}

fn encode_binary_list(output: &mut Vec<u8>, values: &[Vec<u8>]) {
    msgpack::array(output, values.len());
    for value in values {
        msgpack::bin(output, value);
    }
}

fn decode_binary_list(
    bytes: &[u8],
    position: &mut usize,
) -> Result<Vec<Vec<u8>>, PropagationError> {
    let count = msgpack::array_len(bytes, position)?;
    reserve_guard(bytes, *position, count)?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(msgpack::read_bin(bytes, position)?.to_vec());
    }
    Ok(values)
}

fn decode_transfer_limit(
    bytes: &[u8],
    position: &mut usize,
) -> Result<TransferLimit, PropagationError> {
    match msgpack::peek_kind(bytes, *position)? {
        msgpack::Kind::Float => Ok(TransferLimit::Float(msgpack::read_number_f64(
            bytes, position,
        )?)),
        _ => Ok(TransferLimit::Integer(msgpack::read_uint(bytes, position)?)),
    }
}

#[cfg(test)]
#[path = "propagation_tests.rs"]
mod tests;
