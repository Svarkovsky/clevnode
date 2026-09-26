//! Outgoing resource state machine (sender side).
//!
//! Sans-I/O: receives packets via methods, returns raw packet bytes to send.
//! Owned by Link (same pattern as Channel).

use alloc::vec;
use alloc::vec::Vec;

use rand_core::CryptoRngCore;

#[cfg(feature = "compression")]
use crate::constants::RESOURCE_AUTO_COMPRESS_MAX;
use crate::constants::{RESOURCE_HASHMAP_LEN, RESOURCE_WINDOW_MAX_FAST};
use crate::crypto::full_hash;
use crate::hex_fmt::HexFmt;
use crate::link::Link;
use crate::packet::PacketContext;
use crate::resource::hashmap::map_hash;
use crate::resource::msgpack;
use crate::resource::{
    resource_sdu, ResourceAdvertisement, ResourceError, ResourceFlags, ResourceStatus,
    COLLISION_GUARD_SIZE, HASHMAP_IS_EXHAUSTED, HASHMAP_MAX_LEN, PART_TIMEOUT_FACTOR_AFTER_RTT,
    PART_TIMEOUT_FACTOR_INITIAL, PER_RETRY_DELAY_MS, PROCESSING_GRACE_MS, PROOF_TIMEOUT_FACTOR,
    RESOURCE_MAX_ADV_RETRIES, RESOURCE_MAX_EFFICIENT_SIZE, RESOURCE_MAX_RETRIES,
    RESOURCE_RANDOM_HASH_SIZE, SENDER_GRACE_TIME_MS,
};

/// Per-segment parameters for a (possibly multi-segment) outgoing resource.
///
/// A resource whose combined `metadata + data` exceeds
/// [`RESOURCE_MAX_EFFICIENT_SIZE`](crate::resource::RESOURCE_MAX_EFFICIENT_SIZE)
/// is split into `total_segments` independent resource transfers, each with its
/// own hash, hashmap and proof. All segments carry the same `original_hash`
/// (the first segment's resource hash) and the same total `data_size`, matching
/// Python `RNS.Resource` segmentation so a Python `rncp` receiver reassembles
/// the file.
/// The link-derived inputs a resource build needs, detached from the [`Link`]
/// so the CPU-heavy build (compress + encrypt + hash) can run **outside** the
/// node lock (leviculum#29). Captured under the lock via
/// `Link::resource_crypt_params()`, consumed off-lock by
/// [`prepare_resource_send`](crate::resource::prepare_resource_send);
/// [`commit_resource_send`](crate::node::NodeCore::commit_resource_send)
/// re-validates the token key so a mid-build re-key can never ship ciphertext
/// under a stale key.
pub struct ResourceCryptParams {
    pub(crate) active: bool,
    pub(crate) negotiated_mtu: u32,
    pub(crate) mdu: usize,
    pub(crate) token_key: Option<[u8; 64]>,
}

impl ResourceCryptParams {
    /// Token-encrypt `plaintext` exactly as [`Link::encrypt`] would: fresh IV
    /// from `rng`, then `encrypt_token` under the captured link key.
    pub(crate) fn encrypt(
        &self,
        plaintext: &[u8],
        output: &mut [u8],
        rng: &mut impl CryptoRngCore,
    ) -> Result<usize, crate::link::LinkError> {
        use crate::link::LinkError;
        let token_key = self.token_key.as_ref().ok_or(LinkError::InvalidState)?;
        let mut iv = [0u8; 16];
        rng.fill_bytes(&mut iv);
        crate::crypto::encrypt_token(token_key, &iv, plaintext, output)
            .map_err(|_| LinkError::KeyExchangeFailed)
    }
}

/// Everything [`prepare_resource_send`] needs, snapshotted under the node lock
/// by [`NodeCore::resource_send_params`](crate::node::NodeCore::resource_send_params).
/// Opaque: fields are crate-internal so the capture/commit contract can evolve.
pub struct ResourceSendParams {
    pub(crate) crypt: ResourceCryptParams,
    pub(crate) link_id: crate::link::LinkId,
    pub(crate) now_ms: u64,
}

/// A fully-built (compressed + encrypted + hashed) outgoing resource transfer,
/// produced OFF the node lock by [`prepare_resource_send`] and installed under
/// a brief lock by
/// [`NodeCore::commit_resource_send`](crate::node::NodeCore::commit_resource_send).
pub struct PreparedResourceSend {
    pub(crate) kind: PreparedSendKind,
    /// The token key the ciphertext was built under; commit refuses to install
    /// if the link has re-keyed since (the peer could never decrypt it).
    pub(crate) token_key: [u8; 64],
    pub(crate) link_id: crate::link::LinkId,
    pub(crate) resource_hash: [u8; 32],
    pub(crate) adv_bytes: Vec<u8>,
}

pub(crate) enum PreparedSendKind {
    Single(OutgoingResource),
    Split {
        segment1: OutgoingResource,
        plan: OutgoingSegmentPlan,
    },
}

/// Build an outgoing resource transfer WITHOUT touching the node: the bulk
/// compress + encrypt + full/map hashing runs against the snapshotted
/// [`ResourceSendParams`], so the caller (the std driver) does the CPU work
/// outside the node mutex (leviculum#29). Mirrors the single-vs-split logic of
/// `NodeCore::send_resource` exactly.
pub fn prepare_resource_send(
    params: &ResourceSendParams,
    data: &[u8],
    metadata: Option<&[u8]>,
    auto_compress: bool,
    rng: &mut impl CryptoRngCore,
) -> Result<PreparedResourceSend, ResourceError> {
    use crate::resource::RESOURCE_MAX_EFFICIENT_SIZE;

    let token_key = params.crypt.token_key.ok_or(ResourceError::LinkNotActive)?;

    // The split boundary is the combined metadata+data length, matching
    // Python (`metadata_size + len(data) > MAX_EFFICIENT_SIZE`). The
    // metadata block is a 3-byte length prefix plus the metadata bytes.
    let metadata_size = metadata.map(|m| 3 + m.len()).unwrap_or(0);
    let total_size = metadata_size + data.len();

    if total_size <= RESOURCE_MAX_EFFICIENT_SIZE {
        let outgoing = OutgoingResource::new(
            data,
            metadata,
            None,
            &params.crypt,
            auto_compress,
            rng,
            params.now_ms,
        )?;
        Ok(PreparedResourceSend {
            resource_hash: *outgoing.resource_hash(),
            adv_bytes: outgoing.adv_packet().to_vec(),
            kind: PreparedSendKind::Single(outgoing),
            token_key,
            link_id: params.link_id,
        })
    } else {
        let total_segments = segment_count(total_size);
        let mut plan = OutgoingSegmentPlan::new(
            data.to_vec(),
            metadata.map(|m| m.to_vec()),
            metadata_size,
            total_segments,
            auto_compress,
        );
        let segment1 = plan.build_segment(1, &params.crypt, rng, params.now_ms)?;
        let resource_hash = *segment1.resource_hash();
        let adv_bytes = segment1.adv_packet().to_vec();
        // All later segments carry segment 1's resource hash as `o`.
        plan.set_original_hash(resource_hash);
        Ok(PreparedResourceSend {
            resource_hash,
            adv_bytes,
            kind: PreparedSendKind::Split { segment1, plan },
            token_key,
            link_id: params.link_id,
        })
    }
}

pub(crate) struct SegmentParams {
    /// 1-based index of this segment.
    pub segment_index: u32,
    /// Total number of segments in the transfer.
    pub total_segments: u32,
    /// Shared `o` advertisement field for the whole transfer.
    ///
    /// `None` for a fresh resource (single-segment, or segment 1 of a split),
    /// which carries its own salted resource hash as `o` like the reference
    /// (Resource.py:445-446). `Some(h)` pins a later segment to segment 1's
    /// resource hash — the per-transfer group key a Python receiver uses as
    /// the on-disk reassembly filename.
    pub original_hash: Option<[u8; 32]>,
    /// Total logical stream size (`metadata_size + full data size`) for the `d`
    /// advertisement field. `None` uses this segment's own combined length
    /// (single-segment behaviour).
    pub total_data_size: Option<u64>,
    /// Force the `has_metadata` advertisement flag on even when no metadata
    /// bytes are prepended. Python sets it for every segment of a split
    /// resource; the receiver only strips metadata on segment 1.
    pub force_has_metadata: bool,
}

impl SegmentParams {
    /// Parameters for a standalone, single-segment resource (legacy behaviour).
    pub(crate) fn single() -> Self {
        Self {
            segment_index: 1,
            total_segments: 1,
            original_hash: None,
            total_data_size: None,
            force_has_metadata: false,
        }
    }
}

/// Result of polling an outgoing resource for timeout.
#[derive(Debug)]
pub(crate) enum ResourcePollResult {
    /// No action needed.
    Nothing,
    /// Re-send advertisement packet.
    RetransmitAdv(Vec<u8>),
    /// Send CacheRequest for the expected proof.
    /// Contains proof_data: [resource_hash:32][expected_proof:32].
    RequestProof { proof_data: Vec<u8> },
    /// Transfer has timed out, should be failed.
    TimedOut,
}

/// Outgoing resource transfer state machine.
///
/// Fields like `flags`, `original_hash`, `random_hash`, `uncompressed_size`,
/// `total_hashmap_segments`, `request_id`, and `sdu` are stored during resource
/// creation and needed for protocol correctness (advertisement reconstruction,
/// hash verification, hashmap segment calculation). Accessors are provided and
/// exercised in tests; production callers tracked in Codeberg issues #27/#28.
#[allow(dead_code)] // Protocol state fields — see Codeberg issues #27/#28
pub(crate) struct OutgoingResource {
    status: ResourceStatus,
    flags: ResourceFlags,
    resource_hash: [u8; 32],
    original_hash: [u8; 32],
    random_hash: [u8; RESOURCE_RANDOM_HASH_SIZE],
    encrypted_data: Vec<u8>,
    expected_proof: [u8; 32],
    uncompressed_size: u64,
    parts: Vec<Vec<u8>>,
    hashmap: Vec<[u8; RESOURCE_HASHMAP_LEN]>,
    num_parts: u32,
    /// Which parts have been transmitted at least once. Retransmissions must
    /// not count toward completion: the AwaitingProof transition requires
    /// every DISTINCT part sent, mirroring Python's first-send-only
    /// `sent_parts` (Resource.py:1013-1015).
    sent_mask: Vec<bool>,
    receiver_min_consecutive_height: usize,
    total_hashmap_segments: u32,
    window: usize,
    req_received: bool,
    retries: usize,
    adv_retries: usize,
    last_activity_ms: u64,
    /// Explicit advertisement timeout for request Resources.
    ///
    /// Python passes the request receipt timeout into `Resource(timeout=...)`.
    /// Generic Resources leave this unset and derive the timeout from Link RTT.
    advertisement_timeout_ms: Option<u64>,
    request_id: Option<Vec<u8>>,
    adv_packet: Vec<u8>,
    link_mdu: usize,
    sdu: usize,
}

impl OutgoingResource {
    /// Create a new outgoing resource for transfer over a link.
    ///
    /// # Arguments
    /// * `data` - Application data to send
    /// * `metadata` - Optional metadata (msgpack-encoded by caller)
    /// * `request_id` - Optional request ID for request/response pairing
    /// * `link` - The link to send over (must be Active)
    /// * `rng` - Random number generator
    /// * `now_ms` - Current time in milliseconds
    pub(crate) fn new(
        data: &[u8],
        metadata: Option<&[u8]>,
        request_id: Option<&[u8]>,
        crypt: &ResourceCryptParams,
        auto_compress: bool,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
    ) -> Result<Self, ResourceError> {
        Self::new_with_flags(
            data,
            metadata,
            request_id,
            crypt,
            auto_compress,
            rng,
            now_ms,
            false,
            None,
            SegmentParams::single(),
        )
    }

    /// Create an outgoing request Resource with its request timeout applied to
    /// the upload advertisement watchdog.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_request(
        data: &[u8],
        request_id: &[u8],
        link: &Link,
        auto_compress: bool,
        timeout_ms: u64,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
    ) -> Result<Self, ResourceError> {
        Self::new_with_flags(
            data,
            None,
            Some(request_id),
            &link.resource_crypt_params(),
            auto_compress,
            rng,
            now_ms,
            false,
            Some(timeout_ms),
            SegmentParams::single(),
        )
    }

    /// Create one segment of a multi-segment resource transfer.
    ///
    /// `data` is this segment's slice of the file (metadata is prepended only
    /// on segment 1, driven by `metadata` being `Some`). `seg` carries the
    /// shared advertisement fields (`o`, `d`, `l`) and this segment's index so
    /// a Python `rncp` receiver reassembles the file across segments.
    pub(crate) fn new_segment(
        data: &[u8],
        metadata: Option<&[u8]>,
        crypt: &ResourceCryptParams,
        auto_compress: bool,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
        seg: SegmentParams,
    ) -> Result<Self, ResourceError> {
        Self::new_with_flags(
            data,
            metadata,
            None,
            crypt,
            auto_compress,
            rng,
            now_ms,
            false,
            None,
            seg,
        )
    }

    /// Like [`new`](Self::new) but marks the advertisement as a response
    /// resource (`is_response` flag set, `request_id` carried in the ADV).
    ///
    /// Mirrors Python `RNS.Resource(..., is_response=True, request_id=...)`
    /// (`Link.py` response path): a request whose packed response exceeds the
    /// link MDU is delivered as a Resource the receiver correlates to its
    /// pending request. Used by the request/response responder for large
    /// `/status`-style bundles.
    pub(crate) fn new_response(
        data: &[u8],
        metadata: Option<&[u8]>,
        request_id: Option<&[u8]>,
        crypt: &ResourceCryptParams,
        auto_compress: bool,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
    ) -> Result<Self, ResourceError> {
        Self::new_with_flags(
            data,
            metadata,
            request_id,
            crypt,
            auto_compress,
            rng,
            now_ms,
            true,
            None,
            SegmentParams::single(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_flags(
        data: &[u8],
        metadata: Option<&[u8]>,
        request_id: Option<&[u8]>,
        crypt: &ResourceCryptParams,
        auto_compress: bool,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
        is_response: bool,
        advertisement_timeout_ms: Option<u64>,
        seg: SegmentParams,
    ) -> Result<Self, ResourceError> {
        if !crypt.active {
            return Err(ResourceError::LinkNotActive);
        }

        let sdu = resource_sdu(crypt.negotiated_mtu);
        let link_mdu = crypt.mdu;

        // Build combined = metadata_prefix + data
        // Python line 264: struct.pack(">I", metadata_size)[1:] + packed_metadata
        let mut combined = Vec::new();
        // Metadata bytes are prepended only when present (segment 1). The
        // has_metadata *flag* may still be set on later segments of a split
        // resource (Python sets it for every segment); the receiver only strips
        // metadata on segment 1.
        let has_metadata = metadata.is_some() || seg.force_has_metadata;
        if let Some(meta) = metadata {
            let meta_len = meta.len();
            // 3-byte big-endian length (high 3 bytes of u32)
            combined.push((meta_len >> 16) as u8);
            combined.push((meta_len >> 8) as u8);
            combined.push(meta_len as u8);
            combined.extend_from_slice(meta);
        }
        combined.extend_from_slice(data);

        let uncompressed_size = combined.len() as u64;

        // Try compression
        #[allow(unused_mut)]
        let mut compressed = false;
        let data_to_encrypt = {
            #[cfg(feature = "compression")]
            {
                if auto_compress && combined.len() <= RESOURCE_AUTO_COMPRESS_MAX {
                    match super::compression::bz2_compress(&combined) {
                        Ok(compressed_data) if compressed_data.len() < combined.len() => {
                            compressed = true;
                            compressed_data
                        }
                        _ => combined.clone(),
                    }
                } else {
                    combined.clone()
                }
            }
            #[cfg(not(feature = "compression"))]
            {
                let _ = auto_compress;
                combined.clone()
            }
        };

        // Generate wire random (prepended, not stored)
        let mut wire_random = [0u8; RESOURCE_RANDOM_HASH_SIZE];
        rng.fill_bytes(&mut wire_random);

        // Build plaintext: wire_random + data_to_encrypt
        let mut plaintext = Vec::with_capacity(RESOURCE_RANDOM_HASH_SIZE + data_to_encrypt.len());
        plaintext.extend_from_slice(&wire_random);
        plaintext.extend_from_slice(&data_to_encrypt);

        // Encrypt via link
        let enc_size = Link::encrypted_size(plaintext.len());
        let mut encrypted = vec![0u8; enc_size];
        let written = crypt
            .encrypt(&plaintext, &mut encrypted, rng)
            .map_err(|_| ResourceError::CryptoError)?;
        encrypted.truncate(written);

        // Generate verification random_hash (stored, sent in ADV "r" field)
        let mut random_hash = [0u8; RESOURCE_RANDOM_HASH_SIZE];
        rng.fill_bytes(&mut random_hash);

        // resource_hash = full_hash(combined + random_hash), uses UNENCRYPTED combined
        let mut hash_input = Vec::with_capacity(combined.len() + RESOURCE_RANDOM_HASH_SIZE);
        hash_input.extend_from_slice(&combined);
        hash_input.extend_from_slice(&random_hash);
        let resource_hash_full = full_hash(&hash_input);
        let mut resource_hash = [0u8; 32];
        resource_hash.copy_from_slice(&resource_hash_full);

        // expected_proof = full_hash(combined + resource_hash), precomputed
        let mut proof_input = Vec::with_capacity(combined.len() + 32);
        proof_input.extend_from_slice(&combined);
        proof_input.extend_from_slice(&resource_hash);
        let expected_proof_full = full_hash(&proof_input);
        let mut expected_proof = [0u8; 32];
        expected_proof.copy_from_slice(&expected_proof_full);

        // Segment encrypted data into parts
        let num_parts = if encrypted.is_empty() {
            1
        } else {
            encrypted.len().div_ceil(sdu) as u32
        };

        let guard_size = COLLISION_GUARD_SIZE;
        let hashmap_max = HASHMAP_MAX_LEN;

        // Build parts and hashmap, retrying if hash collisions occur
        let (parts, hashmap) = loop {
            let mut parts = Vec::with_capacity(num_parts as usize);
            let mut hashmap_entries = Vec::with_capacity(num_parts as usize);
            let mut collision_guard: Vec<[u8; RESOURCE_HASHMAP_LEN]> = Vec::new();
            let mut collision_found = false;

            for i in 0..num_parts as usize {
                let start = i * sdu;
                let end = core::cmp::min(start + sdu, encrypted.len());
                let part_data = &encrypted[start..end];

                let mh = map_hash(part_data, &random_hash);

                if collision_guard.contains(&mh) {
                    // Collision, regenerate random_hash and retry
                    rng.fill_bytes(&mut random_hash);

                    // Recompute resource_hash and expected_proof with new random_hash
                    let mut hi = Vec::with_capacity(combined.len() + RESOURCE_RANDOM_HASH_SIZE);
                    hi.extend_from_slice(&combined);
                    hi.extend_from_slice(&random_hash);
                    let rh = full_hash(&hi);
                    resource_hash.copy_from_slice(&rh);

                    let mut pi = Vec::with_capacity(combined.len() + 32);
                    pi.extend_from_slice(&combined);
                    pi.extend_from_slice(&resource_hash);
                    let ep = full_hash(&pi);
                    expected_proof.copy_from_slice(&ep);

                    collision_found = true;
                    break;
                }

                collision_guard.push(mh);
                if collision_guard.len() > guard_size {
                    collision_guard.remove(0);
                }

                hashmap_entries.push(mh);
                parts.push(part_data.to_vec());
            }

            if !collision_found {
                break (parts, hashmap_entries);
            }
        };

        // Decide the shared `o` field now that resource_hash is final.
        // - explicit override: a later segment carrying segment 1's hash.
        // - no override: a fresh resource carries its own salted hash, like
        //   Python's `original_hash = self.hash` (Resource.py:445-446). Never
        //   a deterministic content hash: the receiver keys its on-disk
        //   reassembly file by `o` and appends, so a repeatable value lets two
        //   transfers of identical content share one path (Codeberg #165).
        let original_hash = match seg.original_hash {
            Some(h) => h,
            None => resource_hash,
        };

        // Advertisement `d` field: total logical size across all segments
        // (Python sends total_size on every segment), or this segment's own
        // combined length for a single-segment transfer.
        let adv_data_size = seg.total_data_size.unwrap_or(uncompressed_size);

        // Calculate hashmap segments
        let total_hashmap_segments = if hashmap_max == 0 {
            1
        } else {
            hashmap.len().div_ceil(hashmap_max) as u32
        };

        // Build first hashmap segment for advertisement
        let first_segment_end = core::cmp::min(hashmap_max, hashmap.len());
        let mut hashmap_data = Vec::with_capacity(first_segment_end * RESOURCE_HASHMAP_LEN);
        for entry in &hashmap[..first_segment_end] {
            hashmap_data.extend_from_slice(entry);
        }

        // Build flags
        let flags = ResourceFlags {
            encrypted: true,
            compressed,
            split: seg.total_segments > 1,
            // Python marks a Resource carrying a request_id as a request unless
            // it was explicitly constructed as a response.
            is_request: request_id.is_some() && !is_response,
            is_response,
            has_metadata,
        };

        // Build and cache advertisement
        let adv = ResourceAdvertisement {
            transfer_size: encrypted.len() as u64,
            data_size: adv_data_size,
            num_parts,
            resource_hash,
            random_hash,
            original_hash,
            segment_index: seg.segment_index,
            total_segments: seg.total_segments,
            request_id: request_id.map(|r| r.to_vec()),
            flags,
            hashmap_data,
        };
        let adv_packet = adv.pack();

        let sent_mask = vec![false; parts.len()];
        Ok(Self {
            status: ResourceStatus::Advertised,
            flags,
            resource_hash,
            original_hash,
            random_hash,
            encrypted_data: encrypted,
            expected_proof,
            uncompressed_size,
            parts,
            hashmap,
            num_parts,
            sent_mask,
            receiver_min_consecutive_height: 0,
            total_hashmap_segments,
            window: crate::constants::RESOURCE_WINDOW_INITIAL,
            req_received: false,
            retries: 0,
            adv_retries: 0,
            last_activity_ms: now_ms,
            advertisement_timeout_ms,
            request_id: request_id.map(|r| r.to_vec()),
            adv_packet,
            link_mdu,
            sdu,
        })
    }

    /// Anchor the activity clock at `now_ms`.
    ///
    /// A resource built off-lock carries the timestamp of the phase-1 capture
    /// (`ResourceSendParams::now_ms`), but its advertisement only goes out when
    /// [`NodeCore::commit_resource_send`](crate::node::NodeCore::commit_resource_send)
    /// installs it. Whatever time passed in between belongs to the caller's
    /// queue, not to the receiver's response window, so the commit rebases the
    /// clock — otherwise the first poll can charge an advertisement retry
    /// before a byte has moved (issue #196, S4).
    pub(crate) fn rebase_activity_clock(&mut self, now_ms: u64) {
        self.last_activity_ms = now_ms;
    }

    /// Handle a REQ packet from the receiver.
    ///
    /// Returns raw data packets to send (RESOURCE context, pre-encrypted).
    /// May also return an HMU packet if the receiver's hashmap is exhausted.
    pub(crate) fn handle_request(
        &mut self,
        req_data: &[u8],
        link: &Link,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
    ) -> Result<Vec<Vec<u8>>, ResourceError> {
        if self.status == ResourceStatus::Failed {
            crate::tracing::warn!(
                event = "RESOURCE_REQ_ERR",
                rh = %HexFmt(&self.resource_hash[..4]),
                reason = "cancelled",
            );
            return Err(ResourceError::Cancelled);
        }

        // Parse REQ wire format: [1:exhausted_flag][4?:last_map_hash][32:resource_hash][N*4:requested_hashes]
        if req_data.is_empty() {
            crate::tracing::warn!(
                event = "RESOURCE_REQ_ERR",
                rh = %HexFmt(&self.resource_hash[..4]),
                reason = "empty_req",
            );
            return Err(ResourceError::InvalidRequest);
        }

        let wants_more_hashmap = req_data[0] == HASHMAP_IS_EXHAUSTED;
        let pad = if wants_more_hashmap {
            1 + RESOURCE_HASHMAP_LEN
        } else {
            1
        };

        // resource_hash starts at offset `pad`
        if req_data.len() < pad + 32 {
            crate::tracing::warn!(
                event = "RESOURCE_REQ_ERR",
                rh = %HexFmt(&self.resource_hash[..4]),
                reason = "truncated_req",
                len = req_data.len(),
            );
            return Err(ResourceError::InvalidRequest);
        }

        let req_resource_hash = &req_data[pad..pad + 32];
        if req_resource_hash != self.resource_hash {
            crate::tracing::warn!(
                event = "RESOURCE_REQ_ERR",
                rh = %HexFmt(&self.resource_hash[..4]),
                reason = "hash_mismatch",
                req_rh = %HexFmt(&req_resource_hash[..4]),
            );
            return Err(ResourceError::InvalidRequest);
        }

        // Transition to transferring on first REQ
        if self.status == ResourceStatus::Advertised {
            self.status = ResourceStatus::Transferring;
            crate::tracing::debug!(
                event = "RESOURCE_TX_STATE",
                rh = %HexFmt(&self.resource_hash[..4]),
                status = ?self.status,
                retries = self.retries,
            );
        }

        self.req_received = true;
        self.retries = 0;
        self.last_activity_ms = now_ms;

        let mut packets = Vec::new();

        // Parse requested map hashes
        let requested_hashes_data = &req_data[pad + 32..];
        let num_requested = requested_hashes_data.len() / RESOURCE_HASHMAP_LEN;

        let mut requested_hashes = Vec::with_capacity(num_requested);
        for i in 0..num_requested {
            let start = i * RESOURCE_HASHMAP_LEN;
            let mut mh = [0u8; RESOURCE_HASHMAP_LEN];
            mh.copy_from_slice(&requested_hashes_data[start..start + RESOURCE_HASHMAP_LEN]);
            requested_hashes.push(mh);
        }

        // Search within collision guard scope for matching parts
        let search_start = self.receiver_min_consecutive_height;
        let search_end = core::cmp::min(search_start + COLLISION_GUARD_SIZE, self.parts.len());

        // Measurement-only counters for the RESOURCE_REQ_RX event (#85).
        let mut matched: usize = 0;
        let mut first_req_idx: isize = -1;

        for i in search_start..search_end {
            if requested_hashes.contains(&self.hashmap[i]) {
                // Build raw data packet (no per-packet encryption)
                let raw_pkt = link
                    .build_raw_data_packet(&self.parts[i], PacketContext::Resource)
                    .map_err(|_| {
                        crate::tracing::warn!(
                            event = "RESOURCE_REQ_ERR",
                            rh = %HexFmt(&self.resource_hash[..4]),
                            reason = "link_not_active_part",
                            idx = i,
                        );
                        ResourceError::LinkNotActive
                    })?;
                packets.push(raw_pkt);
                self.sent_mask[i] = true;
                if first_req_idx < 0 {
                    first_req_idx = i as isize;
                }
                matched += 1;
            }
        }

        // Handle hashmap exhaustion, send next HMU segment
        if wants_more_hashmap {
            let last_map_hash = &req_data[1..1 + RESOURCE_HASHMAP_LEN];
            let hashmap_max = HASHMAP_MAX_LEN;

            // Find the part index matching last_map_hash
            let mut part_index = self.receiver_min_consecutive_height;
            let scan_end = core::cmp::min(
                self.receiver_min_consecutive_height + COLLISION_GUARD_SIZE,
                self.hashmap.len(),
            );
            for i in self.receiver_min_consecutive_height..scan_end {
                part_index = i + 1;
                if self.hashmap[i] == *last_map_hash {
                    break;
                }
            }

            // Update receiver_min_consecutive_height
            self.receiver_min_consecutive_height =
                part_index.saturating_sub(1 + RESOURCE_WINDOW_MAX_FAST);

            if let Some(segment) = part_index.checked_div(hashmap_max) {
                let hashmap_start = segment * hashmap_max;
                let hashmap_end = core::cmp::min((segment + 1) * hashmap_max, self.hashmap.len());

                // Build hashmap bytes for this segment
                let mut hashmap_bytes =
                    Vec::with_capacity((hashmap_end - hashmap_start) * RESOURCE_HASHMAP_LEN);
                for entry in &self.hashmap[hashmap_start..hashmap_end] {
                    hashmap_bytes.extend_from_slice(entry);
                }

                // Build HMU: resource_hash + msgpack([segment, hashmap_bytes])
                let mut hmu = Vec::with_capacity(32 + 10 + hashmap_bytes.len());
                hmu.extend_from_slice(&self.resource_hash);
                msgpack::write_fixarray_header(&mut hmu, 2);
                msgpack::write_uint(&mut hmu, segment as u64);
                msgpack::write_bin(&mut hmu, &hashmap_bytes);

                // Wrap in encrypted link packet
                let hmu_pkt = link
                    .build_data_packet_with_context(&hmu, PacketContext::ResourceHmu, rng)
                    .map_err(|_| {
                        crate::tracing::warn!(
                            event = "RESOURCE_REQ_ERR",
                            rh = %HexFmt(&self.resource_hash[..4]),
                            reason = "link_not_active_hmu",
                        );
                        ResourceError::LinkNotActive
                    })?;
                packets.push(hmu_pkt);
            }
        }

        // Check if all distinct parts sent at least once → AwaitingProof
        if self.distinct_parts_sent() == self.parts.len() {
            self.status = ResourceStatus::AwaitingProof;
            self.retries = 0;
            crate::tracing::debug!(
                event = "RESOURCE_TX_STATE",
                rh = %HexFmt(&self.resource_hash[..4]),
                status = ?self.status,
                retries = self.retries,
            );
        }

        // One structured event per received REQ (#85): what the REQ asked
        // for, what was resent, and where the sender state stands.
        crate::tracing::debug!(
            event = "RESOURCE_REQ_RX",
            rh = %HexFmt(&self.resource_hash[..4]),
            n_req = num_requested,
            matched = matched,
            first_req_idx = first_req_idx,
            distinct_sent = self.distinct_parts_sent(),
            num_parts = self.parts.len(),
            status = ?self.status,
        );
        if matched == 0 && num_requested > 0 {
            crate::tracing::warn!(
                event = "RESOURCE_REQ_NO_MATCH",
                rh = %HexFmt(&self.resource_hash[..4]),
                n_req = num_requested,
                search_start = search_start,
                search_end = search_end,
            );
        }

        Ok(packets)
    }

    /// Validate a proof from the receiver.
    ///
    /// Returns `Ok(ResourceStatus::Complete)` if valid.
    pub(crate) fn handle_proof(
        &mut self,
        proof_data: &[u8],
    ) -> Result<ResourceStatus, ResourceError> {
        if self.status == ResourceStatus::Failed {
            return Err(ResourceError::Cancelled);
        }

        // Proof format: [32: resource_hash] [32: proof_hash]
        if proof_data.len() != 64 {
            return Err(ResourceError::InvalidProof);
        }

        let proof_resource_hash = &proof_data[..32];
        let proof_hash = &proof_data[32..];

        if proof_resource_hash != self.resource_hash {
            return Err(ResourceError::InvalidProof);
        }

        if proof_hash != self.expected_proof {
            return Err(ResourceError::InvalidProof);
        }

        self.status = ResourceStatus::Complete;
        crate::tracing::debug!(
            event = "RESOURCE_TX_STATE",
            rh = %HexFmt(&self.resource_hash[..4]),
            status = ?self.status,
            retries = self.retries,
        );
        Ok(ResourceStatus::Complete)
    }

    /// Poll for timeout. Called periodically by the timeout handler.
    pub(crate) fn poll(&mut self, now_ms: u64, rtt_ms: u64) -> ResourcePollResult {
        let rtt_ms = core::cmp::max(rtt_ms, 1);

        match self.status {
            ResourceStatus::Advertised => {
                // Python Resource.py:571: timeout + PROCESSING_GRACE
                let timeout = self
                    .advertisement_timeout_ms
                    .unwrap_or_else(|| rtt_ms.saturating_mul(6))
                    .saturating_add(PROCESSING_GRACE_MS);
                if now_ms.saturating_sub(self.last_activity_ms) >= timeout {
                    self.adv_retries += 1;
                    crate::tracing::debug!(
                        event = "RESOURCE_TX_STATE",
                        rh = %HexFmt(&self.resource_hash[..4]),
                        status = ?self.status,
                        retries = self.retries,
                        adv_retries = self.adv_retries,
                    );
                    if self.adv_retries < RESOURCE_MAX_ADV_RETRIES {
                        self.last_activity_ms = now_ms;
                        ResourcePollResult::RetransmitAdv(self.adv_packet.clone())
                    } else {
                        self.status = ResourceStatus::Failed;
                        crate::tracing::debug!(
                            event = "RESOURCE_TX_STATE",
                            rh = %HexFmt(&self.resource_hash[..4]),
                            status = ?self.status,
                            retries = self.retries,
                            adv_retries = self.adv_retries,
                        );
                        ResourcePollResult::TimedOut
                    }
                } else {
                    ResourcePollResult::Nothing
                }
            }
            ResourceStatus::Transferring => {
                // Sender watchdog: wait for receiver's REQ. The receiver drives
                // retransmission, so the sender should be patient.
                // Python sender uses global budget (Resource.py:627-633).
                let timeout_factor = if self.req_received {
                    PART_TIMEOUT_FACTOR_AFTER_RTT // 2: link characteristics known
                } else {
                    PART_TIMEOUT_FACTOR_INITIAL // 4: initial, generous
                };
                let per_retry_extra = self.retries as u64 * PER_RETRY_DELAY_MS;
                let timeout =
                    rtt_ms.saturating_mul(timeout_factor) + SENDER_GRACE_TIME_MS + per_retry_extra;

                if now_ms.saturating_sub(self.last_activity_ms) >= timeout {
                    self.retries += 1;
                    self.last_activity_ms = now_ms;
                    crate::tracing::debug!(
                        event = "RESOURCE_TX_STATE",
                        rh = %HexFmt(&self.resource_hash[..4]),
                        status = ?self.status,
                        retries = self.retries,
                    );
                    if self.retries >= RESOURCE_MAX_RETRIES {
                        self.status = ResourceStatus::Failed;
                        crate::tracing::debug!(
                            event = "RESOURCE_TX_STATE",
                            rh = %HexFmt(&self.resource_hash[..4]),
                            status = ?self.status,
                            retries = self.retries,
                        );
                        ResourcePollResult::TimedOut
                    } else {
                        // Just wait for another REQ
                        ResourcePollResult::Nothing
                    }
                } else {
                    ResourcePollResult::Nothing
                }
            }
            ResourceStatus::AwaitingProof => {
                // Python Resource.py:638-640: PROOF_TIMEOUT_FACTOR * RTT + SENDER_GRACE_TIME
                let per_retry_extra = self.retries as u64 * PER_RETRY_DELAY_MS;
                let timeout = rtt_ms.saturating_mul(PROOF_TIMEOUT_FACTOR)
                    + SENDER_GRACE_TIME_MS
                    + per_retry_extra;
                if now_ms.saturating_sub(self.last_activity_ms) >= timeout {
                    self.retries += 1;
                    self.last_activity_ms = now_ms;
                    crate::tracing::debug!(
                        event = "RESOURCE_TX_STATE",
                        rh = %HexFmt(&self.resource_hash[..4]),
                        status = ?self.status,
                        retries = self.retries,
                    );
                    if self.retries >= RESOURCE_MAX_RETRIES {
                        self.status = ResourceStatus::Failed;
                        crate::tracing::debug!(
                            event = "RESOURCE_TX_STATE",
                            rh = %HexFmt(&self.resource_hash[..4]),
                            status = ?self.status,
                            retries = self.retries,
                        );
                        ResourcePollResult::TimedOut
                    } else {
                        // Send CacheRequest so receiver re-sends the proof
                        let mut proof_data = Vec::with_capacity(64);
                        proof_data.extend_from_slice(&self.resource_hash);
                        proof_data.extend_from_slice(&self.expected_proof);
                        ResourcePollResult::RequestProof { proof_data }
                    }
                } else {
                    ResourcePollResult::Nothing
                }
            }
            _ => ResourcePollResult::Nothing,
        }
    }

    /// Compute the next deadline (absolute ms) for this resource.
    pub(crate) fn next_deadline(&self, rtt_ms: u64) -> Option<u64> {
        let rtt_ms = core::cmp::max(rtt_ms, 1);
        match self.status {
            ResourceStatus::Advertised => Some(
                self.last_activity_ms.saturating_add(
                    self.advertisement_timeout_ms
                        .unwrap_or_else(|| rtt_ms.saturating_mul(6))
                        .saturating_add(PROCESSING_GRACE_MS),
                ),
            ),
            ResourceStatus::Transferring => {
                let timeout_factor = if self.req_received {
                    PART_TIMEOUT_FACTOR_AFTER_RTT
                } else {
                    PART_TIMEOUT_FACTOR_INITIAL
                };
                let per_retry_extra = self.retries as u64 * PER_RETRY_DELAY_MS;
                let timeout =
                    rtt_ms.saturating_mul(timeout_factor) + SENDER_GRACE_TIME_MS + per_retry_extra;
                Some(self.last_activity_ms.saturating_add(timeout))
            }
            ResourceStatus::AwaitingProof => {
                let per_retry_extra = self.retries as u64 * PER_RETRY_DELAY_MS;
                let timeout = rtt_ms.saturating_mul(PROOF_TIMEOUT_FACTOR)
                    + SENDER_GRACE_TIME_MS
                    + per_retry_extra;
                Some(self.last_activity_ms.saturating_add(timeout))
            }
            _ => None,
        }
    }

    /// Mark this resource as failed/cancelled.
    #[allow(dead_code)] // Resource cancel API — see Codeberg issues #27/#28
    pub(crate) fn cancel(&mut self) {
        self.status = ResourceStatus::Failed;
        crate::tracing::debug!(
            event = "RESOURCE_TX_STATE",
            rh = %HexFmt(&self.resource_hash[..4]),
            status = ?self.status,
            retries = self.retries,
        );
    }

    // Accessors
    pub(crate) fn status(&self) -> ResourceStatus {
        self.status
    }

    pub(crate) fn resource_hash(&self) -> &[u8; 32] {
        &self.resource_hash
    }

    pub(crate) fn adv_packet(&self) -> &[u8] {
        &self.adv_packet
    }

    /// Number of distinct parts transmitted at least once.
    fn distinct_parts_sent(&self) -> usize {
        self.sent_mask.iter().filter(|&&sent| sent).count()
    }

    pub(crate) fn progress(&self) -> f32 {
        if self.num_parts == 0 {
            return 1.0;
        }
        self.distinct_parts_sent() as f32 / self.num_parts as f32
    }

    pub(crate) fn transfer_size(&self) -> u64 {
        self.encrypted_data.len() as u64
    }

    #[allow(dead_code)] // Resource accessor API — see Codeberg issues #27/#28
    pub(crate) fn flags(&self) -> &ResourceFlags {
        &self.flags
    }

    #[allow(dead_code)] // Resource accessor API — see Codeberg issues #27/#28
    pub(crate) fn original_hash(&self) -> &[u8; 32] {
        &self.original_hash
    }

    #[allow(dead_code)] // Resource accessor API — see Codeberg issues #27/#28
    pub(crate) fn random_hash(&self) -> &[u8; RESOURCE_RANDOM_HASH_SIZE] {
        &self.random_hash
    }

    pub(crate) fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    #[allow(dead_code)] // Resource accessor API — see Codeberg issues #27/#28
    pub(crate) fn total_hashmap_segments(&self) -> u32 {
        self.total_hashmap_segments
    }

    #[allow(dead_code)] // Resource accessor API — see Codeberg issues #27/#28
    pub(crate) fn request_id(&self) -> Option<&[u8]> {
        self.request_id.as_deref()
    }

    #[allow(dead_code)] // Resource accessor API — see Codeberg issues #27/#28
    pub(crate) fn sdu(&self) -> usize {
        self.sdu
    }

    /// Advertisement retransmissions charged against
    /// [`RESOURCE_MAX_ADV_RETRIES`](crate::resource::RESOURCE_MAX_ADV_RETRIES)
    /// so far. Test-only: the counter is otherwise observable only through the
    /// retransmitted packet.
    #[cfg(test)]
    pub(crate) fn adv_retries(&self) -> usize {
        self.adv_retries
    }

    /// Timestamp the activity clock is currently anchored at. Test-only.
    #[cfg(test)]
    pub(crate) fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms
    }
}

/// Number of segments needed to carry `total_size` bytes, where each segment
/// holds at most [`RESOURCE_MAX_EFFICIENT_SIZE`] bytes of the logical stream.
///
/// Matches Python `((total_size-1)//MAX_EFFICIENT_SIZE)+1`. `total_size` is the
/// combined `metadata_size + data` length. Returns 1 for `total_size == 0`.
pub(crate) fn segment_count(total_size: usize) -> u32 {
    if total_size == 0 {
        return 1;
    }
    ((total_size - 1) / RESOURCE_MAX_EFFICIENT_SIZE) as u32 + 1
}

/// Plan for sending the remaining segments of a split resource transfer.
///
/// Held on the [`Link`] alongside the in-flight [`OutgoingResource`]. Segment 1
/// is advertised immediately by `send_resource`; each subsequent segment is
/// built and advertised only after the previous segment's proof arrives, which
/// mirrors Python `Resource.validate_proof` advertising `next_segment`.
pub(crate) struct OutgoingSegmentPlan {
    /// Full application data (the whole file). Metadata is stored separately.
    data: Vec<u8>,
    /// Segment-1 metadata bytes (msgpack, as passed by the caller).
    metadata: Option<Vec<u8>>,
    /// Length of the metadata block prepended to segment 1, i.e.
    /// `3 + metadata.len()` (the 3-byte length prefix plus the bytes), or 0.
    /// Matches Python `metadata_size`.
    metadata_size: usize,
    /// Total number of segments in this transfer.
    total_segments: u32,
    /// Index (1-based) of the next segment to build and advertise.
    next_index: u32,
    /// Shared `o` advertisement field: segment 1's resource hash.
    original_hash: [u8; 32],
    /// Total logical size (`metadata_size + data.len()`) for the `d` field.
    total_data_size: u64,
    /// Whether resource data should be auto-compressed per segment.
    auto_compress: bool,
}

impl OutgoingSegmentPlan {
    /// Create a plan for a transfer whose combined `metadata + data` exceeds
    /// [`RESOURCE_MAX_EFFICIENT_SIZE`]. `next_index` starts at 2 because the
    /// caller advertises segment 1 directly.
    pub(crate) fn new(
        data: Vec<u8>,
        metadata: Option<Vec<u8>>,
        metadata_size: usize,
        total_segments: u32,
        auto_compress: bool,
    ) -> Self {
        let total_data_size = (metadata_size + data.len()) as u64;
        Self {
            data,
            metadata,
            metadata_size,
            total_segments,
            next_index: 2,
            original_hash: [0u8; 32],
            total_data_size,
            auto_compress,
        }
    }

    /// Record segment 1's resource hash as the shared `o` field for all later
    /// segments.
    pub(crate) fn set_original_hash(&mut self, hash: [u8; 32]) {
        self.original_hash = hash;
    }

    /// Whether there is still a segment left to advertise.
    pub(crate) fn has_next(&self) -> bool {
        self.next_index <= self.total_segments
    }

    /// The byte range of `data` carried by segment `index` (1-based).
    ///
    /// Segment 1 carries `MAX - metadata_size` data bytes after the metadata
    /// block; each later segment carries up to `MAX` data bytes. Matches
    /// Python's `seek_position` / `segment_read_size`.
    fn data_range(&self, index: u32) -> core::ops::Range<usize> {
        let first_read_size = RESOURCE_MAX_EFFICIENT_SIZE.saturating_sub(self.metadata_size);
        let start = if index <= 1 {
            0
        } else {
            first_read_size + (index as usize - 2) * RESOURCE_MAX_EFFICIENT_SIZE
        };
        let want = if index <= 1 {
            first_read_size
        } else {
            RESOURCE_MAX_EFFICIENT_SIZE
        };
        let start = start.min(self.data.len());
        let end = start.saturating_add(want).min(self.data.len());
        start..end
    }

    /// Build the [`OutgoingResource`] for segment `index` (1-based).
    ///
    /// For segment 1 the metadata block is prepended and `o` is set to the
    /// segment's own resource hash; later segments carry the recorded
    /// `original_hash` and no metadata bytes (but keep the `has_metadata` flag
    /// when the transfer has metadata, matching Python).
    pub(crate) fn build_segment(
        &self,
        index: u32,
        crypt: &ResourceCryptParams,
        rng: &mut impl CryptoRngCore,
        now_ms: u64,
    ) -> Result<OutgoingResource, ResourceError> {
        let range = self.data_range(index);
        let slice = &self.data[range];

        let (metadata, original_hash): (Option<&[u8]>, Option<[u8; 32]>) = if index <= 1 {
            (self.metadata.as_deref(), None)
        } else {
            (None, Some(self.original_hash))
        };

        let seg = SegmentParams {
            segment_index: index,
            total_segments: self.total_segments,
            original_hash,
            total_data_size: Some(self.total_data_size),
            force_has_metadata: self.metadata_size > 0,
        };

        OutgoingResource::new_segment(slice, metadata, crypt, self.auto_compress, rng, now_ms, seg)
    }

    /// Advance the plan to the next segment after one has been advertised.
    pub(crate) fn advance(&mut self) {
        self.next_index += 1;
    }

    /// Index of the next segment to advertise.
    pub(crate) fn next_index(&self) -> u32 {
        self.next_index
    }

    /// Total number of segments in the transfer.
    pub(crate) fn total_segments(&self) -> u32 {
        self.total_segments
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceAdvertisement;

    /// Helper to create a test link with known keys.
    /// We use Link's test infrastructure.
    fn make_test_link() -> (Link, Link) {
        use rand_core::OsRng;
        let dest_hash = crate::destination::DestinationHash::new([0xAA; 16]);
        let mut initiator = Link::new_outgoing(dest_hash, &mut OsRng);

        // Create responder and complete handshake for testing
        let dest_hash2 = crate::destination::DestinationHash::new([0xBB; 16]);
        let _responder = Link::new_outgoing(dest_hash2, &mut OsRng);

        // We need an active link for encryption, use the test helper
        // that sets link_key directly.
        let link_key = [0x42u8; 64];
        initiator.set_link_key_for_test(link_key);

        let mut resp = Link::new_outgoing(dest_hash, &mut OsRng);
        resp.set_link_key_for_test(link_key);

        // Both need to be Active
        initiator.set_state(crate::link::LinkState::Active);
        resp.set_state(crate::link::LinkState::Active);

        (initiator, resp)
    }

    #[test]
    fn test_outgoing_resource_creation() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = b"Hello, Resource!";

        let res = OutgoingResource::new(
            data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        assert_eq!(res.status(), ResourceStatus::Advertised);
        assert_eq!(res.num_parts, 1); // small data = 1 part
        assert!(!res.adv_packet().is_empty());
    }

    #[test]
    fn test_outgoing_resource_adv_roundtrip() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = b"Test data for advertisement roundtrip";

        let res = OutgoingResource::new(
            data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        // Verify the cached ADV unpacks correctly
        let adv = ResourceAdvertisement::unpack(res.adv_packet()).unwrap();
        assert_eq!(adv.resource_hash, *res.resource_hash());
        assert_eq!(adv.num_parts, res.num_parts);
        assert!(adv.flags.encrypted);
        assert_eq!(adv.segment_index, 1);
        assert_eq!(adv.total_segments, 1);
        assert!(adv.request_id.is_none());
    }

    #[test]
    fn test_request_resource_uses_caller_advertisement_timeout_and_python_flags() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let request_id = [0x31; crate::constants::TRUNCATED_HASHBYTES];
        let sent_at_ms = 1_000;
        let timeout_ms = 275;
        let mut res = OutgoingResource::new_request(
            b"large request body",
            &request_id,
            &link,
            true,
            timeout_ms,
            &mut rng,
            sent_at_ms,
        )
        .unwrap();

        let adv = ResourceAdvertisement::unpack(res.adv_packet()).unwrap();
        assert!(adv.flags.is_request);
        assert!(!adv.flags.is_response);
        assert!(!adv.flags.split);
        assert_eq!(adv.segment_index, 1);
        assert_eq!(adv.total_segments, 1);
        assert_eq!(adv.request_id.as_deref(), Some(request_id.as_slice()));

        let expected_deadline = sent_at_ms + timeout_ms + PROCESSING_GRACE_MS;
        assert_eq!(res.next_deadline(10_000), Some(expected_deadline));
        assert!(matches!(
            res.poll(expected_deadline - 1, 10_000),
            ResourcePollResult::Nothing
        ));
        assert!(matches!(
            res.poll(expected_deadline, 10_000),
            ResourcePollResult::RetransmitAdv(_)
        ));
    }

    #[test]
    fn test_response_resource_uses_python_flags() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let request_id = [0x52; crate::constants::TRUNCATED_HASHBYTES];
        let res = OutgoingResource::new_response(
            b"response body",
            None,
            Some(&request_id),
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1_000,
        )
        .unwrap();

        let adv = ResourceAdvertisement::unpack(res.adv_packet()).unwrap();
        assert!(!adv.flags.is_request);
        assert!(adv.flags.is_response);
        assert!(!adv.flags.split);
        assert_eq!(adv.segment_index, 1);
        assert_eq!(adv.total_segments, 1);
        assert_eq!(adv.request_id.as_deref(), Some(request_id.as_slice()));
    }

    #[test]
    fn test_outgoing_resource_with_metadata() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = b"data with metadata";
        let metadata = b"some metadata";

        let res = OutgoingResource::new(
            data,
            Some(metadata),
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        let adv = ResourceAdvertisement::unpack(res.adv_packet()).unwrap();
        assert!(adv.flags.has_metadata);
    }

    #[test]
    fn test_outgoing_resource_proof_validation() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = b"proof test data";

        let mut res = OutgoingResource::new(
            data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        // Forge a valid proof: resource_hash + expected_proof
        let mut valid_proof = Vec::new();
        valid_proof.extend_from_slice(&res.resource_hash);
        valid_proof.extend_from_slice(&res.expected_proof);

        let result = res.handle_proof(&valid_proof).unwrap();
        assert_eq!(result, ResourceStatus::Complete);
        assert_eq!(res.status(), ResourceStatus::Complete);
    }

    #[test]
    fn test_outgoing_resource_invalid_proof() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = b"proof test data";

        let mut res = OutgoingResource::new(
            data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        // Invalid proof
        let bad_proof = [0u8; 64];
        assert_eq!(
            res.handle_proof(&bad_proof),
            Err(ResourceError::InvalidProof)
        );
    }

    #[test]
    fn test_outgoing_resource_poll_adv_timeout() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = b"timeout test";

        let mut res = OutgoingResource::new(
            data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        // Not timed out yet
        let result = res.poll(1000, 100);
        assert!(matches!(result, ResourcePollResult::Nothing));

        // Timed out (rtt_ms * 6 + PROCESSING_GRACE_MS = 600 + 1000 = 1600ms)
        let result = res.poll(2601, 100);
        assert!(matches!(result, ResourcePollResult::RetransmitAdv(_)));

        // After max retries, should be TimedOut
        for _ in 0..RESOURCE_MAX_ADV_RETRIES {
            res.poll(res.last_activity_ms + 1601, 100);
        }
        assert_eq!(res.status(), ResourceStatus::Failed);
    }

    #[test]
    fn test_transferring_retries_spaced_by_timeout() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        // Large enough for multiple parts. With the `compression` feature
        // active (workspace builds enable it via leviculum-std), repeated
        // bytes would compress below MTU and yield a single part. Use
        // pseudo-random bytes so part count is independent of the feature.
        use rand_core::{OsRng, RngCore};
        let mut data = vec![0u8; 2000];
        OsRng.fill_bytes(&mut data);

        let mut res = OutgoingResource::new(
            &data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();
        assert!(res.parts.len() >= 2, "need multi-part resource");

        // Build a partial REQ requesting only the first part to keep status=Transferring.
        // Format: [0x00][resource_hash:32][hashmap_entry_0:4]
        let mut req = Vec::new();
        req.push(0x00); // not exhausted
        req.extend_from_slice(&res.resource_hash);
        req.extend_from_slice(&res.hashmap[0]);
        let _ = res.handle_request(&req, &link, &mut rng, 2000);
        assert_eq!(res.status(), ResourceStatus::Transferring);

        // Use rtt_ms = 1000. After handle_request, req_received=true, so
        // timeout_factor = PART_TIMEOUT_FACTOR_AFTER_RTT (2).
        // Sender timeout = rtt * 2 + SENDER_GRACE_TIME_MS(10000) + retries*500
        // = 2000 + 10000 = 12000ms (0 retries)
        let rtt_ms = 1000;

        // First poll just after the REQ, should NOT time out.
        let result = res.poll(2500, rtt_ms);
        assert!(matches!(result, ResourcePollResult::Nothing));

        // Fire first timeout (12000ms after last activity at t=2000 → t=14000).
        let result = res.poll(14001, rtt_ms);
        assert!(matches!(result, ResourcePollResult::Nothing)); // retry incremented, returns Nothing
        assert_eq!(res.retries, 1);

        // Immediately polling again should NOT fire another retry because
        // last_activity_ms was reset.
        let result = res.poll(14002, rtt_ms);
        assert!(matches!(result, ResourcePollResult::Nothing));
        assert_eq!(
            res.retries, 1,
            "retry must not increment without waiting full timeout"
        );

        // After another full timeout period (now 12500ms = 12000 + 500 backoff), retry fires.
        let result = res.poll(14001 + 12501, rtt_ms);
        assert!(matches!(result, ResourcePollResult::Nothing));
        assert_eq!(res.retries, 2);

        // Verify we don't immediately hit max retries (16) from rapid polling.
        for _ in 0..20 {
            res.poll(14001 + 12501 + 1, rtt_ms);
        }
        assert!(
            res.retries < RESOURCE_MAX_RETRIES,
            "retries should not exhaust from rapid polling: got {}",
            res.retries
        );
        assert_eq!(res.status(), ResourceStatus::Transferring);
    }

    #[test]
    fn test_awaiting_proof_retries() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        // Small data, fits in few parts so all get sent in one REQ
        let data = vec![0x42u8; 200];

        let mut res = OutgoingResource::new(
            &data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        // Build full REQ requesting all parts
        let mut req = Vec::new();
        req.push(0x00); // not exhausted
        req.extend_from_slice(&res.resource_hash);
        for h in &res.hashmap {
            req.extend_from_slice(h);
        }
        let _ = res.handle_request(&req, &link, &mut rng, 2000);
        // All parts sent → should transition to AwaitingProof
        assert_eq!(res.status(), ResourceStatus::AwaitingProof);

        let rtt_ms = 1000;
        // AwaitingProof timeout = PROOF_TIMEOUT_FACTOR * rtt + SENDER_GRACE_TIME_MS + retries*500
        // = 3 * 1000 + 10000 + 0 = 13000ms (0 retries)

        // Not timed out yet
        let result = res.poll(14999, rtt_ms);
        assert!(matches!(result, ResourcePollResult::Nothing));

        // First timeout fires at 2000 + 13000 = 15000, sends CacheRequest
        let result = res.poll(15001, rtt_ms);
        assert!(matches!(result, ResourcePollResult::RequestProof { .. }));
        assert_eq!(res.retries, 1);
        assert_eq!(res.status(), ResourceStatus::AwaitingProof);

        // Second timeout at 15001 + 13500 (13000 + 500 backoff) = 28501
        let result = res.poll(28502, rtt_ms);
        assert!(matches!(result, ResourcePollResult::RequestProof { .. }));
        assert_eq!(res.retries, 2);
        assert_eq!(res.status(), ResourceStatus::AwaitingProof);

        // Rapid polling should not exhaust retries
        for _ in 0..20 {
            res.poll(28503, rtt_ms);
        }
        assert!(
            res.retries < RESOURCE_MAX_RETRIES,
            "retries should not exhaust from rapid polling: got {}",
            res.retries
        );
        assert_eq!(res.status(), ResourceStatus::AwaitingProof);

        // Exhaust all retries
        for _ in res.retries..RESOURCE_MAX_RETRIES {
            // Each retry adds 500ms more: timeout grows with retries
            let timeout = 13000 + res.retries as u64 * PER_RETRY_DELAY_MS;
            let t = res.last_activity_ms + timeout + 1;
            res.poll(t, rtt_ms);
        }
        assert_eq!(res.status(), ResourceStatus::Failed);
    }

    /// Codeberg #85: retransmissions must not count toward completion. A lossy
    /// receiver re-REQs the same parts repeatedly; the sender must stay in
    /// Transferring until every DISTINCT part has been sent at least once,
    /// no matter how many transmissions have accumulated (Python only counts
    /// first sends, Resource.py:1013-1015).
    #[test]
    fn test_retransmissions_do_not_trigger_awaiting_proof() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        use rand_core::{OsRng, RngCore};
        let mut data = vec![0u8; 2000];
        OsRng.fill_bytes(&mut data);

        let mut res = OutgoingResource::new(
            &data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();
        let total = res.parts.len();
        assert!(total >= 2, "need multi-part resource");

        // Re-request only the first part more times than there are parts, so
        // the cumulative transmission count crosses the part count while only
        // one distinct part has ever been sent.
        let mut req = Vec::new();
        req.push(0x00); // not exhausted
        req.extend_from_slice(&res.resource_hash);
        req.extend_from_slice(&res.hashmap[0]);
        for n in 0..=total {
            let pkts = res
                .handle_request(&req, &link, &mut rng, 2000 + n as u64)
                .unwrap();
            assert_eq!(pkts.len(), 1, "each REQ resends the one requested part");
            assert_eq!(
                res.status(),
                ResourceStatus::Transferring,
                "retransmissions alone must not trigger AwaitingProof \
                 (after {} transmissions of part 0, {} parts total)",
                n + 1,
                total
            );
        }

        // Once every distinct part has been requested and sent, the
        // transition fires as before.
        let mut req_all = Vec::new();
        req_all.push(0x00);
        req_all.extend_from_slice(&res.resource_hash);
        for h in &res.hashmap {
            req_all.extend_from_slice(h);
        }
        let _ = res.handle_request(&req_all, &link, &mut rng, 9000).unwrap();
        assert_eq!(res.status(), ResourceStatus::AwaitingProof);
    }

    /// Every received REQ emits the RESOURCE_REQ_RX observability event
    /// (Codeberg #85) with the request/resend accounting the stall
    /// diagnosis needs.
    // Log-capture assertion needs the tracing feature; gated so the
    // --no-default-features (tracing-off) build does not compile `tracing::`.
    #[cfg(feature = "tracing")]
    #[test]
    fn test_handle_request_emits_resource_req_rx_event() {
        extern crate std;
        use std::string::String;

        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = vec![0x42u8; 200];
        let mut res = OutgoingResource::new(
            &data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();
        let rh_prefix = {
            let mut s = String::new();
            for b in &res.resource_hash[..4] {
                s.push_str(&std::format!("{b:02x}"));
            }
            s
        };

        // Full REQ requesting all parts.
        let mut req = Vec::new();
        req.push(0x00);
        req.extend_from_slice(&res.resource_hash);
        for h in &res.hashmap {
            req.extend_from_slice(h);
        }

        let ((), logs) = crate::test_log_capture::with_captured_logs(|| {
            let _ = res.handle_request(&req, &link, &mut rng, 2000).unwrap();
        });
        let line = logs
            .lines()
            .find(|l| l.contains("RESOURCE_REQ_RX"))
            .expect("handle_request must emit a RESOURCE_REQ_RX event");
        let num_parts = res.parts.len();
        for key in [
            std::format!("rh={rh_prefix}"),
            std::format!("n_req={num_parts}"),
            std::format!("matched={num_parts}"),
            String::from("first_req_idx=0"),
            std::format!("distinct_sent={num_parts}"),
            std::format!("num_parts={num_parts}"),
            String::from("status=AwaitingProof"),
        ] {
            assert!(
                line.contains(&*key),
                "RESOURCE_REQ_RX missing {key}: {line}"
            );
        }
        assert!(
            logs.lines().any(|l| l.contains("RESOURCE_TX_STATE")),
            "status transitions must emit RESOURCE_TX_STATE: {logs}"
        );
    }

    #[test]
    fn test_outgoing_resource_hashmap_integrity() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        // Create data large enough for multiple parts
        let data = vec![0x42u8; 2000];

        let res = OutgoingResource::new(
            &data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        // Verify each part's map_hash matches
        for (i, part) in res.parts.iter().enumerate() {
            let expected = map_hash(part, &res.random_hash);
            assert_eq!(res.hashmap[i], expected, "hashmap mismatch at part {i}");
        }
    }

    #[test]
    fn test_outgoing_resource_accessors() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = b"accessor test data";
        let request_id = b"req-42";

        let mut res = OutgoingResource::new(
            data,
            None,
            Some(request_id),
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        // Protocol state fields accessible via accessors
        assert!(res.flags().encrypted);
        assert!(!res.flags().compressed);
        assert!(res.original_hash() != &[0u8; 32]);
        assert!(res.random_hash() != &[0u8; RESOURCE_RANDOM_HASH_SIZE]);
        assert!(res.uncompressed_size() > 0);
        assert!(res.total_hashmap_segments() >= 1);
        assert_eq!(res.request_id(), Some(request_id.as_slice()));
        assert!(res.sdu() > 0);

        // cancel() transitions to Failed
        res.cancel();
        assert_eq!(res.status(), ResourceStatus::Failed);
    }

    #[test]
    fn test_awaiting_proof_returns_request_proof_with_correct_data() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;
        let data = vec![0x42u8; 200];

        let mut res = OutgoingResource::new(
            &data,
            None,
            None,
            &link.resource_crypt_params(),
            true,
            &mut rng,
            1000,
        )
        .unwrap();

        // Build full REQ requesting all parts → transition to AwaitingProof
        let mut req = Vec::new();
        req.push(0x00);
        req.extend_from_slice(&res.resource_hash);
        for h in &res.hashmap {
            req.extend_from_slice(h);
        }
        let _ = res.handle_request(&req, &link, &mut rng, 2000);
        assert_eq!(res.status(), ResourceStatus::AwaitingProof);

        let expected_resource_hash = res.resource_hash;
        let expected_proof = res.expected_proof;

        // Trigger first timeout → should return RequestProof
        let rtt_ms = 1000;
        let timeout = rtt_ms * PROOF_TIMEOUT_FACTOR + SENDER_GRACE_TIME_MS;
        let result = res.poll(2000 + timeout + 1, rtt_ms);

        match result {
            ResourcePollResult::RequestProof { proof_data } => {
                assert_eq!(proof_data.len(), 64, "proof_data must be 64 bytes");
                assert_eq!(
                    &proof_data[..32],
                    &expected_resource_hash,
                    "first 32 bytes must be resource_hash"
                );
                assert_eq!(
                    &proof_data[32..],
                    &expected_proof,
                    "last 32 bytes must be expected_proof"
                );
            }
            other => panic!("expected RequestProof, got {other:?}"),
        }
    }

    // ---- Segmentation (Codeberg #27) ----

    const MAX: usize = RESOURCE_MAX_EFFICIENT_SIZE;

    #[test]
    fn test_segment_count_boundaries() {
        // Matches Python ((total_size-1)//MAX)+1, with 0 -> 1 segment.
        assert_eq!(segment_count(0), 1);
        assert_eq!(segment_count(1), 1);
        assert_eq!(segment_count(MAX - 1), 1);
        assert_eq!(segment_count(MAX), 1); // exactly MAX: single segment
        assert_eq!(segment_count(MAX + 1), 2); // one over: split
        assert_eq!(segment_count(2 * MAX), 2);
        assert_eq!(segment_count(2 * MAX + 1), 3);
        assert_eq!(segment_count(7 * MAX / 2), 4); // ~3.5x -> 4 segments
    }

    /// Every segment's data range must tile `data` exactly: start at 0, no gaps
    /// or overlaps, and cover the whole buffer. This is the byte-boundary math a
    /// Python receiver relies on to reassemble the file.
    fn assert_ranges_tile(metadata_size: usize, data_len: usize) {
        let total_size = metadata_size + data_len;
        let total_segments = segment_count(total_size);
        let plan = OutgoingSegmentPlan::new(
            vec![0u8; data_len],
            (metadata_size > 0).then(|| vec![0u8; metadata_size.saturating_sub(3)]),
            metadata_size,
            total_segments,
            false,
        );

        let first_read_size = MAX.saturating_sub(metadata_size);
        let mut expected_start = 0usize;
        for index in 1..=total_segments {
            let r = plan.data_range(index);
            assert_eq!(
                r.start, expected_start,
                "segment {index} start (meta={metadata_size}, len={data_len})"
            );
            let want = if index == 1 { first_read_size } else { MAX };
            let expected_end = (expected_start + want).min(data_len);
            assert_eq!(
                r.end, expected_end,
                "segment {index} end (meta={metadata_size}, len={data_len})"
            );
            expected_start = r.end;
        }
        assert_eq!(
            expected_start, data_len,
            "segments must cover all data bytes (meta={metadata_size}, len={data_len})"
        );
    }

    #[test]
    fn test_plan_data_ranges_tile_data() {
        // Just over MAX, no metadata: 2 segments.
        assert_ranges_tile(0, MAX + 1);
        // Metadata pushes an exactly-MAX data buffer over the boundary: the
        // first segment's data slice shrinks by metadata_size.
        assert_ranges_tile(20, MAX);
        // ~3.5x, no metadata.
        assert_ranges_tile(0, 7 * MAX / 2);
        // ~3.5x with metadata.
        assert_ranges_tile(37, 7 * MAX / 2);
        assert_ranges_tile(100, MAX);
    }

    #[test]
    fn test_plan_first_segment_offsets_by_metadata() {
        // With metadata present, segment 1 carries MAX - metadata_size data
        // bytes so the combined (metadata_block + data) is exactly MAX.
        let metadata_size = 13;
        let data_len = 2 * MAX;
        let plan = OutgoingSegmentPlan::new(
            vec![0u8; data_len],
            Some(vec![0u8; metadata_size - 3]),
            metadata_size,
            segment_count(metadata_size + data_len),
            false,
        );
        assert_eq!(plan.data_range(1).len(), MAX - metadata_size);
        assert_eq!(plan.total_data_size, (metadata_size + data_len) as u64);
    }

    /// #159 tranche 2: pin every generated advertisement field against the
    /// rule the reference receiver applies to it, with the hashes recomputed
    /// independently from the DECRYPTED wire stream (never from our stored
    /// intermediates):
    ///
    /// - `t` = length of the encrypted stream; the receiver allocates
    ///   `total_parts = ceil(t / sdu)` from it (Resource.py:174-190) with
    ///   `sdu = link.mtu - HEADER_MAXSIZE - IFAC_MIN_SIZE` (:337-338), so `t`,
    ///   `n`, and our actual part slicing must all agree under that formula.
    /// - `h` = `full_hash(plaintext + r)` where plaintext is the metadata
    ///   block + data WITHOUT the 4-byte wire-random prefix (:441 sender,
    ///   :682-695 receiver: decrypt, strip 4, hash, compare — mismatch means
    ///   CORRUPT and link teardown).
    /// - `r` keys the per-part map hashes: `full_hash(part + r)[:4]`
    ///   (:505-506); the receiver matches every incoming RESOURCE packet
    ///   purely by these entries (:861-866).
    /// - metadata rides as a 3-byte BE length + bytes prepended to the data
    ///   (:266), stripped by the receiver only on segment 1 (:696-704), with
    ///   the `x` flag bit set.
    /// - the sender's `expected_proof` = `full_hash(plaintext + h)` (:443)
    ///   must equal what the receiver's `prove()` computes over its assembled
    ///   plaintext (:752-755) — pinned here from the decrypted stream and in
    ///   `incoming.rs` from the receiver side.
    /// - `f` bit layout `x<<5 | p<<4 | u<<3 | s<<2 | c<<1 | e` (:1259).
    /// - `i`/`l` are 1-based (:190/:249).
    /// - `o` = `h` itself for every fresh resource (:440-448) — the SALTED
    ///   per-transfer hash, never a deterministic content hash. The receiver
    ///   keys its on-disk reassembly file by `o` (:199), appends in mode
    ///   "ab" (:708) and unlinks only on success (:744), so a repeatable `o`
    ///   lets two transfers of identical content share one path and deliver
    ///   doubled data (Codeberg #165).
    #[test]
    fn advertisement_fields_follow_reference_generation_rules() {
        use crate::crypto::sha256;

        let (link, peer) = make_test_link();
        let mut rng = rand_core::OsRng;

        let metadata = b"META";
        let data: Vec<u8> = (0..2000u32).map(|i| (i * 31 % 251) as u8).collect();
        let res = OutgoingResource::new(
            &data,
            Some(metadata),
            None,
            &link.resource_crypt_params(),
            false,
            &mut rng,
            1_000,
        )
        .unwrap();
        let adv = ResourceAdvertisement::unpack(res.adv_packet()).unwrap();

        // Decrypt the wire stream the way the receiver does.
        let mut buf = vec![0u8; res.encrypted_data.len()];
        let n = peer.decrypt(&res.encrypted_data, &mut buf).unwrap();
        buf.truncate(n);
        let plaintext = &buf[RESOURCE_RANDOM_HASH_SIZE..]; // strip wire random

        // Metadata block: 3-byte BE length + bytes, then the data.
        let mut expected_plaintext = vec![0u8, 0, metadata.len() as u8];
        expected_plaintext.extend_from_slice(metadata);
        expected_plaintext.extend_from_slice(&data);
        assert_eq!(
            plaintext, expected_plaintext,
            "decrypted stream must be metadata block + data behind the 4-byte prefix"
        );

        // h: the receiver's assemble-time acceptance hash.
        let mut h_input = plaintext.to_vec();
        h_input.extend_from_slice(&adv.random_hash);
        assert_eq!(
            adv.resource_hash,
            sha256(&h_input),
            "h must equal full_hash(decrypted plaintext + r): the receiver \
             rejects the whole transfer as CORRUPT otherwise"
        );

        // expected_proof: what the receiver's prove() will send back.
        let mut p_input = plaintext.to_vec();
        p_input.extend_from_slice(&adv.resource_hash);
        assert_eq!(
            res.expected_proof,
            sha256(&p_input),
            "expected_proof must equal full_hash(plaintext + h)"
        );

        // t / n / parts: the receiver's allocation arithmetic.
        let sdu = crate::constants::MTU
            - crate::constants::HEADER_MAXSIZE
            - crate::constants::IFAC_MIN_SIZE;
        assert_eq!(sdu, 464);
        assert_eq!(res.sdu(), sdu, "sender sdu must match the receiver formula");
        assert_eq!(adv.transfer_size as usize, res.encrypted_data.len());
        let expected_parts = (adv.transfer_size as usize).div_ceil(sdu);
        assert_eq!(adv.num_parts as usize, expected_parts);
        assert_eq!(res.parts.len(), expected_parts);
        for (i, part) in res.parts.iter().enumerate() {
            let start = i * sdu;
            let end = core::cmp::min(start + sdu, res.encrypted_data.len());
            assert_eq!(
                part[..],
                res.encrypted_data[start..end],
                "part {i} must be the sdu-aligned slice of the encrypted stream"
            );
        }

        // m: the receiver's part-matching keys.
        assert_eq!(
            adv.hashmap_data.len(),
            expected_parts * RESOURCE_HASHMAP_LEN
        );
        for (i, part) in res.parts.iter().enumerate() {
            let mut m_input = part.clone();
            m_input.extend_from_slice(&adv.random_hash);
            assert_eq!(
                adv.hashmap_data[i * RESOURCE_HASHMAP_LEN..(i + 1) * RESOURCE_HASHMAP_LEN],
                sha256(&m_input)[..RESOURCE_HASHMAP_LEN],
                "map hash {i} must equal full_hash(part + r)[:4]"
            );
        }

        // d: total uncompressed size including the metadata block.
        assert_eq!(adv.data_size as usize, expected_plaintext.len());

        // o: the reference sets original_hash = self.hash for a fresh
        // resource (Resource.py:440-448), so `o` carries the r-salted
        // per-transfer hash — the receiver's reassembly-file key (:199).
        assert_eq!(
            adv.original_hash, adv.resource_hash,
            "o must be the salted resource hash for a fresh single-segment \
             resource, never a deterministic content hash (Codeberg #165)"
        );
        let res2 = OutgoingResource::new(
            &data,
            Some(metadata),
            None,
            &link.resource_crypt_params(),
            false,
            &mut rng,
            1_000,
        )
        .unwrap();
        let adv2 = ResourceAdvertisement::unpack(res2.adv_packet()).unwrap();
        assert_ne!(
            adv.original_hash, adv2.original_hash,
            "two transfers of identical content must not share o: a Python \
             receiver keys its on-disk reassembly file by o and appends, so \
             a shared path can deliver doubled data (Codeberg #165)"
        );

        // Flags byte and segment fields.
        assert_eq!(
            res.flags().to_u8(),
            0x01 | 0x20,
            "f must be encrypted | has_metadata under the reference bit layout"
        );
        assert_eq!(adv.segment_index, 1, "segment index is 1-based");
        assert_eq!(adv.total_segments, 1);
        assert!(adv.request_id.is_none());

        // Request/response resources: u and p bits (Resource.py:1296-1303).
        let req =
            OutgoingResource::new_request(b"x", &[0x11; 16], &link, false, 1_000, &mut rng, 1_000)
                .unwrap();
        assert_eq!(req.flags().to_u8(), 0x01 | 0x08, "request: e | u");
        let resp = OutgoingResource::new_response(
            b"x",
            None,
            Some(&[0x11; 16]),
            &link.resource_crypt_params(),
            false,
            &mut rng,
            1_000,
        )
        .unwrap();
        assert_eq!(resp.flags().to_u8(), 0x01 | 0x10, "response: e | p");
    }

    /// #165: pin the `o` chain of a split transfer against the reference.
    /// Segment 1 of a fresh split resource carries its own salted hash as `o`
    /// (Resource.py:445-446), and every later segment inherits segment 1's
    /// value verbatim (:772 `original_hash=self.original_hash`) — the Python
    /// receiver groups all segments of one transfer into one reassembly file
    /// by this key (:199).
    #[test]
    fn split_segments_share_segment1_salted_hash_as_o() {
        let (link, _) = make_test_link();
        let mut rng = rand_core::OsRng;

        let data: Vec<u8> = (0..MAX + 1).map(|i| (i % 251) as u8).collect();
        let total_segments = segment_count(data.len());
        assert_eq!(total_segments, 2);
        let mut plan = OutgoingSegmentPlan::new(data, None, 0, total_segments, false);

        let seg1 = plan
            .build_segment(1, &link.resource_crypt_params(), &mut rng, 1_000)
            .unwrap();
        let adv1 = ResourceAdvertisement::unpack(seg1.adv_packet()).unwrap();
        assert_eq!(
            adv1.original_hash, adv1.resource_hash,
            "segment 1 of a fresh split resource must carry its own salted \
             hash as o (Resource.py:445-446)"
        );

        plan.set_original_hash(*seg1.resource_hash());
        let seg2 = plan
            .build_segment(2, &link.resource_crypt_params(), &mut rng, 2_000)
            .unwrap();
        let adv2 = ResourceAdvertisement::unpack(seg2.adv_packet()).unwrap();
        assert_ne!(
            adv2.resource_hash, adv1.resource_hash,
            "each segment has its own per-segment hash"
        );
        assert_eq!(
            adv2.original_hash, adv1.resource_hash,
            "later segments must inherit segment 1's o verbatim: the \
             receiver's per-transfer group key (Resource.py:199/:772)"
        );
    }

    /// #159 tranche 2: pin the RESOURCE_HMU reply against the reference
    /// hashmap segment arithmetic. Both sides index the hashmap in units of
    /// `HASHMAP_MAX_LEN = floor((Link.MDU - 134) / 4) = 74` — a CLASS constant
    /// derived from the default MTU, never from the negotiated link MDU
    /// (Resource.py:1236). When the receiver reports exhaustion with the last
    /// map hash it knows, the sender locates that part, and the reference
    /// CANCELS the whole transfer unless the resulting index is an exact
    /// multiple of 74 (Resource.py:1046-1050); the HMU then carries
    /// `h + msgpack([segment, next 74 entries])`, which the receiver writes
    /// at offset `segment * 74` (Resource.py:492-500).
    #[test]
    fn hmu_reply_follows_reference_hashmap_segment_arithmetic() {
        assert_eq!(HASHMAP_MAX_LEN, 74, "protocol constant from default MTU");

        let (link, peer) = make_test_link();
        let mut rng = rand_core::OsRng;

        // > 74 parts so the advertisement's hashmap is a strict prefix.
        let data: Vec<u8> = (0..80 * 464u32).map(|i| (i * 17 % 249) as u8).collect();
        let mut res = OutgoingResource::new(
            &data,
            None,
            None,
            &link.resource_crypt_params(),
            false,
            &mut rng,
            1_000,
        )
        .unwrap();
        let adv = ResourceAdvertisement::unpack(res.adv_packet()).unwrap();
        assert!(res.parts.len() > HASHMAP_MAX_LEN);
        assert_eq!(
            adv.hashmap_data.len(),
            HASHMAP_MAX_LEN * RESOURCE_HASHMAP_LEN,
            "the advertisement carries exactly the first hashmap segment"
        );

        // The receiver's exhaustion report: last advertised map hash is entry
        // 73; the sender's scan yields part_index 74 — 74 % 74 == 0, the
        // alignment the reference sender enforces before answering.
        let last_advertised = &adv.hashmap_data[(HASHMAP_MAX_LEN - 1) * RESOURCE_HASHMAP_LEN..];
        let mut req = vec![HASHMAP_IS_EXHAUSTED];
        req.extend_from_slice(last_advertised);
        req.extend_from_slice(&adv.resource_hash);

        let packets = res.handle_request(&req, &link, &mut rng, 2_000).unwrap();
        let hmu_wire = packets
            .iter()
            .find(|p| p.len() > 18 && p[18] == crate::packet::PacketContext::ResourceHmu.to_byte())
            .expect("an exhausted REQ must be answered with an HMU packet");

        let mut buf = vec![0u8; hmu_wire.len()];
        let n = peer.decrypt(&hmu_wire[19..], &mut buf).unwrap();
        buf.truncate(n);

        assert_eq!(&buf[..32], &adv.resource_hash, "HMU must lead with h");
        assert_eq!(buf[32], 0x92, "then msgpack fixarray(2)");
        assert_eq!(buf[33], 0x01, "segment must be 1: entries 74.. live there");
        // Remaining entries 74..n as one bin.
        let remaining = res.parts.len() - HASHMAP_MAX_LEN;
        assert_eq!(buf[34], 0xC4, "hashmap bytes ride as msgpack bin8");
        assert_eq!(buf[35] as usize, remaining * RESOURCE_HASHMAP_LEN);
        for i in 0..remaining {
            assert_eq!(
                buf[36 + i * RESOURCE_HASHMAP_LEN..36 + (i + 1) * RESOURCE_HASHMAP_LEN],
                res.hashmap[HASHMAP_MAX_LEN + i],
                "HMU entry {i} must be map hash 74+{i}: the receiver writes \
                 it at offset segment*74 + {i}"
            );
        }
    }
}
