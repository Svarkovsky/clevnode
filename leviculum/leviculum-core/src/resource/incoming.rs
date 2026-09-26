//! Incoming resource state machine (receiver side).
//!
//! Sans-I/O: receives packets via methods, returns raw packet bytes to send.
//! Owned by Link (same pattern as Channel).

use alloc::vec;
use alloc::vec::Vec;

use crate::constants::RESOURCE_HASHMAP_LEN;
use crate::crypto::full_hash;
use crate::hex_fmt::HexFmt;
use crate::link::Link;
use crate::resource::hashmap::map_hash;
use crate::resource::msgpack;
use crate::resource::window::{RateSample, WindowPolicy, WindowState};
use crate::resource::{
    ResourceAdvertisement, ResourceError, ResourceFlags, ResourceStatus, HASHMAP_IS_EXHAUSTED,
    HASHMAP_IS_NOT_EXHAUSTED, HASHMAP_MAX_LEN, PART_TIMEOUT_FACTOR_AFTER_RTT,
    PART_TIMEOUT_FACTOR_INITIAL, PER_RETRY_DELAY_MS, RESOURCE_MAX_EFFICIENT_SIZE,
    RESOURCE_MAX_RETRIES, RESOURCE_RANDOM_HASH_SIZE, RESOURCE_WINDOW_FLEXIBILITY,
    RETRY_GRACE_TIME_MS,
};

use super::outgoing::ResourcePollResult;

/// Result of receiving a data part.
#[derive(Debug)]
pub(crate) enum ResourcePartResult {
    /// More parts needed, no immediate action.
    Continue,
    /// Send this REQ packet (encrypted by caller).
    SendRequest(Vec<u8>),
    /// All parts received, caller should call `assemble()`.
    Assembling,
    /// Part did not match any expected hash.
    InvalidPart,
}

/// Incoming resource transfer state machine.
///
/// Fields like `original_hash`, `data_size`, `total_segments`, and `request_id`
/// are stored from the resource advertisement and needed for protocol correctness
/// (hash verification, proof generation, multi-segment reassembly). Accessors are
/// provided and exercised in tests; production callers tracked in Codeberg issues #27/#28
/// (Resource Transfer).
#[allow(dead_code)] // Protocol state fields — see Codeberg issues #27/#28
pub(crate) struct IncomingResource {
    status: ResourceStatus,
    flags: ResourceFlags,
    resource_hash: [u8; 32],
    original_hash: [u8; 32],
    random_hash: [u8; RESOURCE_RANDOM_HASH_SIZE],
    transfer_size: u64,
    data_size: u64,
    num_parts: u32,
    segment_index: u32,
    total_segments: u32,
    request_id: Option<Vec<u8>>,
    // Parts storage
    parts: Vec<Option<Vec<u8>>>,
    hashmap: Vec<Option<[u8; RESOURCE_HASHMAP_LEN]>>,
    hashmap_height: usize,
    consecutive_completed_height: usize,
    // Window (Codeberg #85: state + policy live in resource::window)
    window_state: WindowState,
    window_policy: WindowPolicy,
    outstanding_parts: usize,
    // Timing
    last_activity_ms: u64,
    req_sent_ms: Option<u64>,
    // Transfer metrics
    eifr: u64,
    data_received: bool,
    retries: usize,
    waiting_for_hmu: bool,
    link_mdu: usize,
    sdu: usize,
    // For proof computation
    assembled_with_metadata: Option<Vec<u8>>,
    // Last REQ payload (for retransmission on timeout)
    last_req: Option<Vec<u8>>,
}

impl IncomingResource {
    /// Create from a received advertisement.
    ///
    /// Returns `(incoming_resource, first_req_payload)`.
    /// The caller must encrypt and send the REQ payload.
    pub(crate) fn from_advertisement(
        adv: &ResourceAdvertisement,
        link_mdu: usize,
        sdu: usize,
        now_ms: u64,
        max_size: usize,
        window_policy: WindowPolicy,
    ) -> Result<(Self, Vec<u8>), ResourceError> {
        // Reject oversized resources before allocating
        if adv.transfer_size as usize > max_size {
            return Err(ResourceError::ResourceTooLarge);
        }

        // `d` is the total uncompressed size of the whole transfer, summed
        // over all segments (Resource.py:1282 `self.d = resource.total_size`),
        // so it is NOT bounded by the per-advertisement `max_size`. The
        // reference does bound it against the segment count it advertises:
        // `total_segments = ((total_size-1)//MAX_EFFICIENT_SIZE)+1`
        // (Resource.py:296), i.e. `total_size <= l * MAX_EFFICIENT_SIZE`.
        // Reject anything outside that. `l == 0` is treated as one segment,
        // the way Python's `if adv.l > 1` treats it (Resource.py:203).
        //
        // This is a self-consistency check, not a security boundary: `l` is
        // itself an unvalidated `u32` off the wire, so a hostile sender picks
        // it large enough to saturate the product and make the comparison
        // vacuous. It costs nothing, because nothing sizes an allocation from
        // `l` either; the gate that actually bounds decompression is the
        // unconditional `MAX_DECOMPRESS_HINT` clamp in `bz2_decompress`.
        let advertised_segments = adv.total_segments.max(1) as u64;
        let max_data_size = advertised_segments.saturating_mul(RESOURCE_MAX_EFFICIENT_SIZE as u64);
        if adv.data_size > max_data_size {
            return Err(ResourceError::InvalidAdvertisement);
        }

        // A link whose SDU is zero cannot carry resource parts at all, and
        // deriving a part count from it would divide by zero.
        if sdu == 0 {
            return Err(ResourceError::InvalidRequest);
        }

        // Derive the part count from the size we just bounded, exactly as the
        // reference does: `total_parts = ceil(size / sdu)` (Resource.py:187,
        // computed from `adv.t`). Python never reads the advertisement's `n`
        // at all, so a peer cannot size our allocations with it.
        let num_parts = u32::try_from((adv.transfer_size as usize).div_ceil(sdu))
            .map_err(|_| ResourceError::InvalidAdvertisement)?;

        // `n` is still read off the wire, but only to reject an advertisement
        // that contradicts itself. It never sizes an allocation.
        if adv.num_parts > num_parts {
            return Err(ResourceError::InvalidAdvertisement);
        }

        // Initialize hashmap from advertisement's hashmap_data
        let initial_entries = adv.hashmap_data.len() / RESOURCE_HASHMAP_LEN;
        let mut hashmap = vec![None; num_parts as usize];
        let mut hashmap_height = 0;

        for (i, slot) in hashmap
            .iter_mut()
            .enumerate()
            .take(initial_entries.min(num_parts as usize))
        {
            let start = i * RESOURCE_HASHMAP_LEN;
            let mut entry = [0u8; RESOURCE_HASHMAP_LEN];
            entry.copy_from_slice(&adv.hashmap_data[start..start + RESOURCE_HASHMAP_LEN]);
            *slot = Some(entry);
            hashmap_height += 1;
        }

        let mut incoming = Self {
            status: ResourceStatus::Transferring,
            flags: adv.flags,
            resource_hash: adv.resource_hash,
            original_hash: adv.original_hash,
            random_hash: adv.random_hash,
            transfer_size: adv.transfer_size,
            data_size: adv.data_size,
            num_parts,
            segment_index: adv.segment_index,
            total_segments: adv.total_segments,
            request_id: adv.request_id.clone(),
            parts: vec![None; num_parts as usize],
            hashmap,
            hashmap_height,
            consecutive_completed_height: 0,
            window_state: WindowState::new(),
            window_policy,
            outstanding_parts: 0,
            last_activity_ms: now_ms,
            req_sent_ms: None,
            eifr: 0,
            data_received: false,
            retries: 0,
            waiting_for_hmu: false,
            link_mdu,
            sdu,
            assembled_with_metadata: None,
            last_req: None,
        };

        // Build first request
        let req = incoming.build_request();
        incoming.last_activity_ms = now_ms;
        incoming.req_sent_ms = Some(now_ms);

        Ok((incoming, req))
    }

    /// Build a REQ packet payload for the next window of parts.
    ///
    /// Wire format: `[1:exhausted_flag][4?:last_map_hash][32:resource_hash][N*4:requested_hashes]`
    fn build_request(&mut self) -> Vec<u8> {
        self.outstanding_parts = 0;
        let mut hashmap_exhausted = HASHMAP_IS_NOT_EXHAUSTED;
        let mut requested_hashes = Vec::new();

        // Update consecutive_completed_height
        while self.consecutive_completed_height < self.num_parts as usize
            && self.parts[self.consecutive_completed_height].is_some()
        {
            self.consecutive_completed_height += 1;
        }

        let search_start = self.consecutive_completed_height;
        let search_end = core::cmp::min(
            search_start + self.window_state.window(),
            self.num_parts as usize,
        );

        for pn in search_start..search_end {
            if self.parts[pn].is_none() {
                if let Some(hash) = self.hashmap[pn] {
                    requested_hashes.extend_from_slice(&hash);
                    self.outstanding_parts += 1;
                } else {
                    hashmap_exhausted = HASHMAP_IS_EXHAUSTED;
                    break;
                }
            }
        }

        let mut req = Vec::with_capacity(
            1 + if hashmap_exhausted == HASHMAP_IS_EXHAUSTED {
                RESOURCE_HASHMAP_LEN
            } else {
                0
            } + 32
                + requested_hashes.len(),
        );

        req.push(hashmap_exhausted);
        if hashmap_exhausted == HASHMAP_IS_EXHAUSTED {
            // Append last known map hash
            if self.hashmap_height > 0 {
                if let Some(last_hash) = self.hashmap[self.hashmap_height - 1] {
                    crate::tracing::debug!(
                        "REQ: HASHMAP_EXHAUSTED, hashmap_height={}, last_map_hash={:02x}{:02x}{:02x}{:02x}, outstanding={}, consecutive_height={}, window={}",
                        self.hashmap_height,
                        last_hash[0], last_hash[1], last_hash[2], last_hash[3],
                        self.outstanding_parts,
                        self.consecutive_completed_height,
                        self.window_state.window(),
                    );
                    req.extend_from_slice(&last_hash);
                } else {
                    crate::tracing::warn!("REQ: HASHMAP_EXHAUSTED but last entry is None!");
                    req.extend_from_slice(&[0u8; RESOURCE_HASHMAP_LEN]);
                }
            } else {
                crate::tracing::warn!("REQ: HASHMAP_EXHAUSTED but hashmap_height=0!");
                req.extend_from_slice(&[0u8; RESOURCE_HASHMAP_LEN]);
            }
            self.waiting_for_hmu = true;
        } else {
            crate::tracing::debug!(
                "REQ: NOT_EXHAUSTED, outstanding={}, consecutive_height={}, hashmap_height={}, window={}",
                self.outstanding_parts,
                self.consecutive_completed_height,
                self.hashmap_height,
                self.window_state.window(),
            );
        }

        req.extend_from_slice(&self.resource_hash);
        req.extend_from_slice(&requested_hashes);

        self.window_state.start_round();
        self.last_req = Some(req.clone());
        req
    }

    /// Receive a data part (RESOURCE context packet).
    pub(crate) fn receive_part(
        &mut self,
        part_data: &[u8],
        now_ms: u64,
        _rtt_ms: u64,
    ) -> ResourcePartResult {
        if self.status != ResourceStatus::Transferring {
            crate::tracing::debug!(
                event = "RESOURCE_PART_REJECT",
                rh = %HexFmt(&self.resource_hash[..4]),
                reason = "not_transferring",
                status = ?self.status,
            );
            return ResourcePartResult::InvalidPart;
        }

        // Compute map hash and find matching part
        let mh = map_hash(part_data, &self.random_hash);

        // Search within window scope for matching hashmap entry
        let search_start = self.consecutive_completed_height;
        let search_end = core::cmp::min(
            search_start + self.window_state.window() + RESOURCE_WINDOW_FLEXIBILITY,
            self.num_parts as usize,
        );

        let mut matched_index = None;
        for i in search_start..search_end {
            if self.parts[i].is_none() {
                if let Some(entry) = self.hashmap[i] {
                    if entry == mh {
                        matched_index = Some(i);
                        break;
                    }
                }
            }
        }

        let Some(index) = matched_index else {
            // An arriving-but-rejected part (#85): the part crossed the air
            // but matched no hashmap entry in the current window scope.
            crate::tracing::debug!(
                event = "RESOURCE_PART_REJECT",
                rh = %HexFmt(&self.resource_hash[..4]),
                reason = "no_matching_hash",
                mh = %HexFmt(&mh),
                consecutive = self.consecutive_completed_height,
                win_start = search_start,
                win_end = search_end,
            );
            return ResourcePartResult::InvalidPart;
        };

        // Store the part
        self.parts[index] = Some(part_data.to_vec());
        self.outstanding_parts = self.outstanding_parts.saturating_sub(1);
        self.window_state.record_part(part_data.len());
        self.last_activity_ms = now_ms;
        self.data_received = true;
        self.retries = 0;

        // Per stored part (#85): between round boundaries a stored part
        // previously logged nothing.
        crate::tracing::debug!(
            event = "RESOURCE_PART_RX",
            rh = %HexFmt(&self.resource_hash[..4]),
            idx = index,
            outstanding = self.outstanding_parts,
            consecutive = self.consecutive_completed_height,
        );

        // Update EIFR based on RTT
        if let Some(req_sent) = self.req_sent_ms {
            let elapsed = now_ms.saturating_sub(req_sent);
            if elapsed > 0 && self.window_state.parts_received_this_window() == 1 {
                // First part of this window, measure data RTT rate
                let bytes_per_part = if self.num_parts > 0 {
                    self.transfer_size / self.num_parts as u64
                } else {
                    self.sdu as u64
                };
                self.eifr = bytes_per_part.saturating_mul(1000) / elapsed;
            }
        }

        // Check if all outstanding parts are received
        if self.outstanding_parts == 0 {
            // Round complete: hand the rate measurements to the window policy
            // (Codeberg #85). first_part_rate is the persisted eifr the
            // historical logic reacted to; round_rate is the whole-round
            // goodput for future policies.
            let round_elapsed_ms = self
                .req_sent_ms
                .map(|req_sent| now_ms.saturating_sub(req_sent))
                .unwrap_or(0);
            let sample = RateSample {
                first_part_rate: self.eifr,
                round_rate: self
                    .window_state
                    .bytes_received_this_window()
                    .saturating_mul(1000)
                    / core::cmp::max(round_elapsed_ms, 1),
            };
            self.window_state
                .on_round_complete(self.window_policy, sample);

            let missing_parts = self.parts.iter().filter(|p| p.is_none()).count();
            crate::tracing::debug!(
                event = "RESOURCE_RW",
                rh = %HexFmt(&self.resource_hash[..4]),
                round = self.window_state.rounds_completed(),
                window = self.window_state.window(),
                wmax = self.window_state.window_max(),
                rate = sample.first_part_rate,
                outst = missing_parts,
                t = round_elapsed_ms,
            );

            // Check if ALL parts received
            let all_received = missing_parts == 0;
            if all_received {
                self.status = ResourceStatus::Assembling;
                return ResourcePartResult::Assembling;
            }

            // Not done yet, build next request
            if !self.waiting_for_hmu {
                let req = self.build_request();
                self.req_sent_ms = Some(now_ms);
                return ResourcePartResult::SendRequest(req);
            } else {
                crate::tracing::debug!(
                    "Window complete but waiting_for_hmu=true, consecutive_height={}, hashmap_height={}",
                    self.consecutive_completed_height,
                    self.hashmap_height,
                );
            }
        }

        ResourcePartResult::Continue
    }

    /// Process a hashmap update (HMU) packet.
    pub(crate) fn handle_hashmap_update(
        &mut self,
        now_ms: u64,
        hmu_data: &[u8],
    ) -> Result<Option<Vec<u8>>, ResourceError> {
        if self.status == ResourceStatus::Failed {
            return Err(ResourceError::Cancelled);
        }

        // HMU format: [32: resource_hash] [msgpack: [segment_number, hashmap_bytes]]
        if hmu_data.len() < 32 {
            return Err(ResourceError::InvalidHashmap);
        }

        let hmu_resource_hash = &hmu_data[..32];
        if hmu_resource_hash != self.resource_hash {
            return Err(ResourceError::InvalidHashmap);
        }

        // Parse msgpack fixarray(2): [segment_number, hashmap_bytes]
        let msgpack_data = &hmu_data[32..];
        let mut pos = 0;

        let array_len = msgpack::read_fixarray_len(msgpack_data, &mut pos)
            .ok_or(ResourceError::InvalidHashmap)?;
        if array_len != 2 {
            return Err(ResourceError::InvalidHashmap);
        }

        let segment = msgpack::read_msgpack_uint(msgpack_data, &mut pos)
            .ok_or(ResourceError::InvalidHashmap)? as usize;
        let hashmap_bytes = msgpack::read_msgpack_bin(msgpack_data, &mut pos)
            .ok_or(ResourceError::InvalidHashmap)?;

        // Parse hashmap entries
        // Use HASHMAP_MAX_LEN (protocol constant from standard Link.MDU),
        // NOT the negotiated link_mdu. Python's ResourceAdvertisement.HASHMAP_MAX_LEN
        // is a class constant that doesn't change with MTU discovery.
        let seg_len = HASHMAP_MAX_LEN;
        let num_entries = hashmap_bytes.len() / RESOURCE_HASHMAP_LEN;

        crate::tracing::debug!(
            "HMU received: segment={}, seg_len={}, num_entries={}, hashmap_height_before={}",
            segment,
            seg_len,
            num_entries,
            self.hashmap_height,
        );

        for i in 0..num_entries {
            let idx = i + segment * seg_len;
            if idx >= self.hashmap.len() {
                break;
            }
            let start = i * RESOURCE_HASHMAP_LEN;
            let mut entry = [0u8; RESOURCE_HASHMAP_LEN];
            entry.copy_from_slice(&hashmap_bytes[start..start + RESOURCE_HASHMAP_LEN]);
            if self.hashmap[idx].is_none() {
                self.hashmap_height += 1;
            }
            self.hashmap[idx] = Some(entry);
        }

        self.waiting_for_hmu = false;
        // last_activity_ms intentionally not updated. HMU is a control message,
        // not data progress, so the timeout should continue from the last data part.
        self.retries = 0;

        // Now that we have more hashmap entries, build next request. Reset
        // req_sent_ms like every other REQ producer, so the next round's rate
        // samples measure the REQ to first part interval instead of spanning
        // the previous round plus the HMU round trip (Python Resource.py:971).
        let req = self.build_request();
        self.req_sent_ms = Some(now_ms);
        Ok(Some(req))
    }

    /// Assemble the complete data from all received parts.
    ///
    /// Returns `(application_data, optional_metadata)`.
    pub(crate) fn assemble(
        &mut self,
        link: &Link,
    ) -> Result<(Vec<u8>, Option<Vec<u8>>), ResourceError> {
        if self.status != ResourceStatus::Assembling {
            return Err(ResourceError::InvalidRequest);
        }

        // 1. Concatenate all parts
        let mut stream = Vec::with_capacity(self.transfer_size as usize);
        for part in &self.parts {
            match part {
                Some(data) => stream.extend_from_slice(data),
                None => return Err(ResourceError::HashMismatch),
            }
        }

        // 2. Decrypt
        let mut decrypted = vec![0u8; stream.len()];
        let plaintext_len = link
            .decrypt(&stream, &mut decrypted)
            .map_err(|_| ResourceError::CryptoError)?;
        decrypted.truncate(plaintext_len);

        // 3. Strip LEADING wire_random bytes
        if decrypted.len() < RESOURCE_RANDOM_HASH_SIZE {
            return Err(ResourceError::HashMismatch);
        }
        let stripped = &decrypted[RESOURCE_RANDOM_HASH_SIZE..];

        // 4. Decompress if needed
        let assembled = if self.flags.compressed {
            #[cfg(feature = "compression")]
            {
                super::compression::bz2_decompress(stripped, self.data_size as usize)?
            }
            #[cfg(not(feature = "compression"))]
            {
                return Err(ResourceError::CompressionUnsupported);
            }
        } else {
            stripped.to_vec()
        };

        // 5. Verify hash: full_hash(assembled + random_hash) == resource_hash
        let mut hash_input = Vec::with_capacity(assembled.len() + RESOURCE_RANDOM_HASH_SIZE);
        hash_input.extend_from_slice(&assembled);
        hash_input.extend_from_slice(&self.random_hash);
        let calculated = full_hash(&hash_input);
        if calculated != self.resource_hash {
            self.status = ResourceStatus::Corrupt;
            return Err(ResourceError::HashMismatch);
        }

        // Store assembled data (including metadata prefix) for proof computation
        self.assembled_with_metadata = Some(assembled.clone());

        // 6. Extract metadata if present (only in segment 1, per Python Resource.py:685)
        let (app_data, metadata) = if self.flags.has_metadata && self.segment_index == 1 {
            if assembled.len() < 3 {
                return Err(ResourceError::HashMismatch);
            }
            let meta_len = ((assembled[0] as usize) << 16)
                | ((assembled[1] as usize) << 8)
                | (assembled[2] as usize);
            if assembled.len() < 3 + meta_len {
                return Err(ResourceError::HashMismatch);
            }
            let metadata = assembled[3..3 + meta_len].to_vec();
            let data = assembled[3 + meta_len..].to_vec();
            (data, Some(metadata))
        } else {
            (assembled, None)
        };

        self.status = ResourceStatus::Complete;
        Ok((app_data, metadata))
    }

    /// Build the completion proof.
    ///
    /// Must be called AFTER `assemble()` succeeds.
    /// Returns the proof payload: `[32: resource_hash][32: proof_hash]`.
    pub(crate) fn build_proof(&self) -> Result<Vec<u8>, ResourceError> {
        let assembled = self
            .assembled_with_metadata
            .as_ref()
            .ok_or(ResourceError::InvalidRequest)?;

        // proof = full_hash(assembled_with_metadata + resource_hash)
        let mut proof_input = Vec::with_capacity(assembled.len() + 32);
        proof_input.extend_from_slice(assembled);
        proof_input.extend_from_slice(&self.resource_hash);
        let proof_hash = full_hash(&proof_input);

        let mut proof_data = Vec::with_capacity(64);
        proof_data.extend_from_slice(&self.resource_hash);
        proof_data.extend_from_slice(&proof_hash);
        Ok(proof_data)
    }

    /// Poll for timeout.
    pub(crate) fn poll(&mut self, now_ms: u64, rtt_ms: u64) -> ResourcePollResult {
        let rtt_ms = core::cmp::max(rtt_ms, 1);

        match self.status {
            ResourceStatus::Transferring => {
                // Timeout factor reduces after first data received
                // (Python Resource.py:828. PART_TIMEOUT_FACTOR_AFTER_RTT).
                let timeout_factor = if self.data_received {
                    PART_TIMEOUT_FACTOR_AFTER_RTT // 2
                } else {
                    PART_TIMEOUT_FACTOR_INITIAL // 4
                };

                // Base timeout: expected time-of-flight for outstanding parts.
                // When eifr is measured, per_part_tof = bytes_per_part * 1000 / eifr.
                // Cap at rtt_ms: a single part is one packet, should arrive within
                // one RTT. If measured eifr suggests longer, the measurement is
                // contaminated by dropped frames inflating the req-to-first-part
                // elapsed time. Python avoids this by falling back to the link
                // establishment rate (Resource.py:552).
                let eifr_tof = if self.num_parts > 0 && self.eifr > 0 {
                    self.transfer_size.saturating_mul(1000) / self.num_parts as u64 / self.eifr
                } else {
                    rtt_ms
                };
                let per_part_tof = core::cmp::min(eifr_tof, rtt_ms);
                let base = per_part_tof * core::cmp::max(self.outstanding_parts, 1) as u64;
                // Per-retry progressive delay (Python Resource.py:594).
                let per_retry_extra = self.retries as u64 * PER_RETRY_DELAY_MS;
                let timeout = base * timeout_factor + RETRY_GRACE_TIME_MS + per_retry_extra;

                if now_ms.saturating_sub(self.last_activity_ms) >= timeout {
                    self.retries += 1;
                    crate::tracing::debug!(
                        "Resource timeout: retry={}/{}, waiting_for_hmu={}, consecutive_height={}, hashmap_height={}, outstanding={}",
                        self.retries, RESOURCE_MAX_RETRIES,
                        self.waiting_for_hmu,
                        self.consecutive_completed_height,
                        self.hashmap_height,
                        self.outstanding_parts,
                    );
                    if self.retries >= RESOURCE_MAX_RETRIES {
                        self.status = ResourceStatus::Failed;
                        ResourcePollResult::TimedOut
                    } else {
                        // Policy hook (Codeberg #85): Current is a no-op, the
                        // historical logic never touches the window on timeout.
                        self.window_state.on_timeout(self.window_policy);
                        self.last_activity_ms = now_ms;
                        // Rebuild request with only the currently missing parts
                        // (matches Python Resource.py:622, request_next() on timeout).
                        let req = self.build_request();
                        self.req_sent_ms = Some(now_ms);
                        ResourcePollResult::RetransmitAdv(req)
                    }
                } else {
                    ResourcePollResult::Nothing
                }
            }
            _ => ResourcePollResult::Nothing,
        }
    }

    /// Compute the next deadline (absolute ms).
    pub(crate) fn next_deadline(&self, rtt_ms: u64) -> Option<u64> {
        let rtt_ms = core::cmp::max(rtt_ms, 1);
        match self.status {
            ResourceStatus::Transferring => {
                let timeout_factor = if self.data_received {
                    PART_TIMEOUT_FACTOR_AFTER_RTT
                } else {
                    PART_TIMEOUT_FACTOR_INITIAL
                };
                let eifr_tof = if self.num_parts > 0 && self.eifr > 0 {
                    self.transfer_size.saturating_mul(1000) / self.num_parts as u64 / self.eifr
                } else {
                    rtt_ms
                };
                let per_part_tof = core::cmp::min(eifr_tof, rtt_ms);
                let base = per_part_tof * core::cmp::max(self.outstanding_parts, 1) as u64;
                let per_retry_extra = self.retries as u64 * PER_RETRY_DELAY_MS;
                let timeout = base * timeout_factor + RETRY_GRACE_TIME_MS + per_retry_extra;
                Some(self.last_activity_ms.saturating_add(timeout))
            }
            _ => None,
        }
    }

    /// Mark as failed/cancelled.
    #[allow(dead_code)] // Resource cancel API — see Codeberg issues #27/#28
    pub(crate) fn cancel(&mut self) {
        self.status = ResourceStatus::Failed;
    }

    // Accessors
    pub(crate) fn status(&self) -> ResourceStatus {
        self.status
    }

    pub(crate) fn resource_hash(&self) -> &[u8; 32] {
        &self.resource_hash
    }

    pub(crate) fn progress(&self) -> f32 {
        if self.num_parts == 0 {
            return 1.0;
        }
        let received = self.parts.iter().filter(|p| p.is_some()).count();
        received as f32 / self.num_parts as f32
    }

    pub(crate) fn transfer_size(&self) -> u64 {
        self.transfer_size
    }

    pub(crate) fn data_size(&self) -> u64 {
        self.data_size
    }

    #[allow(dead_code)] // Resource accessor API — see Codeberg issues #27/#28
    pub(crate) fn original_hash(&self) -> &[u8; 32] {
        &self.original_hash
    }

    pub(crate) fn segment_index(&self) -> u32 {
        self.segment_index
    }

    #[allow(dead_code)] // Resource accessor API — see Codeberg issues #27/#28
    pub(crate) fn total_segments(&self) -> u32 {
        self.total_segments
    }

    /// The request id carried in the advertisement (`is_response` transfers),
    /// correlating a raw file response to its pending request.
    pub(crate) fn request_id(&self) -> Option<&[u8]> {
        self.request_id.as_deref()
    }

    /// Whether the advertisement flagged this resource as a request response
    /// (`is_response`). A response resource that matches an outstanding request
    /// is delivered as `ResponseReceived`, not a generic `ResourceCompleted`.
    pub(crate) fn is_response(&self) -> bool {
        self.flags.is_response
    }

    pub(crate) fn flags(&self) -> ResourceFlags {
        self.flags
    }

    /// Receive-window observability for the deterministic measurement
    /// harness (Codeberg #85, `node::mvr_resource_window`).
    #[cfg(test)]
    pub(crate) fn window_state(&self) -> &WindowState {
        &self.window_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ResourceAdvertisement;

    fn make_test_adv(num_parts: u32, hashmap_data: Vec<u8>) -> ResourceAdvertisement {
        ResourceAdvertisement {
            transfer_size: 464,
            data_size: 100,
            num_parts,
            resource_hash: [0xAA; 32],
            random_hash: [0xBB; RESOURCE_RANDOM_HASH_SIZE],
            original_hash: [0xCC; 32],
            segment_index: 1,
            total_segments: 1,
            request_id: None,
            flags: ResourceFlags {
                encrypted: true,
                ..Default::default()
            },
            hashmap_data,
        }
    }

    #[test]
    fn test_incoming_from_advertisement() {
        // 1 part with a known map hash
        let hashmap_data = vec![0x11, 0x22, 0x33, 0x44];
        let adv = make_test_adv(1, hashmap_data);

        let (incoming, req) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        assert_eq!(incoming.status(), ResourceStatus::Transferring);
        assert_eq!(incoming.num_parts, 1);
        assert_eq!(incoming.hashmap_height, 1);
        assert!(!req.is_empty());
    }

    #[test]
    fn test_incoming_req_wire_format() {
        let hashmap_data = vec![0x11, 0x22, 0x33, 0x44];
        let adv = make_test_adv(1, hashmap_data);

        let (_, req) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        // REQ: [0x00 (not exhausted)] [32: resource_hash] [4: requested_hash]
        assert_eq!(req[0], HASHMAP_IS_NOT_EXHAUSTED);
        assert_eq!(&req[1..33], &[0xAA; 32]);
        assert_eq!(&req[33..37], &[0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn test_incoming_req_exhausted_format() {
        // 2 parts but only 1 hashmap entry
        let hashmap_data = vec![0x11, 0x22, 0x33, 0x44];
        let mut adv = make_test_adv(2, hashmap_data);
        adv.transfer_size = 928; // 2 * 464 sdu

        let (_, req) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        // Should be exhausted since we only have 1 hash but need 2+ parts
        // Window is 4 (initial), so parts 0 and 1 will be scanned
        // Part 0 has hash, part 1 doesn't → exhausted
        assert_eq!(req[0], HASHMAP_IS_EXHAUSTED);
        // Last known hash follows
        assert_eq!(&req[1..5], &[0x11, 0x22, 0x33, 0x44]);
        // Then resource_hash
        assert_eq!(&req[5..37], &[0xAA; 32]);
    }

    #[test]
    fn test_incoming_progress() {
        let hashmap_data = vec![0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let mut adv = make_test_adv(2, hashmap_data);
        adv.transfer_size = 928; // 2 * 464 sdu

        let (incoming, _) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        assert_eq!(incoming.progress(), 0.0);
    }

    #[test]
    fn test_incoming_hmu_parsing() {
        let hashmap_data = vec![0x11, 0x22, 0x33, 0x44];
        let mut adv = make_test_adv(3, hashmap_data);
        adv.transfer_size = 1392; // 3 * 464 sdu

        let (mut incoming, _) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        assert_eq!(incoming.hashmap_height, 1);

        // Build HMU: resource_hash + msgpack([1, bin([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11])])
        let mut hmu = Vec::new();
        hmu.extend_from_slice(&[0xAA; 32]); // resource_hash
        msgpack::write_fixarray_header(&mut hmu, 2);
        msgpack::write_uint(&mut hmu, 1); // segment 1
        let hmu_hashes = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11];
        msgpack::write_bin(&mut hmu, &hmu_hashes);

        let result = incoming.handle_hashmap_update(1000, &hmu);
        assert!(result.is_ok());

        // Should have added 2 more hashmap entries at positions seg_len*1..
        // HASHMAP_MAX_LEN = 74, so entries go at indices 74 and 75
        // But we only have 3 parts, so index 74 and 75 are out of range
        // The entries are at segment*seg_len + i = 74 + 0 = 74, 74 + 1 = 75
        // Both are >= 3 (num_parts), so they won't be stored
        // This test mainly verifies parsing doesn't crash
    }

    /// The REQ built in response to an HMU must reset `req_sent_ms`, exactly
    /// like every other REQ producer. Otherwise the next round's rate samples
    /// (eifr and round_rate) are measured from the PREVIOUS round's REQ,
    /// spanning that round plus the HMU round trip, which under-measures the
    /// link by 2-3x and can push a healthy slow link below the VERY_SLOW rate
    /// tier, permanently pinning window_max to 4 on >74-part transfers.
    /// Python resets it on every REQ including HMU-driven ones
    /// (Resource.py:971-974, request_next).
    #[test]
    fn test_hmu_req_resets_rate_measurement_baseline() {
        const NUM_PARTS: usize = 100;
        const SDU: usize = 464;
        let random_hash = [0xBB; RESOURCE_RANDOM_HASH_SIZE];

        let parts: Vec<Vec<u8>> = (0..NUM_PARTS)
            .map(|i| {
                let mut p = vec![0u8; SDU];
                p[0] = (i & 0xff) as u8;
                p[1] = ((i >> 8) & 0xff) as u8;
                p
            })
            .collect();

        // The advertisement carries only the first HASHMAP_MAX_LEN (74)
        // entries, so the receiver must exhaust the hashmap and wait for an
        // HMU before it can request parts 74..100.
        let mut adv_hashmap = Vec::new();
        for p in parts.iter().take(HASHMAP_MAX_LEN) {
            adv_hashmap.extend_from_slice(&map_hash(p, &random_hash));
        }
        let mut adv = make_test_adv(NUM_PARTS as u32, adv_hashmap);
        adv.transfer_size = (NUM_PARTS * SDU) as u64; // bytes_per_part = 464

        let mut now: u64 = 1000;
        let (mut incoming, first_req) = IncomingResource::from_advertisement(
            &adv,
            431,
            SDU,
            now,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        fn hashes_in_req(req: &[u8]) -> usize {
            let header = if req[0] == HASHMAP_IS_EXHAUSTED {
                1 + RESOURCE_HASHMAP_LEN + 32
            } else {
                1 + 32
            };
            (req.len() - header) / RESOURCE_HASHMAP_LEN
        }

        // Drive rounds at a healthy pace (first part 50 ms after each REQ,
        // 464_000 / 50 = 9280 B/s) until the REQ comes back exhausted and the
        // receiver waits for the HMU.
        let mut req = first_req;
        let mut next_part = 0;
        loop {
            let exhausted = req[0] == HASHMAP_IS_EXHAUSTED;
            let count = hashes_in_req(&req);
            assert!(count > 0, "every driven REQ must request parts");
            now += 50;
            let mut last = ResourcePartResult::Continue;
            for _ in 0..count {
                last = incoming.receive_part(&parts[next_part], now, 100);
                assert!(
                    !matches!(last, ResourcePartResult::InvalidPart),
                    "part {next_part} must match its hashmap entry"
                );
                next_part += 1;
            }
            if exhausted {
                assert!(
                    matches!(last, ResourcePartResult::Continue),
                    "exhausted round must wait for the HMU, got {last:?}"
                );
                break;
            }
            req = match last {
                ResourcePartResult::SendRequest(r) => r,
                other => panic!("round must complete with a next REQ, got {other:?}"),
            };
        }
        assert_eq!(next_part, HASHMAP_MAX_LEN, "all advertised hashes consumed");

        // The HMU (segment 1: hashes 74..100) arrives well after the round
        // completed; the gap models the HMU round trip on a slow link.
        now += 5000;
        let mut hmu = Vec::new();
        hmu.extend_from_slice(&[0xAA; 32]);
        msgpack::write_fixarray_header(&mut hmu, 2);
        msgpack::write_uint(&mut hmu, 1);
        let mut seg1 = Vec::new();
        for p in parts.iter().skip(HASHMAP_MAX_LEN) {
            seg1.extend_from_slice(&map_hash(p, &random_hash));
        }
        msgpack::write_bin(&mut hmu, &seg1);

        let req = incoming
            .handle_hashmap_update(now, &hmu)
            .unwrap()
            .expect("HMU must produce the next REQ");
        assert!(hashes_in_req(&req) > 0, "post-HMU REQ must request parts");
        assert_eq!(
            incoming.req_sent_ms,
            Some(now),
            "HMU-driven REQ must reset req_sent_ms (Python Resource.py:971)"
        );

        // First part of the post-HMU round lands 50 ms later. The measured
        // first-part rate must cover ONLY those 50 ms (9280 B/s), not the
        // previous round plus the HMU round trip.
        now += 50;
        incoming.receive_part(&parts[next_part], now, 100);
        assert_eq!(
            incoming.eifr, 9280,
            "post-HMU first-part rate must be measured from the HMU REQ"
        );
    }

    #[test]
    fn test_incoming_resource_accessors_and_cancel() {
        let hashmap_data = vec![0x11, 0x22, 0x33, 0x44];
        let adv = make_test_adv(1, hashmap_data);

        let (mut incoming, _) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        // State machine fields accessible via accessors
        assert_eq!(incoming.original_hash(), &[0xCC; 32]);
        assert_eq!(incoming.total_segments(), 1);
        assert!(incoming.request_id().is_none());
        assert_eq!(incoming.transfer_size(), 464);
        assert_eq!(incoming.data_size(), 100);

        // cancel() transitions to Failed
        incoming.cancel();
        assert_eq!(incoming.status(), ResourceStatus::Failed);
    }

    #[test]
    fn test_resource_too_large_rejected() {
        let mut adv = make_test_adv(1, vec![0u8; RESOURCE_HASHMAP_LEN]);
        adv.transfer_size = 100_000;
        let result = IncomingResource::from_advertisement(
            &adv,
            431,
            400,
            1000,
            8 * 1024,
            WindowPolicy::Current,
        );
        match result {
            Err(ResourceError::ResourceTooLarge) => {}
            Err(e) => panic!("expected ResourceTooLarge, got {e}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn test_resource_within_limit_accepted() {
        let adv = make_test_adv(1, vec![0u8; RESOURCE_HASHMAP_LEN]);
        // transfer_size = 464, limit = 8KB
        let result = IncomingResource::from_advertisement(
            &adv,
            431,
            400,
            1000,
            8 * 1024,
            WindowPolicy::Current,
        );
        assert!(result.is_ok(), "expected Ok, got Err");
    }

    /// Pins the exact receive-window trajectory of the window adaptation
    /// logic (Codeberg #85). Drives an IncomingResource through 19 rounds
    /// with controlled first-part timing so the measured rate crosses the
    /// SLOW, FAST and VERY_SLOW tiers, and asserts the exact
    /// (window, window_max) value after every completed round. This test
    /// is the behavior-identical proof for the WindowState/WindowPolicy
    /// extraction: it must stay green, unchanged, through the refactor.
    #[test]
    fn test_window_trajectory_pinned() {
        const NUM_PARTS: usize = 100;
        const SDU: usize = 464;
        let random_hash = [0xBB; RESOURCE_RANDOM_HASH_SIZE];

        // Build real parts and a full hashmap so REQs are never exhausted.
        let parts: Vec<Vec<u8>> = (0..NUM_PARTS)
            .map(|i| {
                let mut p = vec![0u8; SDU];
                p[0] = (i & 0xff) as u8;
                p[1] = ((i >> 8) & 0xff) as u8;
                p
            })
            .collect();
        let mut hashmap_data = Vec::new();
        for p in &parts {
            hashmap_data.extend_from_slice(&map_hash(p, &random_hash));
        }

        let mut adv = make_test_adv(NUM_PARTS as u32, hashmap_data);
        adv.transfer_size = (NUM_PARTS * SDU) as u64; // bytes_per_part = 464

        let mut now: u64 = 1000;
        let (mut incoming, first_req) = IncomingResource::from_advertisement(
            &adv,
            431,
            SDU,
            now,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        // First-part delay per round: with bytes_per_part = 464 the measured
        // rate is 464_000 / elapsed_ms. 50 ms -> 9280 B/s (SLOW tier),
        // 5 ms -> 92800 B/s (FAST tier), 600 ms -> 773 B/s (VERY_SLOW tier).
        let first_part_delay_ms: [u64; 19] = [
            50, 50, 50, 50, 50, 50, 50, 50, 50, // rounds 1-9: slow
            5, 5, 5, 5, 5, 5, 5, 5, 5,   // rounds 10-18: fast
            600, // round 19: very slow
        ];

        // Number of part hashes a not-exhausted REQ carries:
        // [1: flag][32: resource_hash][N*4: hashes].
        fn requested_parts(req: &[u8]) -> usize {
            assert_eq!(req[0], HASHMAP_IS_NOT_EXHAUSTED);
            (req.len() - 1 - 32) / RESOURCE_HASHMAP_LEN
        }

        let mut req = first_req;
        let mut next_part = 0;
        let mut trajectory = Vec::new();
        for delay in first_part_delay_ms {
            let count = requested_parts(&req);
            now += delay;
            let mut last = ResourcePartResult::Continue;
            for _ in 0..count {
                last = incoming.receive_part(&parts[next_part], now, 100);
                assert!(
                    !matches!(last, ResourcePartResult::InvalidPart),
                    "part {next_part} must match its hashmap entry"
                );
                next_part += 1;
            }
            trajectory.push((
                incoming.window_state.window(),
                incoming.window_state.window_max(),
            ));
            req = match last {
                ResourcePartResult::SendRequest(r) => r,
                other => panic!("round must complete with a next REQ, got {other:?}"),
            };
        }

        // Hand-computed against the current algorithm: window grows by 1
        // once consecutive_completed_windows reaches window + FLEXIBILITY(4),
        // window_max follows the rate tier of the round's first-part rate,
        // and a lowered window_max clamps the window down.
        let expected = vec![
            (4, 10), // round 1: slow rate keeps window_max at SLOW
            (4, 10),
            (4, 10),
            (4, 10),
            (4, 10),
            (4, 10),
            (4, 10),
            (5, 10), // round 8: 8 completed rounds >= 4+4, window grows
            (5, 10),
            (5, 75), // round 10: fast rate lifts window_max to FAST
            (5, 75),
            (5, 75),
            (5, 75),
            (5, 75),
            (5, 75),
            (5, 75),
            (6, 75), // round 17: 9 completed rounds >= 5+4, window grows
            (6, 75),
            (4, 4), // round 19: very slow rate clamps window to VERY_SLOW max
        ];
        assert_eq!(trajectory, expected, "pinned window trajectory changed");
    }

    /// The round-complete point emits the RESOURCE_RW observability event
    /// (Codeberg #85) with the receiver-side rate the policy reacted to.
    // Log-capture assertion needs the tracing feature; gated so the
    // --no-default-features (tracing-off) build does not compile `tracing::`.
    #[cfg(feature = "tracing")]
    #[test]
    fn test_round_complete_emits_resource_rw_event() {
        const NUM_PARTS: usize = 8;
        const SDU: usize = 464;
        let random_hash = [0xBB; RESOURCE_RANDOM_HASH_SIZE];
        let parts: Vec<Vec<u8>> = (0..NUM_PARTS)
            .map(|i| {
                let mut p = vec![0u8; SDU];
                p[0] = i as u8;
                p
            })
            .collect();
        let mut hashmap_data = Vec::new();
        for p in &parts {
            hashmap_data.extend_from_slice(&map_hash(p, &random_hash));
        }
        let mut adv = make_test_adv(NUM_PARTS as u32, hashmap_data);
        adv.transfer_size = (NUM_PARTS * SDU) as u64;

        let (mut incoming, _req) = IncomingResource::from_advertisement(
            &adv,
            431,
            SDU,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        // Complete the first window (4 parts), first part 50 ms after the REQ.
        let ((), logs) = crate::test_log_capture::with_captured_logs(|| {
            for part in parts.iter().take(4) {
                incoming.receive_part(part, 1050, 100);
            }
        });
        let line = logs
            .lines()
            .find(|l| l.contains("RESOURCE_RW"))
            .expect("round completion must emit a RESOURCE_RW event");
        for key in [
            "rh=aaaaaaaa",
            "round=1",
            "window=4",
            "wmax=10",
            "rate=9280",
            "outst=4",
            "t=50",
        ] {
            assert!(line.contains(key), "RESOURCE_RW missing {key}: {line}");
        }
    }

    #[test]
    fn test_inconsistent_num_parts_rejected() {
        let mut adv = make_test_adv(10_000, vec![0u8; RESOURCE_HASHMAP_LEN]);
        adv.transfer_size = 100; // 100 bytes can't have 10000 parts
        let result = IncomingResource::from_advertisement(
            &adv,
            431,
            400,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        );
        match result {
            Err(ResourceError::InvalidAdvertisement) => {}
            Err(e) => panic!("expected InvalidAdvertisement, got {e}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    /// Codeberg #263 item 2: the part count that sizes `parts` and `hashmap`
    /// must be *derived* from the accepted transfer size, never taken from the
    /// peer's `n` field. The reference does not read `n` at all
    /// (Resource.py:187, `total_parts = ceil(size / sdu)`), so a truthful `t`
    /// with a nonsense `n` still yields the correct part count.
    #[test]
    fn num_parts_is_derived_from_transfer_size_not_from_the_wire_field() {
        let mut adv = make_test_adv(0, vec![0x11, 0x22, 0x33, 0x44]);
        adv.transfer_size = 5 * 464; // five full SDUs

        let (incoming, _) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .expect("a truthful transfer size is accepted");

        assert_eq!(
            incoming.num_parts, 5,
            "part count follows ceil(transfer_size / sdu), not the advertised n"
        );
        assert_eq!(incoming.parts.len(), 5, "parts allocation follows it too");
        assert_eq!(incoming.hashmap.len(), 5, "so does the hashmap allocation");
    }

    /// Codeberg #263 item 2: with the default ceiling in place, an
    /// advertisement claiming a multi-gigabyte transfer is refused before any
    /// allocation. Before `RESOURCE_MAX_INCOMING_SIZE` had a real value it was
    /// `usize::MAX`, so this check never fired for any input.
    #[test]
    fn oversized_transfer_size_rejected_by_the_default_limit() {
        use crate::resource::RESOURCE_MAX_INCOMING_SIZE;

        let mut adv = make_test_adv(1, vec![0x11, 0x22, 0x33, 0x44]);
        adv.transfer_size = u32::MAX as u64; // ~4 GiB claimed by the peer

        let result = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            RESOURCE_MAX_INCOMING_SIZE,
            WindowPolicy::Current,
        );
        match result {
            Err(ResourceError::ResourceTooLarge) => {}
            Err(e) => panic!("expected ResourceTooLarge, got {e}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    /// Codeberg #263 item 3: `data_size` (`d`) is the only advertisement field
    /// that had no validation at all. It is the total across every segment, so
    /// the reference's own segmentation arithmetic bounds it:
    /// `total_segments = ((total_size-1)//MAX_EFFICIENT_SIZE)+1`
    /// (Resource.py:296) means `d <= l * MAX_EFFICIENT_SIZE`.
    #[test]
    fn data_size_beyond_the_advertised_segment_count_rejected() {
        let mut adv = make_test_adv(1, vec![0x11, 0x22, 0x33, 0x44]);
        adv.total_segments = 1;
        adv.data_size = RESOURCE_MAX_EFFICIENT_SIZE as u64 + 1;

        let result = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            crate::resource::RESOURCE_MAX_INCOMING_SIZE,
            WindowPolicy::Current,
        );
        match result {
            Err(ResourceError::InvalidAdvertisement) => {}
            Err(e) => panic!("expected InvalidAdvertisement, got {e}"),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    /// The counterpart: a split transfer legitimately advertises a `d` far
    /// above one segment's worth, and every one of its advertisements must
    /// still be accepted. The bound is per-transfer-total, not per-segment.
    #[test]
    fn data_size_of_a_legitimate_split_transfer_accepted() {
        // Ten segments of a 10 MiB transfer: `d` is the full total in each.
        let mut adv = make_test_adv(1, vec![0x11, 0x22, 0x33, 0x44]);
        adv.total_segments = 10;
        adv.segment_index = 4;
        adv.data_size = 10 * RESOURCE_MAX_EFFICIENT_SIZE as u64;

        IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1000,
            crate::resource::RESOURCE_MAX_INCOMING_SIZE,
            WindowPolicy::Current,
        )
        .expect("a split transfer's total data size is not an oversized resource");
    }

    /// #159 tranche 2: pin the exhausted RESOURCE_REQ we generate as a
    /// receiver. When the window reaches past the known hashmap, the request
    /// must carry `HASHMAP_IS_EXHAUSTED` plus the LAST map hash we know
    /// (Resource.py:966-969: `hashmap[hashmap_height-1]`). The sender scans
    /// for that hash, and the reference CANCELS the whole transfer unless the
    /// resulting part index is an exact multiple of `HASHMAP_MAX_LEN = 74`
    /// (Resource.py:1046-1050) — so after consuming a full advertisement
    /// segment of 74 entries, the hash we report must be entry 73, nothing
    /// else.
    #[test]
    fn exhausted_request_reports_the_last_known_hash_at_a_segment_boundary() {
        use crate::resource::HASHMAP_MAX_LEN;

        // 100-part resource, advertisement carries the first 74 map hashes.
        let mut hashmap_data = Vec::new();
        for i in 0..HASHMAP_MAX_LEN as u8 {
            hashmap_data.extend_from_slice(&[i, i, i, i]);
        }
        let mut adv = make_test_adv(100, hashmap_data);
        adv.transfer_size = 100 * 464;

        let (mut incoming, first_req) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1_000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        // The initial request must NOT claim exhaustion: the window's hashes
        // are all known.
        assert_eq!(first_req[0], HASHMAP_IS_NOT_EXHAUSTED);

        // All 74 known parts received; the next window starts at part 74,
        // whose map hash is unknown.
        for i in 0..HASHMAP_MAX_LEN {
            incoming.parts[i] = Some(vec![0u8; 1]);
        }
        let req = incoming.build_request();

        assert_eq!(req[0], HASHMAP_IS_EXHAUSTED);
        let boundary = HASHMAP_MAX_LEN as u8 - 1;
        assert_eq!(
            &req[1..1 + RESOURCE_HASHMAP_LEN],
            &[boundary, boundary, boundary, boundary],
            "the reported hash must be entry 73: the sender resolves it to \
             part index 74, the only value that passes the reference's \
             `part_index % 74 == 0` sequencing gate"
        );
        assert_eq!(
            &req[1 + RESOURCE_HASHMAP_LEN..1 + RESOURCE_HASHMAP_LEN + 32],
            &adv.resource_hash,
        );
        assert_eq!(
            req.len(),
            1 + RESOURCE_HASHMAP_LEN + 32,
            "no part hashes can be requested past the known hashmap"
        );
        assert!(incoming.waiting_for_hmu);
    }

    /// #159 tranche 2: pin the completion proof we generate as a receiver:
    /// `h + full_hash(assembled_plaintext + h)` (Resource.py:752-758), where
    /// the assembled plaintext still INCLUDES the metadata block (prove()
    /// runs on `self.data` before the metadata strip touches only a local).
    /// The sender compares bytes 32.. against its precomputed
    /// `full_hash(plaintext + h)` and completes the transfer only on
    /// equality (Resource.py:782-786) — the sender-side half of this pin
    /// lives in `outgoing.rs`
    /// (`advertisement_fields_follow_reference_generation_rules`).
    #[test]
    fn completion_proof_follows_reference_formula() {
        use crate::crypto::sha256;

        let adv = make_test_adv(1, vec![0x11, 0x22, 0x33, 0x44]);
        let (mut incoming, _req) = IncomingResource::from_advertisement(
            &adv,
            431,
            464,
            1_000,
            usize::MAX,
            WindowPolicy::Current,
        )
        .unwrap();

        let assembled = b"\x00\x00\x04METAactual data".to_vec();
        incoming.assembled_with_metadata = Some(assembled.clone());

        let proof = incoming.build_proof().unwrap();
        assert_eq!(proof.len(), 64, "proof is h(32) + proof hash(32)");
        assert_eq!(&proof[..32], &adv.resource_hash, "proof must lead with h");
        let mut p_input = assembled;
        p_input.extend_from_slice(&adv.resource_hash);
        assert_eq!(
            &proof[32..],
            &sha256(&p_input),
            "proof hash must equal full_hash(assembled-with-metadata + h)"
        );
    }
}
