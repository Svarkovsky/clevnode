//! Tunnel bookkeeping for path restoration across TCP reconnects (Codeberg #64).
//!
//! Mirrors Python-RNS `RNS.Transport` tunnel handling (Transport.py:119,
//! 2284-2393). A tunnel is a stable association between a transport peer and
//! the paths learned over the connection to it. The peer identity plus its
//! interface hash form a `tunnel_id` that survives socket drops, so when a
//! flapping TCP client reconnects and re-runs the `rnstransport.tunnel.synthesize`
//! handshake, the paths it previously carried are restored immediately without
//! waiting for a fresh announce.
//!
//! Interface isolation: the `tunnel_id` is synthesized by the TCP interface
//! (peer public key + interface hash, both media-specific). The table below and
//! the association/restore logic in `transport` are media-agnostic and keyed
//! only on the opaque `tunnel_id` the interface supplies.

use crate::constants::{RANDOM_HASHBYTES, TRUNCATED_HASHBYTES};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// Tunnel table entries live eight hours if unused (Python `TUNNEL_TIMEOUT`,
/// Transport.py:93).
pub const TUNNEL_TIMEOUT_MS: u64 = 8 * 60 * 60 * 1000;

/// Tunnel path entries live eight hours if unused (Python `TUNNEL_PATH_TIMEOUT`,
/// Transport.py:94).
pub const TUNNEL_PATH_TIMEOUT_MS: u64 = 8 * 60 * 60 * 1000;

/// A `tunnel_id` is a full (untruncated) SHA-256 hash (Python
/// `Identity.full_hash`, Transport.py:2291).
pub const TUNNEL_ID_LEN: usize = 32;

// Wire layout of the `rnstransport.tunnel.synthesize` PLAIN broadcast payload
// (Python Transport.py:2296, 2310-2318). All lengths are byte counts.

/// Peer transport identity public key (`Identity.KEYSIZE//8`).
pub const SYNTH_PUBKEY_LEN: usize = 64;
/// Peer interface hash (`Identity.HASHLENGTH//8`).
pub const SYNTH_IFHASH_LEN: usize = 32;
/// Anti-replay random hash (`Reticulum.TRUNCATED_HASHLENGTH//8`).
pub const SYNTH_RANDHASH_LEN: usize = 16;
/// Ed25519 signature over `pubkey || ifhash || randhash` (`Identity.SIGLENGTH//8`).
pub const SYNTH_SIG_LEN: usize = 64;
/// Total synthesize payload length.
pub const SYNTH_TOTAL_LEN: usize =
    SYNTH_PUBKEY_LEN + SYNTH_IFHASH_LEN + SYNTH_RANDHASH_LEN + SYNTH_SIG_LEN;

/// A path snapshot held against a tunnel so it can be restored on reconnect.
///
/// Mirrors the Python tunnel-path list `[timestamp, received_from, hops,
/// expires, random_blobs, receiving_interface, packet_hash]` (Transport.py:2029),
/// keeping only the fields required to rebuild a path-table entry. The receiving
/// interface is deliberately not stored: on restore the path is re-homed onto
/// whichever interface completed the reconnect handshake.
#[derive(Clone, Debug)]
pub struct TunnelPathEntry {
    /// Hop count to the destination.
    pub hops: u8,
    /// Absolute expiry (ms, clock epoch) inherited from the original path.
    pub expires_ms: u64,
    /// Random blobs seen for this destination (announce emission timebase).
    pub random_blobs: Vec<[u8; RANDOM_HASHBYTES]>,
    /// Next relay hop (announce transport id), if any.
    pub next_hop: Option<[u8; TRUNCATED_HASHBYTES]>,
    /// When this snapshot was last refreshed (ms, clock epoch); drives the
    /// per-path eight-hour timeout.
    pub timestamp_ms: u64,
}

/// A tunnel: a set of paths associated with a reconnectable peer.
///
/// Mirrors the Python tunnel entry `[tunnel_id, interface, paths, expires]`
/// (Transport.py:2345). The interface index is `None` while the tunnel is
/// dormant (peer disconnected); the paths persist so they can be restored when
/// the peer returns.
#[derive(Clone, Debug)]
pub struct TunnelEntry {
    /// Interface index currently carrying this tunnel, or `None` when dormant.
    pub interface_index: Option<usize>,
    /// Paths learned over this tunnel, keyed by destination hash.
    pub paths: BTreeMap<[u8; TRUNCATED_HASHBYTES], TunnelPathEntry>,
    /// Absolute tunnel expiry (ms, clock epoch); refreshed on each association.
    pub expires_ms: u64,
}

impl TunnelEntry {
    /// Create an empty tunnel bound to `interface_index`, expiring at `expires_ms`.
    pub fn new(interface_index: Option<usize>, expires_ms: u64) -> Self {
        Self {
            interface_index,
            paths: BTreeMap::new(),
            expires_ms,
        }
    }
}

/// Borrowed view over a parsed synthesize payload, prior to signature validation.
pub struct SynthesizePayload<'a> {
    /// Peer transport identity public key (64 bytes).
    pub public_key: &'a [u8],
    /// Peer interface hash (32 bytes).
    pub interface_hash: &'a [u8],
    /// Anti-replay random hash (16 bytes).
    pub random_hash: &'a [u8],
    /// Signature over `public_key || interface_hash || random_hash` (64 bytes).
    pub signature: &'a [u8],
}

impl<'a> SynthesizePayload<'a> {
    /// Split a raw synthesize payload into its fields.
    ///
    /// Returns `None` if `data` is not exactly [`SYNTH_TOTAL_LEN`] bytes, matching
    /// the Python length gate (Transport.py:2311).
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        if data.len() != SYNTH_TOTAL_LEN {
            return None;
        }
        let (public_key, rest) = data.split_at(SYNTH_PUBKEY_LEN);
        let (interface_hash, rest) = rest.split_at(SYNTH_IFHASH_LEN);
        let (random_hash, signature) = rest.split_at(SYNTH_RANDHASH_LEN);
        Some(Self {
            public_key,
            interface_hash,
            random_hash,
            signature,
        })
    }

    /// The bytes covered by the signature: `public_key || interface_hash ||
    /// random_hash` (Python `signed_data`, Transport.py:2319).
    pub fn signed_data(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SYNTH_PUBKEY_LEN + SYNTH_IFHASH_LEN + SYNTH_RANDHASH_LEN);
        out.extend_from_slice(self.public_key);
        out.extend_from_slice(self.interface_hash);
        out.extend_from_slice(self.random_hash);
        out
    }
}

/// Derive the `tunnel_id` from a peer public key and interface hash.
///
/// `tunnel_id = full_hash(public_key || interface_hash)` (Python
/// Transport.py:2290-2291). Media-agnostic: the caller supplies the two byte
/// strings; the interface owns where they come from.
pub fn compute_tunnel_id(public_key: &[u8], interface_hash: &[u8]) -> [u8; TUNNEL_ID_LEN] {
    let mut buf = Vec::with_capacity(public_key.len() + interface_hash.len());
    buf.extend_from_slice(public_key);
    buf.extend_from_slice(interface_hash);
    crate::crypto::full_hash(&buf)
}

/// Build the payload of a `rnstransport.tunnel.synthesize` packet.
///
/// Layout matches Python `synthesize_tunnel` (Transport.py:2290-2296):
/// `public_key || interface_hash || random_hash || signature`, where the
/// signature covers `public_key || interface_hash || random_hash`. The caller
/// supplies the local transport identity (for `public_key` and the signature),
/// the media-specific `interface_hash`, and a fresh 16-byte `random_hash`.
pub fn build_synthesize_payload(
    identity: &crate::identity::Identity,
    interface_hash: &[u8; SYNTH_IFHASH_LEN],
    random_hash: &[u8; SYNTH_RANDHASH_LEN],
) -> Result<Vec<u8>, crate::identity::IdentityError> {
    let public_key = identity.public_key_bytes();
    let mut signed = Vec::with_capacity(SYNTH_PUBKEY_LEN + SYNTH_IFHASH_LEN + SYNTH_RANDHASH_LEN);
    signed.extend_from_slice(&public_key);
    signed.extend_from_slice(interface_hash);
    signed.extend_from_slice(random_hash);

    let signature = identity.sign(&signed)?;

    let mut out = Vec::with_capacity(SYNTH_TOTAL_LEN);
    out.extend_from_slice(&signed);
    out.extend_from_slice(&signature);
    Ok(out)
}
