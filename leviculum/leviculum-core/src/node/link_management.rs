//! Link management methods for NodeCore
//!
//! This module contains all link-related methods on [`NodeCore`]:
//! connection establishment, acceptance, data transfer, packet routing,
//! and internal link packet processing (handshake, data, timeouts).

use alloc::vec::Vec;

use crate::constants::{
    CHANNEL_DEFAULT_RTT_MS, DATA_RECEIPT_TIMEOUT_MS, LINK_REQUEST_MAX_RETRIES, MODE_AES256_CBC,
    MS_PER_SECOND, PROOF_DATA_SIZE, RTT_RETRY_MAX_ATTEMPTS, TRAFFIC_TIMEOUT_FACTOR,
    TRAFFIC_TIMEOUT_MIN_MS, TRUNCATED_HASHBYTES,
};
use crate::destination::{DestinationHash, ProofStrategy};
use crate::hex_fmt::{HexFmt, HexShort};
use crate::link::channel::{ChannelAction, ChannelError, Message, ReceiveOutcome};
use crate::link::{Link, LinkCloseReason, LinkError, LinkId, LinkPhase, LinkState};
use crate::packet::{packet_hash, Packet, PacketContext, PacketType};
use crate::traits::{Clock, Storage};
use rand_core::CryptoRngCore;

use super::event::NodeEvent;
use super::send;
use super::{LinkStats, NodeCore};

/// State for link establishment retry.
///
/// Stored per link_id in `NodeCore::link_retry_state`. When a link request
/// times out, the retry state determines whether to re-attempt (with fresh
/// ephemeral keys and a new link_id) or emit `LinkClosed::Timeout`.
#[derive(Debug, Clone)]
pub(super) struct LinkRetryState {
    pub remaining: u8,
}

/// Outcome of trying to advance a split outgoing resource transfer after a
/// segment's proof arrives.
enum SegmentAdvance {
    /// The next segment was built and advertised; the transfer continues.
    Sent,
    /// No more segments; the transfer is complete. Carries the transfer's total
    /// segment count for the sender's ResourceCompleted event (1 when the
    /// transfer was never split).
    Done { total_segments: u32 },
    /// Building or advertising the next segment failed, or the link went away
    /// mid-transfer. The transfer did NOT complete: the caller emits
    /// ResourceFailed rather than ResourceCompleted, so an unsent LXMF message
    /// is not marked Delivered (Codeberg #270). The caller supplies the
    /// resource hash from the just-completed segment it still holds; LXMF keys
    /// the failure on the link, not the hash.
    Failed {
        error: crate::resource::ResourceError,
    },
}

/// Simple message type for sending raw bytes over a channel
struct RawBytesMessage<'a>(&'a [u8]);

impl Message for RawBytesMessage<'_> {
    const MSGTYPE: u16 = 0x0000;

    fn pack(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn unpack(data: &[u8]) -> Result<Self, ChannelError> {
        let _ = data;
        Err(ChannelError::EnvelopeTruncated)
    }
}

/// Tracks channel message receipts awaiting delivery proofs.
///
/// Each entry records a sent link packet: its packet hashes, link, optional
/// channel sequence number, and absolute proof deadline. A single
/// `Vec<ReceiptEntry>` replaces the three
/// separate maps that previously encoded this relationship in different
/// directions (`data_receipts`, `channel_receipt_keys`, `channel_hash_to_seq`).
///
/// Every datum exists exactly once. Lookups are linear scans, n is bounded
/// by channel window size × active links (realistically < 100, typically < 20).
pub(super) struct ReceiptTracker {
    entries: Vec<ReceiptEntry>,
}

pub(super) struct ReceiptEntry {
    pub(super) truncated_hash: [u8; TRUNCATED_HASHBYTES],
    pub(super) full_hash: [u8; 32],
    pub(super) link_id: LinkId,
    /// Channel sequence, or `None` for a raw link packet such as LXMF.
    pub(super) sequence: Option<u16>,
    pub(super) deadline_ms: u64,
}

impl ReceiptTracker {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Register a receipt for a channel message, replacing any existing entry
    /// for the same `(link_id, sequence)`.
    pub(super) fn register(
        &mut self,
        packet_data: &[u8],
        link_id: LinkId,
        sequence: u16,
        now_ms: u64,
    ) {
        self.entries
            .retain(|e| !(e.link_id == link_id && e.sequence == Some(sequence)));

        let full_hash = packet_hash(packet_data);
        let mut truncated_hash = [0u8; TRUNCATED_HASHBYTES];
        truncated_hash.copy_from_slice(&full_hash[..TRUNCATED_HASHBYTES]);

        crate::tracing::debug!(
            hash = %HexFmt(&full_hash),
            truncated = %HexFmt(&truncated_hash),
            link = %HexFmt(link_id.as_bytes()),
            "link_mgr: registered channel receipt"
        );

        self.entries.push(ReceiptEntry {
            truncated_hash,
            full_hash,
            link_id,
            sequence: Some(sequence),
            deadline_ms: now_ms.saturating_add(DATA_RECEIPT_TIMEOUT_MS),
        });
    }

    /// Register a proof receipt for a plain (non-Channel) link packet.
    pub(super) fn register_raw(
        &mut self,
        packet_data: &[u8],
        link_id: LinkId,
        deadline_ms: u64,
    ) -> [u8; 32] {
        let full_hash = packet_hash(packet_data);
        let mut truncated_hash = [0u8; TRUNCATED_HASHBYTES];
        truncated_hash.copy_from_slice(&full_hash[..TRUNCATED_HASHBYTES]);
        self.entries.retain(|entry| entry.full_hash != full_hash);
        self.entries.push(ReceiptEntry {
            truncated_hash,
            full_hash,
            link_id,
            sequence: None,
            deadline_ms,
        });
        full_hash
    }

    /// Look up a receipt by truncated hash. Returns `(link_id, full_hash)`.
    pub(super) fn lookup_receipt(
        &self,
        truncated: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<(LinkId, [u8; 32])> {
        self.entries
            .iter()
            .find(|e| e.truncated_hash == *truncated)
            .map(|e| (e.link_id, e.full_hash))
    }

    /// Remove the entry matching `truncated_hash` and return its sequence number.
    pub(super) fn confirm_delivery(
        &mut self,
        truncated: &[u8; TRUNCATED_HASHBYTES],
    ) -> Option<Option<u16>> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.truncated_hash == *truncated)?;
        let entry = self.entries.swap_remove(idx);
        Some(entry.sequence)
    }

    /// Remove all entries for a given link.
    pub(super) fn remove_for_link(&mut self, link_id: &LinkId) {
        self.entries.retain(|e| e.link_id != *link_id);
    }

    /// Remove and return entries whose proof deadline has elapsed.
    pub(super) fn expire(&mut self, now_ms: u64) -> Vec<ReceiptEntry> {
        let mut expired = Vec::new();
        let mut index = 0;
        while index < self.entries.len() {
            if now_ms >= self.entries[index].deadline_ms {
                expired.push(self.entries.swap_remove(index));
            } else {
                index += 1;
            }
        }
        expired
    }

    /// Number of tracked receipts.
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Earliest expiry deadline across all entries, if any.
    pub(super) fn earliest_expiry(&self) -> Option<u64> {
        self.entries.iter().map(|e| e.deadline_ms).min()
    }

    #[cfg(test)]
    pub(crate) fn count_for_link(&self, link_id: &LinkId) -> usize {
        self.entries
            .iter()
            .filter(|e| e.link_id == *link_id)
            .count()
    }

    #[cfg(test)]
    pub(crate) fn contains_truncated(&self, truncated: &[u8; TRUNCATED_HASHBYTES]) -> bool {
        self.entries.iter().any(|e| e.truncated_hash == *truncated)
    }
}

impl<R: CryptoRngCore, C: Clock, S: Storage> NodeCore<R, C, S> {
    // Link Management (Public API)
    /// Initiate a link to a destination
    ///
    /// # Arguments
    /// * `dest_hash` - The destination to connect to
    /// * `dest_signing_key` - The destination's signing key (from announce)
    ///
    /// # Returns
    /// A tuple of `(LinkId, was_routed, TickOutput)`:
    /// - `was_routed`: `true` if the link request was sent via a known path,
    ///   `false` if it was broadcast (no path existed).
    pub fn connect(
        &mut self,
        dest_hash: DestinationHash,
        dest_signing_key: &[u8; 32],
    ) -> (LinkId, bool, crate::transport::TickOutput) {
        // Check if we have path info for this destination
        let (next_hop, hops) = if let Some(path) = self.transport.path(dest_hash.as_bytes()) {
            if path.needs_relay() {
                (path.next_hop, path.hops)
            } else {
                (None, path.hops)
            }
        } else {
            (None, 1)
        };

        let now_ms = self.transport.clock().now_ms();

        // Look up HW_MTU for next-hop interface (for link MTU negotiation)
        let hw_mtu = self
            .transport
            .next_hop_interface_hw_mtu(dest_hash.as_bytes());

        // Create new outgoing link
        let mut link = Link::new_outgoing(dest_hash, &mut self.rng);
        link.set_keepalive_override(self.transport_config().link_keepalive_secs);
        let packet = link.build_link_request_packet_with_transport(next_hop, hops, hw_mtu);
        link.set_hops(hops);

        // Scale establishment timeout for slow first-hop interfaces (LoRa, etc.)
        //
        // When the next-hop bitrate is known (e.g., directly on LoRa), use it.
        // When unknown and hops > 1 (path traverses relay nodes whose interfaces
        // we can't see), use a conservative estimate. This covers clients that
        // connect via TCP to a transport daemon which routes through LoRa, the
        // client sees TCP (no bitrate) but the actual path includes slow links.
        // Python handles this via an RPC call to `get_first_hop_timeout()`.
        if let Some(bitrate) = self
            .transport
            .next_hop_interface_bitrate(dest_hash.as_bytes())
        {
            link.set_first_hop_timeout_from_bitrate(bitrate);
        } else if hops > 1 {
            link.set_first_hop_timeout_from_bitrate(
                crate::constants::UNKNOWN_BITRATE_ASSUMPTION_BPS,
            );
        }
        let link_id = *link.id();
        if let Err(e) = link.set_destination_keys(dest_signing_key) {
            crate::tracing::debug!(%e, "set_destination_keys failed");
        }
        link.set_phase(LinkPhase::PendingOutgoing {
            created_at_ms: now_ms,
        });
        self.links.insert(link_id, link);

        // Register retry state so check_timeouts() can re-attempt on failure.
        // Retry count is hop-aware: multi-hop paths need more attempts because
        // each hop is an independent loss opportunity. A 3-hop path has 3×
        // the chance of losing the request or proof compared to 1-hop.
        let retries = core::cmp::max(LINK_REQUEST_MAX_RETRIES, hops);
        self.link_retry_state
            .insert(link_id, LinkRetryState { remaining: retries });

        // Register link_id as a local destination so that the returning
        // LRPROOF (and subsequent data packets) are delivered to us.
        // Without this, handle_proof() silently drops the proof on
        // non-transport nodes that connect via TCP.
        // Removed when the link is cleaned up (close, timeout, reject).
        self.transport.register_destination(*link_id.as_bytes());

        // Route through transport: path lookup first, broadcast if no path
        let was_routed = self
            .transport
            .send_to_destination(dest_hash.as_bytes(), &packet)
            .is_ok();
        if !was_routed {
            self.transport.send_on_all_interfaces(&packet);
        }

        // Establishment packet-level instrumentation (initiator TX). Pairs with
        // LINK_REQUEST_RX / LINK_PROOF_TX / LINK_PROOF_RX so a hardware run can
        // pinpoint which handshake packet is lost on a cold lossy path.
        crate::tracing::debug!(
            target: "leviculum_core::link",
            "LINK_REQUEST_TX link={} dest={} t_ms={} hops={} routed={}",
            HexShort(link_id.as_bytes()),
            HexShort(dest_hash.as_bytes()),
            now_ms,
            hops,
            was_routed,
        );

        let output = self.process_events_and_actions();
        (link_id, was_routed, output)
    }

    /// Build and route the link establishment proof for an incoming link.
    ///
    /// Reads the destination identity + proof strategy, builds the proof packet
    /// (`MODE_AES256_CBC`, unchanged wire bytes), caches it, moves the link to
    /// `PendingIncoming`, and routes the proof on the link's attached interface.
    /// Called by the auto-accept path in [`Self::handle_link_request`] when the
    /// destination accepts links.
    fn send_establishment_proof(&mut self, link_id: &LinkId) -> Result<(), LinkError> {
        let dest_hash = self
            .links
            .get(link_id)
            .map(|l| *l.destination_hash())
            .ok_or(LinkError::NotFound)?;

        let dest = self
            .destinations
            .get(&dest_hash)
            .ok_or(LinkError::DestinationNotRegistered)?;
        let identity = dest.identity().ok_or(LinkError::DestinationNotRegistered)?;
        let proof_strategy = dest.proof_strategy();

        let now_ms = self.transport.clock().now_ms();

        let link = self.links.get_mut(link_id).ok_or(LinkError::NotFound)?;
        link.set_proof_strategy(proof_strategy);
        if let Some(sk) = identity.ed25519_signing_key() {
            link.set_dest_signing_key(sk.clone());
        }
        let proof_mtu = link.negotiated_mtu();
        let proof = link.build_proof_packet(identity, proof_mtu, MODE_AES256_CBC)?;
        link.set_cached_proof(proof.clone());
        link.set_phase(LinkPhase::PendingIncoming {
            proof_sent_at_ms: now_ms,
        });

        // OBS-3 (Codeberg #114): endpoint proof generation. The link
        // establishment proof is the endpoint's proof path; it never reaches
        // Transport::send_proof (that is only the single-packet delivery proof),
        // so without this the responder proved links invisibly. `for_pkt` is the
        // link id (= truncated hash of the link request being proved), matching
        // the single-packet PROOF_GEN semantics; `to_dst` is the destination the
        // link is for.
        crate::tracing::debug!(
            event = "PROOF_GEN",
            for_pkt = %HexShort(link_id.as_bytes()),
            to_dst = %HexShort(dest_hash.as_bytes()),
        );

        // Establishment packet-level instrumentation (responder proof TX).
        crate::tracing::debug!(
            target: "leviculum_core::link",
            "LINK_PROOF_TX link={} dest={} t_ms={} proof_len={}",
            HexShort(link_id.as_bytes()),
            HexShort(dest_hash.as_bytes()),
            now_ms,
            proof.len(),
        );

        // Route proof on attached interface (matching Python Link.prove())
        let attached = self.links.get(link_id).and_then(|l| l.attached_interface());
        debug_assert!(
            attached.is_some(),
            "send_establishment_proof: link {:?} has no attached_interface",
            link_id
        );
        if let Some(iface_idx) = attached {
            // OBS-3 (Codeberg #114): endpoint proof send. `pkt` is the link id
            // (the proof packet's target), matching PROOF_SEND semantics.
            crate::tracing::debug!(
                event = "PROOF_SEND",
                pkt = %HexShort(link_id.as_bytes()),
                iface = %self.transport.iface_name(iface_idx),
            );
            if let Err(e) = self.transport.send_on_interface(iface_idx, &proof) {
                crate::tracing::debug!(%e, "send_on_interface failed");
            }
        } else {
            self.transport.send_on_all_interfaces(&proof);
        }

        Ok(())
    }

    /// Remove a link and unregister its ID from local destinations.
    ///
    /// Every `self.links.insert(link_id, ..)` is paired with a
    /// `self.transport.register_destination(link_id)`. This helper
    /// ensures the reverse cleanup always happens together.
    fn remove_link(&mut self, link_id: &LinkId) {
        // Fail any in-flight resource transfers before dropping the link, so
        // the app waiting on the transfer is notified instead of hanging
        // (mirrors RNS Link.link_closed() cancelling both incoming and
        // outgoing resources). Every genuine teardown removes its link through
        // here; the establishment re-key/alias path (Codeberg #66) uses
        // self.links.remove directly and never reaches this helper, so it
        // emits no spurious ResourceFailed. Pre-activation teardowns (reject,
        // invalid proof, handshake timeout) hold no resource, so this is a
        // no-op there.
        let (outgoing_hash, incoming_hash) = match self.links.get(link_id) {
            Some(link) => (
                link.outgoing_resource().map(|r| *r.resource_hash()),
                link.incoming_resource().map(|r| *r.resource_hash()),
            ),
            None => (None, None),
        };
        if outgoing_hash.is_some() || incoming_hash.is_some() {
            // Report the caller-visible id, consistent with emit_link_closed
            // (which translates re-keyed wire ids back to the original).
            let event_id = self
                .link_origin_ids
                .get(link_id)
                .copied()
                .unwrap_or(*link_id);
            if let Some(resource_hash) = outgoing_hash {
                self.events.push(NodeEvent::ResourceFailed {
                    link_id: event_id,
                    resource_hash,
                    error: crate::resource::ResourceError::LinkClosed,
                    is_sender: true,
                });
            }
            if let Some(resource_hash) = incoming_hash {
                self.events.push(NodeEvent::ResourceFailed {
                    link_id: event_id,
                    resource_hash,
                    error: crate::resource::ResourceError::LinkClosed,
                    is_sender: false,
                });
            }
        }
        self.links.remove(link_id);
        self.link_retry_state.remove(link_id);
        self.transport.unregister_destination(link_id.as_bytes());
        // Drop caller-visible aliases that pointed at this link
        // (Codeberg #66 retry rekeying). The origin-id entry is NOT
        // dropped here: emit_link_closed consumes it after removal
        // (every close path emits per its invariant).
        self.link_id_aliases.retain(|_, target| target != link_id);
    }

    /// Resolve a possibly-stale caller-visible link id to the current
    /// wire id. Establishment retries regenerate the ephemeral keys and
    /// re-key the link (Codeberg #66); ids handed out before a retry
    /// resolve through `link_id_aliases`.
    pub(crate) fn resolve_link_id(&self, link_id: &LinkId) -> LinkId {
        self.link_id_aliases
            .get(link_id)
            .copied()
            .unwrap_or(*link_id)
    }

    /// Reject an incoming link request
    pub fn reject_link(&mut self, link_id: &LinkId) {
        self.remove_link(link_id);
    }

    /// Close a link gracefully
    pub fn close_link(&mut self, link_id: &LinkId) -> crate::transport::TickOutput {
        let link_id = &self.resolve_link_id(link_id);
        if let Some(link) = self.links.get_mut(link_id) {
            let is_initiator = link.is_initiator();
            let destination_hash = *link.destination_hash();
            // Build and route close packet BEFORE removing the link
            if let Ok(close_packet) = link.build_close_packet(&mut self.rng) {
                link.close();
                // Route via attached interface while link is still in map
                self.route_link_packet(link_id, &close_packet);
            } else {
                link.close();
            }
            self.remove_link(link_id);
            self.emit_link_closed(
                *link_id,
                LinkCloseReason::Normal,
                is_initiator,
                destination_hash,
            );
        }
        self.process_events_and_actions()
    }

    /// Get a link by ID
    pub fn link(&self, link_id: &LinkId) -> Option<&crate::link::Link> {
        self.links.get(&self.resolve_link_id(link_id))
    }

    /// Get a mutable reference to a link by ID
    pub fn link_mut(&mut self, link_id: &LinkId) -> Option<&mut crate::link::Link> {
        let resolved = self.resolve_link_id(link_id);
        self.links.get_mut(&resolved)
    }

    /// Get the number of active links
    pub fn active_link_count(&self) -> usize {
        self.links
            .values()
            .filter(|l| l.state() == LinkState::Active)
            .count()
    }

    /// Get the number of pending links
    pub fn pending_link_count(&self) -> usize {
        self.links
            .values()
            .filter(|l| !matches!(l.phase(), LinkPhase::Established))
            .count()
    }

    /// Return all local links as RPC-exportable rows.
    ///
    /// One entry per [`crate::link::Link`] currently in the `links` map,
    /// regardless of state — the `state` field on the returned struct
    /// disambiguates `pending` / `handshake` / `active` / `stale` / `closed`.
    /// Backs the `link_table` shared-instance RPC (`lnstest diag` v2).
    pub fn link_table_entries(&self) -> alloc::vec::Vec<crate::transport::LinkTableExport> {
        let now_secs = self.transport.clock().now_ms() / MS_PER_SECOND;
        self.links
            .values()
            .map(|link| {
                let state = match link.state() {
                    LinkState::Pending => "pending",
                    LinkState::Handshake => "handshake",
                    LinkState::Active => "active",
                    LinkState::Stale => "stale",
                    LinkState::Closed => "closed",
                };
                let age_secs = link
                    .established_at_secs()
                    .map(|est| now_secs.saturating_sub(est));
                crate::transport::LinkTableExport {
                    link_id: *link.id().as_bytes(),
                    state,
                    destination_hash: *link.destination_hash().as_bytes(),
                    age_secs,
                    interface_index: link.attached_interface(),
                }
            })
            .collect()
    }

    // Link Data Transfer
    /// Send data on an existing link via Channel (reliable, ordered)
    pub fn send_on_link(
        &mut self,
        link_id: &LinkId,
        data: &[u8],
    ) -> Result<crate::transport::TickOutput, send::SendError> {
        let link_id = &self.resolve_link_id(link_id);
        let link = self.links.get(link_id).ok_or(send::SendError::NoLink)?;

        let attached_iface = link.attached_interface();
        let dest_hash = *link.destination_hash();

        // Check interface readiness before building/encrypting the packet.
        // Only applies to app-originated sends; event-loop-internal sends
        // (retransmits, proofs, keepalives) go through route_link_packet()
        // and use the driver retry queue instead.
        //
        // Use the next-slot backchannel: a future slot becomes
        // SendError::PacingDelay { ready_at_ms } so stream.rs can
        // sleep_until rather than polling.
        let now_ms = self.transport.clock().now_ms();
        if let Some(iface_idx) = attached_iface {
            let next_slot = self.transport.next_slot_ms_for_interface(iface_idx, now_ms);
            if next_slot > now_ms {
                // Telemetry (leviculum#35): the interface gate is backpressure
                // a capacity estimator must distinguish from app-limited idle.
                if let Some(link) = self.links.get_mut(link_id) {
                    link.note_iface_pacing_rejection();
                }
                return Err(send::SendError::PacingDelay {
                    ready_at_ms: next_slot,
                });
            }
        }

        let link = self.links.get_mut(link_id).ok_or(send::SendError::NoLink)?;
        if link.state() != LinkState::Active {
            return Err(send::SendError::LinkFailed);
        }
        let link_mdu = link.mdu();
        let rtt_ms = link.rtt_ms();
        let envelope_data = {
            let channel = link.ensure_channel(rtt_ms);
            channel
                .send(&RawBytesMessage(data), link_mdu, now_ms, rtt_ms)
                .map_err(|e| match e {
                    ChannelError::Busy => send::SendError::Busy,
                    ChannelError::PacingDelay { ready_at_ms } => {
                        send::SendError::PacingDelay { ready_at_ms }
                    }
                    _ => send::SendError::LinkFailed,
                })?
        };
        let packet_bytes = link
            .build_data_packet_with_context(&envelope_data, PacketContext::Channel, &mut self.rng)
            .map_err(|_| send::SendError::LinkFailed)?;

        // Register receipt for channel delivery tracking (Python Channel.py:606)
        if let Some(seq) = self
            .links
            .get(link_id)
            .and_then(|l| l.channel())
            .map(|ch| ch.last_sent_sequence())
        {
            self.receipt_tracker
                .register(&packet_bytes, *link_id, seq, now_ms);
        }

        // Route via attached interface
        if let Some(iface_idx) = attached_iface {
            if let Err(e) = self.transport.send_on_interface(iface_idx, &packet_bytes) {
                crate::tracing::debug!(%e, "send_on_interface failed");
            }
        } else if let Err(e) = self
            .transport
            .send_to_destination(dest_hash.as_bytes(), &packet_bytes)
        {
            crate::tracing::warn!(%e, "send_to_destination failed — packet lost");
        }

        Ok(self.process_events_and_actions())
    }

    /// Send a plain data packet on an existing link.
    ///
    /// This mirrors Python `RNS.Packet(link, data)`: the payload is encrypted
    /// for the link and sent with `PacketContext::None`, without Channel
    /// framing. A raw receipt is retained so a PROVE_ALL response emits
    /// `LinkDeliveryConfirmed`, matching Python's packet receipt callback.
    pub fn send_packet_on_link(
        &mut self,
        link_id: &LinkId,
        data: &[u8],
    ) -> Result<([u8; 32], crate::transport::TickOutput), send::SendError> {
        let link_id = &self.resolve_link_id(link_id);
        let link = self.links.get(link_id).ok_or(send::SendError::NoLink)?;

        if link.state() != LinkState::Active {
            return Err(send::SendError::LinkFailed);
        }
        if data.len() > link.mdu() {
            return Err(send::SendError::TooLarge);
        }

        let packet_bytes = link
            .build_data_packet(data, &mut self.rng)
            .map_err(|e| match e {
                LinkError::InvalidState => send::SendError::LinkFailed,
                _ => send::SendError::LinkFailed,
            })?;

        let now_ms = self.transport.clock().now_ms();
        // Python PacketReceipt derives Link packet timeouts from the measured
        // RTT (Packet.py:430-431). Raw packets have no Channel retransmission
        // state, so this is their actual proof-acceptance deadline.
        let receipt_timeout_ms = link
            .rtt_ms()
            .saturating_mul(TRAFFIC_TIMEOUT_FACTOR)
            .max(TRAFFIC_TIMEOUT_MIN_MS);
        let deadline_ms = now_ms.saturating_add(receipt_timeout_ms);
        let packet_hash = self
            .receipt_tracker
            .register_raw(&packet_bytes, *link_id, deadline_ms);
        self.route_link_packet(link_id, &packet_bytes);

        Ok((packet_hash, self.process_events_and_actions()))
    }

    /// Send a proof for an application-accepted plain link data packet.
    pub fn send_data_proof(
        &mut self,
        link_id: &LinkId,
        packet_hash: &[u8; 32],
    ) -> Result<crate::transport::TickOutput, LinkError> {
        let link_id = self.resolve_link_id(link_id);
        let link = self.links.get(&link_id).ok_or(LinkError::NotFound)?;
        let signing_key = link
            .proof_signing_key()
            .ok_or(LinkError::NoIdentity)?
            .clone();
        let proof_packet =
            link.build_data_proof_packet_with_signing_key(packet_hash, &signing_key)?;

        self.route_link_packet(&link_id, &proof_packet);

        Ok(self.process_events_and_actions())
    }

    /// Find an existing active link to a destination
    #[cfg(test)]
    pub(crate) fn find_link_to(&self, dest_hash: &DestinationHash) -> Option<LinkId> {
        self.links
            .iter()
            .find(|(_, link)| {
                link.state() == LinkState::Active && link.destination_hash() == dest_hash
            })
            .map(|(id, _)| *id)
    }

    /// Get link statistics
    pub fn link_stats(&self, link_id: &LinkId) -> Option<LinkStats> {
        let link_id = &self.resolve_link_id(link_id);
        let link = self.links.get(link_id)?;
        let ch = link.channel();
        Some(LinkStats {
            tx_ring_size: ch.map(|c| c.outstanding()).unwrap_or(0),
            window: ch.map(|c| c.window()).unwrap_or(0),
            window_max: ch.map(|c| c.window_max()).unwrap_or(0),
            pacing_interval_ms: ch.map(|c| c.pacing_interval_ms()).unwrap_or(0),
            // — leviculum#35 delivery telemetry —
            bytes_delivered: ch
                .map(|c| c.delivered_bytes())
                .unwrap_or(0)
                .saturating_add(link.resource_bytes_delivered()),
            srtt_ms: ch.map(|c| c.srtt_ms()).filter(|s| *s > 0.0),
            rttvar_ms: ch.filter(|c| c.srtt_ms() > 0.0).map(|c| c.rttvar_ms()),
            min_rtt_ms: ch.and_then(|c| c.min_rtt_ms()),
            rtt_ms: link.rtt_us().map(|us| us / 1000),
            busy_rejections: ch.map(|c| c.busy_rejections()).unwrap_or(0),
            pacing_rejections: ch.map(|c| c.pacing_rejections()).unwrap_or(0),
            iface_pacing_rejections: link.iface_pacing_rejections(),
        })
    }

    // Internal: Link Packet Processing
    /// Process an incoming link-related packet
    pub(super) fn process_link_packet(
        &mut self,
        packet: &Packet,
        raw_packet: &[u8],
        now_ms: u64,
        interface_index: usize,
    ) {
        match packet.flags.packet_type {
            PacketType::LinkRequest => {
                self.handle_link_request(packet, raw_packet, interface_index);
            }
            PacketType::Proof => {
                self.handle_link_proof(packet, now_ms, interface_index);
            }
            PacketType::Data => {
                self.handle_link_data(packet, raw_packet, now_ms);
            }
            PacketType::Announce => {
                // Announces are not link packets, ignore
            }
        }
    }

    /// Handle an incoming LINK_REQUEST packet
    fn handle_link_request(&mut self, packet: &Packet, raw_packet: &[u8], interface_index: usize) {
        let dest_hash = DestinationHash::new(packet.destination_hash);

        // Check if we accept links for this destination (consolidated with self.destinations)
        let accepts = self
            .destinations
            .get(&dest_hash)
            .map(|d| d.accepts_links())
            .unwrap_or(false);
        if !accepts {
            crate::tracing::trace!(
                dest = %HexShort(dest_hash.as_bytes()),
                "Link request ignored, destination does not accept links"
            );
            return;
        }

        // Calculate link ID from raw packet
        let link_id = Link::calculate_link_id(raw_packet);

        // Check if we already have this link
        if let Some(link) = self.links.get(&link_id) {
            // Link-request retry scenario: if the link is still in PendingIncoming,
            // the original proof was likely lost. Re-send the cached proof.
            if matches!(link.phase(), LinkPhase::PendingIncoming { .. }) {
                if let Some(proof) = link.cached_proof() {
                    let proof = proof.to_vec();
                    if let Some(iface_idx) = link.attached_interface() {
                        crate::tracing::debug!(
                            link = %HexShort(link_id.as_bytes()),
                            "Re-sending proof for duplicate link request"
                        );
                        if let Err(e) = self.transport.send_on_interface(iface_idx, &proof) {
                            crate::tracing::debug!(%e, "proof re-send failed");
                        }
                    } else {
                        crate::tracing::warn!(
                            link = %HexShort(link_id.as_bytes()),
                            "Cannot re-send proof: no attached interface"
                        );
                    }
                }
            } else {
                crate::tracing::trace!(
                    link = %HexShort(link_id.as_bytes()),
                    "Link request ignored, link already exists"
                );
            }
            return;
        }

        // Extract request data from packet payload
        let request_data = packet.data.as_slice();

        // Create the incoming link
        let iface_hw_mtu = self.transport.interface_hw_mtu(interface_index);
        let Ok(mut link) = Link::new_incoming(
            request_data,
            link_id,
            dest_hash,
            &mut self.rng,
            iface_hw_mtu,
        ) else {
            crate::tracing::warn!(
                link = %HexShort(link_id.as_bytes()),
                "Dropped malformed link request, failed to parse"
            );
            return;
        };

        // Set attached interface from the receiving interface
        link.set_attached_interface(interface_index);
        link.set_keepalive_override(self.transport_config().link_keepalive_secs);

        // Copy the packet's hop count so establishment_timeout_ms() scales correctly
        link.set_hops(packet.hops);

        // Store the link
        self.links.insert(link_id, link);

        // Register link_id as a local destination so that data packets
        // addressed to this link are delivered to us.
        // Removed when the link is cleaned up (close, timeout, reject).
        self.transport.register_destination(*link_id.as_bytes());

        crate::tracing::debug!(
            "Incoming link request <{}> for <{}> accepted on {}",
            HexShort(link_id.as_bytes()),
            HexShort(dest_hash.as_bytes()),
            self.transport.iface_name(interface_index)
        );

        // OBS-3 (Codeberg #114): endpoint link acceptance. Distinct from the
        // relay-side LINK_ENTRY_SET (transport.rs), which fires only when we
        // forward a link request for someone else. This marks that WE are the
        // endpoint terminating the link, so an endpoint node is observable.
        crate::tracing::debug!(
            event = "LINK_LOCAL",
            link = %HexShort(link_id.as_bytes()),
            dst = %HexShort(dest_hash.as_bytes()),
            iface = %self.transport.iface_name(interface_index),
        );

        // Establishment packet-level instrumentation (responder request RX).
        crate::tracing::debug!(
            target: "leviculum_core::link",
            "LINK_REQUEST_RX link={} dest={} t_ms={} hops={} iface={}",
            HexShort(link_id.as_bytes()),
            HexShort(dest_hash.as_bytes()),
            self.transport.clock().now_ms(),
            packet.hops,
            self.transport.iface_name(interface_index),
        );

        // Auto-accept (Python parity): a fresh Destination accepts links
        // (`accepts` was checked above), so prove the link immediately. The
        // proof bytes are identical (MODE_AES256_CBC via send_establishment_proof).
        // The link surfaces to consumers via NodeEvent::LinkEstablished once the
        // handshake completes; there is no separate request event or accept step.
        if let Err(e) = self.send_establishment_proof(&link_id) {
            crate::tracing::debug!(
                %e,
                link = %HexShort(link_id.as_bytes()),
                "auto-accept: establishment proof send failed"
            );
        }
    }

    /// Handle an incoming PROOF packet (link establishment or data proof)
    fn handle_link_proof(&mut self, packet: &Packet, now_ms: u64, interface_index: usize) {
        let link_id = LinkId::new(packet.destination_hash);
        let proof_data = packet.data.as_slice();

        crate::tracing::debug!(
            "handle_link_proof: link=<{}> context={:?} proof_len={} iface={}",
            HexShort(link_id.as_bytes()),
            packet.context,
            proof_data.len(),
            interface_index,
        );

        // Check if this is a data proof (not a link establishment proof)
        if proof_data.len() == PROOF_DATA_SIZE && packet.context == PacketContext::None {
            self.handle_data_proof(&link_id, proof_data);
            return;
        }

        // Establishment packet-level instrumentation (initiator proof RX).
        crate::tracing::debug!(
            target: "leviculum_core::link",
            "LINK_PROOF_RX link={} t_ms={} proof_len={} iface={}",
            HexShort(link_id.as_bytes()),
            now_ms,
            proof_data.len(),
            interface_index,
        );

        if packet.context == PacketContext::ResourcePrf {
            self.handle_resource_proof(link_id, proof_data, now_ms);
            return;
        }

        let Some(link) = self.links.get_mut(&link_id) else {
            crate::tracing::debug!(
                "handle_link_proof: link <{}> not found in self.links ({} links tracked)",
                HexShort(link_id.as_bytes()),
                self.links.len(),
            );
            return;
        };

        // Must be a pending outgoing link
        if !matches!(link.phase(), LinkPhase::PendingOutgoing { .. }) {
            crate::tracing::debug!(
                "handle_link_proof: link <{}> phase={:?}, expected PendingOutgoing",
                HexShort(link_id.as_bytes()),
                link.phase(),
            );
            return;
        }
        if link.state() != LinkState::Pending || !link.is_initiator() {
            crate::tracing::debug!(
                "handle_link_proof: link <{}> state={:?} initiator={}, expected Pending+initiator",
                HexShort(link_id.as_bytes()),
                link.state(),
                link.is_initiator(),
            );
            return;
        }

        // Set attached interface from the interface the proof arrived on
        link.set_attached_interface(interface_index);

        // Process the proof
        if link.process_proof(proof_data).is_err() {
            crate::tracing::debug!(
                "handle_link_proof: link <{}> proof verification failed, closing",
                HexShort(link_id.as_bytes())
            );
            let is_initiator = link.is_initiator();
            let destination_hash = *link.destination_hash();
            self.remove_link(&link_id);
            self.emit_link_closed(
                link_id,
                LinkCloseReason::InvalidProof,
                is_initiator,
                destination_hash,
            );
            return;
        }

        // Proof verified! Calculate RTT and transition to established
        let now_secs = now_ms / MS_PER_SECOND;
        let measured_rtt_ms = match link.phase() {
            LinkPhase::PendingOutgoing { created_at_ms } => now_ms.saturating_sub(created_at_ms),
            _ => 0,
        };

        // Store measured RTT on initiator so RTT retry packets carry the
        // correct value instead of defaulting to 0.0.
        if measured_rtt_ms > 0 {
            link.set_rtt_ms(measured_rtt_ms);
        }

        link.set_phase(LinkPhase::Established);

        // Link established, no more retries needed.
        self.link_retry_state.remove(&link_id);

        // Keepalive timing uses the actual measurement (even 0 for localhost).
        let rtt_seconds_actual = measured_rtt_ms as f64 / MS_PER_SECOND as f64;
        link.update_keepalive_from_rtt(rtt_seconds_actual);
        link.mark_established(now_secs);

        crate::tracing::debug!(
            "Link <{}> established with RTT {}ms",
            HexShort(link_id.as_bytes()),
            measured_rtt_ms
        );
        crate::tracing::debug!(
            target: "leviculum_core::link",
            "LINK_ESTAB link={} rtt_ms={} keepalive_s={} stale_s={} inactivity_timeout_ms={} hops={} initiator=true",
            HexShort(link_id.as_bytes()),
            link.rtt_ms(),
            link.keepalive_secs(),
            link.stale_time_secs(),
            link.stale_close_timeout_secs().saturating_mul(MS_PER_SECOND),
            link.hops(),
        );

        // For the RTT packet, use at least the default to avoid sending
        // 0.0 which would corrupt the responder's channel window tier and
        // timeout math (FAST tier + 100ms timeout on LoRa = retransmit storm).
        let rtt_for_wire_ms = measured_rtt_ms.max(CHANNEL_DEFAULT_RTT_MS);
        let rtt_seconds_wire = rtt_for_wire_ms as f64 / MS_PER_SECOND as f64;

        // Build RTT packet and route via attached interface
        match link.build_rtt_packet(rtt_seconds_wire, &mut self.rng) {
            Ok(rtt_packet) => {
                link.record_rtt_sent(now_ms);
                self.events.push(NodeEvent::LinkEstablished {
                    link_id,
                    is_initiator: true,
                    destination_hash: *link.destination_hash(),
                });
                self.route_link_packet(&link_id, &rtt_packet);
            }
            Err(e) => {
                crate::tracing::warn!(
                    "handle_link_proof: link <{}> established but build_rtt_packet failed: {:?}",
                    HexShort(link_id.as_bytes()),
                    e,
                );
                // Emit the event anyway, the link IS established even if the
                // RTT packet could not be sent.
                self.events.push(NodeEvent::LinkEstablished {
                    link_id,
                    is_initiator: true,
                    destination_hash: *link.destination_hash(),
                });
            }
        }
    }

    /// Handle a data proof packet (PROVE_ALL response)
    fn handle_data_proof(&mut self, link_id: &LinkId, proof_data: &[u8]) {
        if proof_data.len() != PROOF_DATA_SIZE {
            crate::tracing::debug!(
                len = proof_data.len(),
                expected = PROOF_DATA_SIZE,
                "link_mgr: data proof wrong size"
            );
            return;
        }
        let proof_hash: [u8; 32] = match proof_data[..32].try_into() {
            Ok(h) => h,
            Err(_) => return,
        };

        let mut truncated = [0u8; TRUNCATED_HASHBYTES];
        truncated.copy_from_slice(&proof_hash[..TRUNCATED_HASHBYTES]);
        crate::tracing::debug!(
            proof_hash = %HexFmt(&proof_hash),
            truncated = %HexFmt(&truncated),
            link = %HexFmt(link_id.as_bytes()),
            receipts = self.receipt_tracker.len(),
            "link_mgr: data proof received"
        );

        // Look up receipt
        let Some((receipt_link_id, receipt_full_hash)) =
            self.receipt_tracker.lookup_receipt(&truncated)
        else {
            crate::tracing::debug!(
                receipts = self.receipt_tracker.len(),
                "link_mgr: data proof — no matching receipt"
            );
            return;
        };

        if receipt_link_id != *link_id {
            crate::tracing::debug!("link_mgr: data proof — link_id mismatch");
            return;
        }

        // Validate the proof using the link
        let is_valid = self
            .links
            .get(link_id)
            .map(|l| l.validate_data_proof(proof_data, &receipt_full_hash))
            .unwrap_or(false);

        if is_valid {
            // A validated inbound proof is link activity: refresh the activity
            // clock and recover a Stale link so a flow whose only inbound is
            // proofs (e.g. this node streaming to a peer that proves every
            // packet) is not marked Stale and closed mid-transfer (#124).
            // Python parity: Packet.py:463 sets last_proof; Link.py:789's stale
            // check uses max(last_inbound, last_proof, activated_at).
            let now_secs = self.transport.clock().now_ms() / MS_PER_SECOND;
            self.try_recover_stale(link_id, now_secs);
            if let Some(link) = self.links.get_mut(link_id) {
                link.record_inbound(now_secs);
            }

            self.try_confirm_rtt(link_id);

            // confirm_delivery removes the entry entirely (fixes orphan path #1).
            // Event only fires when the receipt still exists, a valid proof for
            // an already-expired/removed receipt is silently dropped.
            if let Some(sequence) = self.receipt_tracker.confirm_delivery(&truncated) {
                crate::tracing::debug!(
                    receipts = self.receipt_tracker.len(),
                    "link_mgr: data proof validated — delivered"
                );

                let now_ms = self.transport.clock().now_ms();
                let rtt_ms = self.links.get(link_id).map(|l| l.rtt_ms()).unwrap_or(500);
                if let Some(sequence) = sequence {
                    if let Some(link) = self.links.get_mut(link_id) {
                        if let Some(ch) = link.channel_mut() {
                            ch.mark_delivered(sequence, now_ms, rtt_ms);
                        }
                    }
                }

                self.events.push(NodeEvent::LinkDeliveryConfirmed {
                    link_id: *link_id,
                    packet_hash: receipt_full_hash,
                });
            }
        } else {
            crate::tracing::debug!("link_mgr: data proof — signature validation failed");
        }
    }

    /// Handle an incoming DATA packet on a link.
    ///
    /// Dispatches to per-context handlers after stale-link recovery.
    fn handle_link_data(&mut self, packet: &Packet, raw_packet: &[u8], now_ms: u64) {
        let link_id = LinkId::new(packet.destination_hash);
        let now_secs = now_ms / MS_PER_SECOND;

        // Recover stale links on any inbound traffic (Python Link.py:987-988)
        self.try_recover_stale(&link_id, now_secs);

        if !self.links.contains_key(&link_id) {
            crate::tracing::trace!(
                link = %HexShort(link_id.as_bytes()),
                "Link data packet for unknown link, ignoring"
            );
            return;
        }

        self.try_confirm_rtt(&link_id);

        // Any non-keepalive inbound link packet resets the inactivity watchdog
        // (the per-context handlers below call record_inbound). Keepalive echoes
        // are logged separately as kind=keepalive_resp; handshake RTT and close
        // packets are not link activity.
        if !matches!(
            packet.context,
            PacketContext::Keepalive | PacketContext::Lrrtt | PacketContext::LinkClose
        ) {
            crate::tracing::debug!(
                target: "leviculum_core::link",
                "LINK_ACTIVITY link={} kind=data t_ms={} ctx={:?}",
                HexShort(link_id.as_bytes()),
                now_ms,
                packet.context,
            );
        }

        match packet.context {
            PacketContext::Lrrtt => self.handle_rtt_packet(link_id, packet, now_secs),
            PacketContext::Keepalive => self.handle_keepalive_packet(link_id, packet, now_secs),
            PacketContext::LinkClose => self.handle_close_packet(link_id, packet),
            PacketContext::Channel => {
                self.handle_channel_packet(link_id, packet, raw_packet, now_ms, now_secs)
            }
            PacketContext::ResourceAdv => {
                self.handle_resource_adv(link_id, packet, raw_packet, now_ms, now_secs);
            }
            PacketContext::ResourceReq => {
                self.handle_resource_req(link_id, packet, now_ms, now_secs);
            }
            PacketContext::Resource => {
                self.handle_resource_data(link_id, packet, now_ms, now_secs);
            }
            PacketContext::ResourceHmu => {
                self.handle_resource_hmu(link_id, packet, raw_packet, now_ms, now_secs);
            }
            PacketContext::ResourceIcl => {
                self.handle_resource_cancel(link_id, true);
            }
            PacketContext::ResourceRcl => {
                self.handle_resource_cancel(link_id, false);
            }
            PacketContext::LinkIdentify => {
                self.handle_link_identify(link_id, packet, now_secs);
            }
            PacketContext::Request => {
                self.handle_request_packet(link_id, packet, raw_packet, now_ms, now_secs);
            }
            PacketContext::Response => {
                self.handle_response_packet(link_id, packet, raw_packet, now_secs);
            }
            PacketContext::CacheRequest => {
                self.handle_cache_request(link_id, packet, now_secs);
            }
            // A context byte we assign no meaning to must NOT fall into the
            // plain-data arm below: that arm means "this is link payload for
            // the application", which is a guess we have no basis for (#332).
            // NodeCore's delivery gate normally catches this before the packet
            // reaches here; the arm is explicit so a future caller of this
            // handler cannot silently reintroduce the guess. Python ends up in
            // the same place by falling off the `elif packet.context == ...`
            // chain in Link.receive (Link.py:972-1100).
            PacketContext::Unknown(byte) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    ctx = byte,
                    "Link packet with unrecognised context byte, not interpreted"
                );
            }
            PacketContext::None
            | PacketContext::ResourcePrf
            | PacketContext::PathResponse
            | PacketContext::Command
            | PacketContext::CommandStatus
            | PacketContext::LinkProof
            | PacketContext::Lrproof => {
                self.handle_plain_data_packet(link_id, packet, raw_packet, now_secs)
            }
        }
    }

    /// RTT packet (context = Lrrtt), responder side
    fn handle_rtt_packet(&mut self, link_id: LinkId, packet: &Packet, now_secs: u64) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        if link.state() != LinkState::Handshake || link.is_initiator() {
            return;
        }
        let encrypted_data = packet.data.as_slice();
        if let Ok(rtt_secs) = link.process_rtt(encrypted_data) {
            link.update_keepalive_from_rtt(rtt_secs);
            link.mark_established(now_secs);
            link.set_phase(LinkPhase::Established);
            crate::tracing::debug!(
                "Link <{}> established (responder), RTT {:.3}s",
                HexShort(link_id.as_bytes()),
                rtt_secs
            );
            crate::tracing::debug!(
                target: "leviculum_core::link",
                "LINK_ESTAB link={} rtt_ms={} keepalive_s={} stale_s={} inactivity_timeout_ms={} hops={} initiator=false",
                HexShort(link_id.as_bytes()),
                link.rtt_ms(),
                link.keepalive_secs(),
                link.stale_time_secs(),
                link.stale_close_timeout_secs().saturating_mul(MS_PER_SECOND),
                link.hops(),
            );
            self.events.push(NodeEvent::LinkEstablished {
                link_id,
                is_initiator: false,
                destination_hash: *link.destination_hash(),
            });
        }
    }

    /// Keepalive packet, record activity and echo if requested
    fn handle_keepalive_packet(&mut self, link_id: LinkId, packet: &Packet, now_secs: u64) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        link.record_inbound(now_secs);
        let data = packet.data.as_slice();
        if let Ok(should_echo) = link.process_keepalive(data) {
            // Initiator side: a valid responder echo (should_echo == false) reset
            // the watchdog. process_keepalive() already bumped keepalives_acked.
            if !should_echo {
                crate::tracing::debug!(
                    target: "leviculum_core::link",
                    "LINK_ACTIVITY link={} kind=keepalive_resp t_ms={} keepalives_sent={} keepalives_acked={}",
                    HexShort(link_id.as_bytes()),
                    now_secs.saturating_mul(MS_PER_SECOND),
                    link.keepalives_sent(),
                    link.keepalives_acked(),
                );
            }
            if should_echo {
                if let Ok(echo_packet) = link.build_keepalive_packet() {
                    self.route_link_packet(&link_id, &echo_packet);
                }
            }
        }
    }

    /// Link close packet, verify and tear down the link
    fn handle_close_packet(&mut self, link_id: LinkId, packet: &Packet) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        let encrypted_data = packet.data.as_slice();
        if link.process_close(encrypted_data).is_ok() {
            crate::tracing::debug!("Link <{}> closed by peer", HexShort(link_id.as_bytes()));
            let is_initiator = link.is_initiator();
            let destination_hash = *link.destination_hash();
            self.remove_link(&link_id);
            self.emit_link_closed(
                link_id,
                LinkCloseReason::PeerClosed,
                is_initiator,
                destination_hash,
            );
        }
    }

    /// Channel packet, decrypt, process through channel, build proof
    fn handle_channel_packet(
        &mut self,
        link_id: LinkId,
        packet: &Packet,
        raw_packet: &[u8],
        now_ms: u64,
        now_secs: u64,
    ) {
        let Some(link) = self.links.get_mut(&link_id) else {
            crate::tracing::trace!(
                link = %HexShort(link_id.as_bytes()),
                "Channel packet for unknown link, ignoring"
            );
            return;
        };
        if link.state() != LinkState::Active {
            crate::tracing::trace!(
                link = %HexShort(link_id.as_bytes()),
                "Channel packet for non-active link, ignoring"
            );
            return;
        }

        link.record_inbound(now_secs);

        // 1. Decrypt the envelope data
        let encrypted_data = packet.data.as_slice();
        let max_plaintext_len = encrypted_data.len();
        let mut plaintext = alloc::vec![0u8; max_plaintext_len];
        let decrypted_len = match link.decrypt(encrypted_data, &mut plaintext) {
            Ok(len) => len,
            Err(_) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    "Channel packet decryption failed"
                );
                return;
            }
        };
        plaintext.truncate(decrypted_len);

        // 2. Channel receive + drain + proof, link borrow scoped in this block
        // so that route_link_packet (which needs &mut self) can run afterward.
        //
        // Deferred proof strategy: only prove messages that are delivered in-order
        // to the application. Out-of-order buffered messages store their proof hash
        // and get proved when drained. This prevents the sender from clearing its
        // retransmit queue for undelivered messages during head-of-line blocking.
        let mut rx_ring_full = false;
        let proof_packets: Vec<Vec<u8>> = {
            let rtt_ms = link.rtt_ms();
            let channel = link.ensure_channel(rtt_ms);
            let full_packet_hash = packet_hash(raw_packet);

            let mut proofs = Vec::new();

            match channel.receive(&plaintext, full_packet_hash) {
                Ok(ReceiveOutcome::Delivered(envelope)) => {
                    crate::tracing::debug!(
                        seq = envelope.sequence,
                        msgtype = envelope.msgtype,
                        len = envelope.data.len(),
                        "link_mgr: channel message received (in-order)"
                    );
                    self.events.push(NodeEvent::MessageReceived {
                        link_id,
                        msgtype: envelope.msgtype,
                        sequence: envelope.sequence,
                        data: envelope.data,
                    });
                    if let Some(p) =
                        Self::build_channel_proof_from_hash(link, &link_id, &full_packet_hash)
                    {
                        proofs.push(p);
                    }
                }
                Ok(ReceiveOutcome::AlreadyDelivered) => {
                    // Duplicate of delivered msg: re-prove so sender clears retransmit
                    if let Some(p) =
                        Self::build_channel_proof_from_hash(link, &link_id, &full_packet_hash)
                    {
                        proofs.push(p);
                    }
                }
                Ok(ReceiveOutcome::Buffered) => {
                    crate::tracing::debug!("link_mgr: channel message buffered (out-of-order)");
                    // No proof, sender will retransmit the gap-filling packet
                }
                Ok(ReceiveOutcome::DuplicateBuffered) => {
                    // Duplicate of buffered msg, no proof
                }
                Err(ChannelError::RxRingFull) => {
                    rx_ring_full = true;
                }
                Err(e) => {
                    crate::tracing::debug!(?e, "link_mgr: channel receive failed");
                }
            };

            // Drain consecutive buffered messages now ready for delivery
            let drained: Vec<_> = link
                .channel_mut()
                .map(|ch| ch.drain_received())
                .unwrap_or_default();
            for (envelope, stored_hash) in drained {
                self.events.push(NodeEvent::MessageReceived {
                    link_id,
                    msgtype: envelope.msgtype,
                    sequence: envelope.sequence,
                    data: envelope.data,
                });
                if let Some(p) = Self::build_channel_proof_from_hash(link, &link_id, &stored_hash) {
                    proofs.push(p);
                }
            }

            proofs
        };

        // 3. Post-processing (link borrow released)
        if rx_ring_full {
            self.update_rx_ring_full(now_ms);
        }
        for proof in proof_packets {
            self.route_link_packet(&link_id, &proof);
        }
    }

    /// Build a data proof from a pre-computed packet hash.
    ///
    /// Used for deferred proofs on buffered out-of-order messages where we
    /// stored only the 32-byte hash, not the full encrypted packet.
    fn build_channel_proof_from_hash(
        link: &Link,
        link_id: &LinkId,
        full_packet_hash: &[u8; 32],
    ) -> Option<Vec<u8>> {
        if let Some(signing_key) = link.proof_signing_key() {
            match link.build_data_proof_packet_with_signing_key(full_packet_hash, signing_key) {
                Ok(proof) => {
                    crate::tracing::debug!(
                        hash = %HexFmt(full_packet_hash),
                        link = %HexFmt(link_id.as_bytes()),
                        proof_len = proof.len(),
                        "link_mgr: channel proof generated"
                    );
                    Some(proof)
                }
                Err(e) => {
                    crate::tracing::warn!(
                        hash = %HexFmt(full_packet_hash),
                        link = %HexFmt(link_id.as_bytes()),
                        error = %e,
                        "link_mgr: channel proof build failed"
                    );
                    None
                }
            }
        } else {
            crate::tracing::warn!(
                link = %HexFmt(link_id.as_bytes()),
                "link_mgr: channel proof skipped — no signing key"
            );
            None
        }
    }

    /// Update rx_ring overflow counter with rate-limited warning.
    fn update_rx_ring_full(&mut self, now_ms: u64) {
        self.rx_ring_full_count += 1;
        let elapsed = now_ms.saturating_sub(self.rx_ring_full_last_log_ms);
        if self.rx_ring_full_last_log_ms == 0 || elapsed >= 5000 {
            crate::tracing::warn!(
                "link_mgr: channel rx_ring full — {} messages dropped in last {}s (cap={})",
                self.rx_ring_full_count,
                elapsed / 1000,
                crate::constants::CHANNEL_RX_RING_MAX
            );
            self.rx_ring_full_count = 0;
            self.rx_ring_full_last_log_ms = now_ms;
        }
    }

    /// Regular data packet, decrypt, handle proof strategy, emit data event
    fn handle_plain_data_packet(
        &mut self,
        link_id: LinkId,
        packet: &Packet,
        raw_packet: &[u8],
        now_secs: u64,
    ) {
        let Some(link) = self.links.get_mut(&link_id) else {
            crate::tracing::trace!(
                link = %HexShort(link_id.as_bytes()),
                "Data packet for unknown link, ignoring"
            );
            return;
        };
        if link.state() != LinkState::Active {
            crate::tracing::trace!(
                link = %HexShort(link_id.as_bytes()),
                "Data packet for non-active link, ignoring"
            );
            return;
        }

        link.record_inbound(now_secs);

        let encrypted_data = packet.data.as_slice();
        let max_plaintext_len = encrypted_data.len();
        let mut plaintext = alloc::vec![0u8; max_plaintext_len];

        match link.decrypt(encrypted_data, &mut plaintext) {
            Ok(len) => {
                plaintext.truncate(len);

                let full_packet_hash = packet_hash(raw_packet);

                match link.proof_strategy() {
                    ProofStrategy::All => {
                        if let Some(signing_key) = link.proof_signing_key() {
                            if let Ok(proof_packet) = link.build_data_proof_packet_with_signing_key(
                                &full_packet_hash,
                                signing_key,
                            ) {
                                self.route_link_packet(&link_id, &proof_packet);
                            }
                        }
                    }
                    ProofStrategy::App => {
                        self.events.push(NodeEvent::LinkProofRequested {
                            link_id,
                            packet_hash: full_packet_hash,
                        });
                    }
                    ProofStrategy::None => {}
                }

                self.events.push(NodeEvent::LinkDataReceived {
                    link_id,
                    data: plaintext,
                });
            }
            Err(_) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    "Plain data packet decryption failed"
                );
            }
        }
    }

    /// Handle a LINKIDENTIFY packet, peer is proving their identity.
    ///
    /// Protocol: plaintext = public_key(64) + signature(64) = 128 bytes.
    /// signed_data = link_id(16) + public_key(64) = 80 bytes.
    /// Only accepted on responder side (non-initiator). Matches Python Link.py:1014-1032.
    fn handle_link_identify(&mut self, link_id: LinkId, packet: &Packet, now_secs: u64) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };

        // Only responders accept identify (Python: `if not self.initiator`)
        if link.is_initiator() {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                "LinkIdentify rejected: we are initiator"
            );
            return;
        }

        if link.state() != LinkState::Active {
            return;
        }

        link.record_inbound(now_secs);

        // Decrypt
        let encrypted_data = packet.data.as_slice();
        let max_plaintext_len = encrypted_data.len();
        let mut plaintext = alloc::vec![0u8; max_plaintext_len];

        let plaintext_len = match link.decrypt(encrypted_data, &mut plaintext) {
            Ok(len) => len,
            Err(_) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    "LinkIdentify decryption failed"
                );
                return;
            }
        };

        // Validate length: exactly 128 bytes (64 public_key + 64 signature)
        if plaintext_len != 128 {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                len = plaintext_len,
                "LinkIdentify rejected: wrong plaintext length"
            );
            return;
        }

        // Extract public_key and signature
        let public_key = &plaintext[..64];
        let signature = &plaintext[64..128];

        // Reconstruct signed_data = link_id(16) + public_key(64)
        let mut signed_data = [0u8; 80];
        signed_data[..16].copy_from_slice(link_id.as_bytes());
        signed_data[16..80].copy_from_slice(public_key);

        // Create identity from public key bytes
        let identity = match crate::identity::Identity::from_public_key_bytes(public_key) {
            Ok(id) => id,
            Err(e) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    %e,
                    "LinkIdentify rejected: invalid public key"
                );
                return;
            }
        };

        // Validate signature
        match identity.verify(&signed_data, signature) {
            Ok(true) => {}
            Ok(false) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    "LinkIdentify rejected: invalid signature"
                );
                return;
            }
            Err(e) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    %e,
                    "LinkIdentify rejected: verification error"
                );
                return;
            }
        }

        let identity_hash = *identity.hash();

        // Codeberg #88: terminate incoming links from blackholed identities.
        // Python runs this check right after the identify signature validates
        // (Link.py:1021-1023) and calls teardown(), which sends a LINKCLOSE
        // to the peer (Link.py:704). The identity is NOT stored and no
        // identified event is emitted.
        if self.transport.is_blackholed(&identity_hash) {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                identity = %HexShort(&identity_hash),
                "Terminating incoming link from blackholed identity"
            );
            if let Some(link) = self.links.get_mut(&link_id) {
                let is_initiator = link.is_initiator();
                let destination_hash = *link.destination_hash();
                if let Ok(close_packet) = link.build_close_packet(&mut self.rng) {
                    link.close();
                    self.route_link_packet(&link_id, &close_packet);
                } else {
                    link.close();
                }
                self.remove_link(&link_id);
                self.emit_link_closed(
                    link_id,
                    LinkCloseReason::Blackholed,
                    is_initiator,
                    destination_hash,
                );
            }
            return;
        }

        // Store identity on link and emit event
        crate::tracing::debug!(
            link = %HexShort(link_id.as_bytes()),
            identity = %HexShort(&identity_hash),
            "Link peer identified"
        );

        if let Some(link) = self.links.get_mut(&link_id) {
            link.set_remote_identity(identity);
        }

        self.events.push(NodeEvent::LinkIdentified {
            link_id,
            identity_hash,
        });
    }

    // Internal: Request/Response Handlers
    /// Handle an incoming request packet (responder side).
    ///
    /// Protocol: plaintext = msgpack fixarray(3) [timestamp, path_hash, data].
    /// Matches Python Link.py:1036 request_handler().
    fn handle_request_packet(
        &mut self,
        link_id: LinkId,
        packet: &Packet,
        raw_packet: &[u8],
        _now_ms: u64, // passed by dispatch_link_data_packet uniformly; unused here
        now_secs: u64,
    ) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };

        if link.state() != LinkState::Active {
            return;
        }
        link.record_inbound(now_secs);

        // Decrypt
        let encrypted_data = packet.data.as_slice();
        let max_plaintext_len = encrypted_data.len();
        let mut plaintext = alloc::vec![0u8; max_plaintext_len];
        let plaintext_len = match link.decrypt(encrypted_data, &mut plaintext) {
            Ok(len) => len,
            Err(_) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    "Request packet decryption failed"
                );
                return;
            }
        };
        let plaintext = &plaintext[..plaintext_len];

        // Compute request_id = truncated_packet_hash(raw_packet)
        let request_id = crate::packet::truncated_packet_hash(raw_packet);

        self.handle_request_payload(link_id, request_id, plaintext);
    }

    /// Parse, authorize, and dispatch a decrypted request payload.
    ///
    /// Both ordinary Request packets and request Resources use this path. The
    /// caller supplies the request ID because packet requests derive it from
    /// the packet hash while Resource requests derive it from the packed
    /// plaintext hash (matching Python `Link.request()`).
    fn handle_request_payload(
        &mut self,
        link_id: LinkId,
        request_id: [u8; crate::constants::TRUNCATED_HASHBYTES],
        plaintext: &[u8],
    ) {
        use crate::resource::msgpack::{
            read_fixarray_len, read_float64, read_msgpack_bin, read_msgpack_raw_value,
        };

        // Parse msgpack
        let mut pos = 0;
        let Some(3) = read_fixarray_len(plaintext, &mut pos) else {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                "Request: invalid array header"
            );
            return;
        };
        let Some(requested_at) = read_float64(plaintext, &mut pos) else {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                "Request: invalid timestamp"
            );
            return;
        };
        let Some(path_hash_bytes) = read_msgpack_bin(plaintext, &mut pos) else {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                "Request: invalid path_hash"
            );
            return;
        };
        if path_hash_bytes.len() != crate::constants::TRUNCATED_HASHBYTES {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                len = path_hash_bytes.len(),
                "Request: path_hash wrong length"
            );
            return;
        }
        let mut path_hash = [0u8; crate::constants::TRUNCATED_HASHBYTES];
        path_hash.copy_from_slice(path_hash_bytes);

        let Some(data_raw) = read_msgpack_raw_value(plaintext, &mut pos) else {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                "Request: invalid data value"
            );
            return;
        };

        // Convert nil to empty, otherwise preserve raw bytes
        let data = if data_raw == [0xc0] {
            alloc::vec::Vec::new()
        } else {
            data_raw.to_vec()
        };

        // Look up handler keyed by (destination, path).
        let link = self.links.get(&link_id).expect("link checked above");
        let destination_hash = *link.destination_hash();
        let Some(handler) = self.request_handlers.get(&(destination_hash, path_hash)) else {
            crate::tracing::trace!(
                link = %HexShort(link_id.as_bytes()),
                path_hash = %HexShort(&path_hash),
                "Request: no handler for (destination, path_hash), dropping"
            );
            return;
        };

        // Authorization check
        let path = handler.path.clone();
        match &handler.policy {
            super::request::RequestPolicy::AllowNone => {
                crate::tracing::trace!(
                    link = %HexShort(link_id.as_bytes()),
                    "Request: AllowNone policy, dropping"
                );
                return;
            }
            super::request::RequestPolicy::AllowAll => {
                // Allowed
            }
            super::request::RequestPolicy::AllowList(list) => {
                let allowed = link
                    .remote_identity()
                    .map(|id| list.contains(id.hash()))
                    .unwrap_or(false);
                if !allowed {
                    crate::tracing::debug!(
                        link = %HexShort(link_id.as_bytes()),
                        "Request: identity not in allow list, dropping"
                    );
                    return;
                }
            }
        }

        // OBS-3 (Codeberg #114): endpoint request dispatch (e.g. the
        // remote-management `/status` responder). Fires only after the auth
        // policy passes, so it marks a request the endpoint actually answers.
        // `path` is deliberately omitted (it is a name-type string, #113); the
        // stable `path_hash` identifies the handler.
        crate::tracing::debug!(
            event = "REQUEST_RX",
            link = %HexShort(link_id.as_bytes()),
            path_hash = %HexShort(&path_hash),
            request_id = %HexShort(&request_id),
        );

        self.events.push(NodeEvent::RequestReceived {
            link_id,
            destination_hash,
            request_id,
            path,
            path_hash,
            data,
            requested_at,
        });
    }

    /// Handle an incoming response packet (initiator side).
    ///
    /// Protocol: plaintext = msgpack fixarray(2) [request_id, response_data].
    fn handle_response_packet(
        &mut self,
        link_id: LinkId,
        packet: &Packet,
        raw_packet: &[u8],
        now_secs: u64,
    ) {
        let _ = raw_packet; // request_id is inside the payload, not derived from packet hash

        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };

        if link.state() != LinkState::Active {
            return;
        }
        link.record_inbound(now_secs);

        // Decrypt
        let encrypted_data = packet.data.as_slice();
        let max_plaintext_len = encrypted_data.len();
        let mut plaintext = alloc::vec![0u8; max_plaintext_len];
        let plaintext_len = match link.decrypt(encrypted_data, &mut plaintext) {
            Ok(len) => len,
            Err(_) => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    "Response packet decryption failed"
                );
                return;
            }
        };
        let plaintext = &plaintext[..plaintext_len];

        self.handle_response_payload(link_id, plaintext);
    }

    /// Parse and correlate a decrypted response payload.
    ///
    /// Shared by ordinary Response packets and response Resources. Responses
    /// are accepted only on the link where the request is pending.
    pub(super) fn handle_response_payload(&mut self, link_id: LinkId, plaintext: &[u8]) {
        use crate::resource::msgpack::{
            read_fixarray_len, read_msgpack_bin, read_msgpack_raw_value,
        };

        // Parse msgpack
        let mut pos = 0;
        let Some(2) = read_fixarray_len(plaintext, &mut pos) else {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                "Response: invalid array header"
            );
            return;
        };
        let Some(request_id_bytes) = read_msgpack_bin(plaintext, &mut pos) else {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                "Response: invalid request_id"
            );
            return;
        };
        if request_id_bytes.len() != crate::constants::TRUNCATED_HASHBYTES {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                len = request_id_bytes.len(),
                "Response: request_id wrong length"
            );
            return;
        }
        let mut request_id = [0u8; crate::constants::TRUNCATED_HASHBYTES];
        request_id.copy_from_slice(request_id_bytes);

        let Some(response_data) = read_msgpack_raw_value(plaintext, &mut pos) else {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                "Response: invalid response data"
            );
            return;
        };

        // Correlate the response before removing the pending request. A valid
        // ID arriving on another link must not consume the real request.
        if self
            .pending_requests
            .get(&request_id)
            .is_none_or(|request| request.link_id != link_id)
        {
            crate::tracing::trace!(
                link = %HexShort(link_id.as_bytes()),
                request_id = %HexShort(&request_id),
                "Response: no pending request found, ignoring"
            );
            return;
        }
        self.remove_pending_request(&request_id);

        self.events.push(NodeEvent::ResponseReceived {
            link_id,
            request_id,
            response_data: response_data.to_vec(),
            metadata: None,
        });
    }

    /// Parse a wrapped request response payload `[request_id, response_data]`
    /// (msgpack fixarray(2)): the framing both `send_response` (single packet)
    /// and `send_response_resource` (`> MDU` resource) produce. Returns the
    /// 16-byte `request_id` and the raw response value. Used by the response-
    /// resource completion path to mirror Python `response_resource_concluded`
    /// unpacking `unpacked_response[0]` / `unpacked_response[1]`.
    fn parse_wrapped_response(
        plaintext: &[u8],
    ) -> Option<([u8; crate::constants::TRUNCATED_HASHBYTES], Vec<u8>)> {
        use crate::resource::msgpack::{
            read_fixarray_len, read_msgpack_bin, read_msgpack_raw_value,
        };
        let mut pos = 0;
        if read_fixarray_len(plaintext, &mut pos)? != 2 {
            return None;
        }
        let request_id_bytes = read_msgpack_bin(plaintext, &mut pos)?;
        let request_id: [u8; crate::constants::TRUNCATED_HASHBYTES] =
            request_id_bytes.try_into().ok()?;
        let response_data = read_msgpack_raw_value(plaintext, &mut pos)?;
        Some((request_id, response_data.to_vec()))
    }

    /// Handle a CacheRequest packet: re-send a cached resource proof if we have it.
    fn handle_cache_request(&mut self, link_id: LinkId, packet: &Packet, now_secs: u64) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        link.record_inbound(now_secs);

        let data = packet.data.as_slice();
        if data.len() != 32 {
            crate::tracing::debug!(
                link = %HexShort(link_id.as_bytes()),
                len = data.len(),
                "CacheRequest with invalid length"
            );
            return;
        }
        let mut requested_hash = [0u8; 32];
        requested_hash.copy_from_slice(data);

        let proof_bytes = match link.get_cached_resource_proof(&requested_hash) {
            Some(bytes) => bytes.clone(),
            None => {
                crate::tracing::debug!(
                    link = %HexShort(link_id.as_bytes()),
                    hash = %HexShort(&requested_hash),
                    "CacheRequest for unknown hash"
                );
                return;
            }
        };

        link.record_outbound(now_secs);
        crate::tracing::debug!(
            link = %HexShort(link_id.as_bytes()),
            "Re-sending cached resource proof"
        );
        self.route_link_packet(&link_id, &proof_bytes);
    }

    /// Reset the pending-request deadline for every pending request
    /// on `link_id` to `now_ms`. Bug #26 fix: the rtt-derived timeout
    /// set at `send_request` (mod.rs) would otherwise fire while a
    /// resource transfer is still actively delivering chunks, because
    /// the Response packet queues behind the resource data on slow
    /// links (LoRa airtime). Any observed inbound traffic on the same
    /// link counts as peer progress — reset the timer so it reflects
    /// "is the peer still answering?" rather than "how long will the
    /// transfer take?".
    pub(super) fn reset_pending_requests_on_link(&mut self, link_id: &LinkId, now_ms: u64) {
        for pr in self.pending_requests.values_mut() {
            if pr.link_id == *link_id {
                if let Some(response_started_at_ms) = &mut pr.response_started_at_ms {
                    *response_started_at_ms = now_ms;
                }
            }
        }
    }

    /// Check for request timeouts and emit RequestTimedOut events.
    pub(super) fn check_request_timeouts(&mut self, now_ms: u64) {
        let expired: Vec<[u8; crate::constants::TRUNCATED_HASHBYTES]> = self
            .pending_requests
            .iter()
            .filter(|(_, pr)| {
                pr.response_started_at_ms
                    .is_some_and(|started| started.saturating_add(pr.timeout_ms) < now_ms)
            })
            .map(|(id, _)| *id)
            .collect();

        for request_id in expired {
            if let Some(pr) = self.remove_pending_request(&request_id) {
                self.events.push(NodeEvent::RequestTimedOut {
                    link_id: pr.link_id,
                    request_id: pr.request_id,
                });
            }
        }
    }

    /// Apply Resource outcomes to correlated outgoing requests.
    ///
    /// Request-body uploads and response-Resource downloads are phases where
    /// the Resource engine owns liveness. A transfer failure therefore fails
    /// the semantic request immediately instead of waiting for its ordinary
    /// response timeout.
    pub(super) fn reconcile_request_resource_outcomes(&mut self) {
        // Only inspect events that existed on entry. A failed transfer appends
        // a RequestTimedOut below, which is deliberately not another Resource
        // outcome. Indexing avoids a temporary allocation on constrained nodes.
        let outcome_count = self.events.len();
        let now_ms = self.transport.clock().now_ms();
        for index in 0..outcome_count {
            let outcome = match self.events.get(index) {
                Some(NodeEvent::ResourceCompleted {
                    link_id,
                    resource_hash,
                    is_sender,
                    ..
                }) => Some((*link_id, *resource_hash, *is_sender, true)),
                Some(NodeEvent::ResourceFailed {
                    link_id,
                    resource_hash,
                    is_sender,
                    ..
                }) => Some((*link_id, *resource_hash, *is_sender, false)),
                _ => None,
            };
            let Some((link_id, resource_hash, is_sender, completed)) = outcome else {
                continue;
            };
            let request_id = if is_sender {
                self.request_resource_uploads.get(&resource_hash).copied()
            } else {
                self.response_resource_downloads
                    .get(&resource_hash)
                    .copied()
            };
            let Some(request_id) = request_id else {
                continue;
            };
            let matches_link = self
                .pending_requests
                .get(&request_id)
                .is_some_and(|pending| pending.link_id == link_id);
            if !matches_link {
                continue;
            }

            if is_sender {
                self.request_resource_uploads.remove(&resource_hash);
            } else {
                self.response_resource_downloads.remove(&resource_hash);
            }
            if completed {
                // A normal response-Resource completion is decoded by
                // handle_response_payload before this reconciliation pass and
                // has already removed the pending request. If decoding did not
                // produce a response, re-arm the bounded semantic timeout.
                // A peer may advertise its response before the sender-side
                // proof for a request Resource is processed. In that race the
                // accepted response download remains the active phase, so its
                // Resource watchdog must continue to own liveness.
                let response_in_progress = self
                    .response_resource_downloads
                    .values()
                    .any(|mapped_request| *mapped_request == request_id);
                if !is_sender || !response_in_progress {
                    if let Some(pending) = self.pending_requests.get_mut(&request_id) {
                        pending.response_started_at_ms = Some(now_ms);
                    }
                }
            } else if let Some(pending) = self.remove_pending_request(&request_id) {
                // RequestTimedOut is the existing request-failure notification
                // used by sans-I/O clients. The preceding ResourceFailed event
                // retains the precise transport failure reason.
                self.events.push(NodeEvent::RequestTimedOut {
                    link_id: pending.link_id,
                    request_id: pending.request_id,
                });
            }
        }
    }

    fn remove_pending_request(
        &mut self,
        request_id: &[u8; crate::constants::TRUNCATED_HASHBYTES],
    ) -> Option<super::request::PendingRequest> {
        let pending = self.pending_requests.remove(request_id);
        if pending.is_some() {
            self.request_resource_uploads
                .retain(|_, mapped_request| mapped_request != request_id);
            self.response_resource_downloads
                .retain(|_, mapped_request| mapped_request != request_id);
        }
        pending
    }

    fn remove_pending_requests_for_link(&mut self, wire_id: LinkId, visible_id: LinkId) {
        self.pending_requests
            .retain(|_, pending| pending.link_id != wire_id && pending.link_id != visible_id);
        let pending_requests = &self.pending_requests;
        self.request_resource_uploads
            .retain(|_, request_id| pending_requests.contains_key(request_id));
        self.response_resource_downloads
            .retain(|_, request_id| pending_requests.contains_key(request_id));
    }

    /// If the link is Stale and we receive any valid packet, recover to Active.
    /// Matches Python Link.py:987-988.
    fn try_recover_stale(&mut self, link_id: &LinkId, now_secs: u64) -> bool {
        let Some(link) = self.links.get_mut(link_id) else {
            return false;
        };
        if link.state() == LinkState::Stale {
            link.set_state(LinkState::Active);
            link.record_inbound(now_secs);
            self.events
                .push(NodeEvent::LinkRecovered { link_id: *link_id });
            crate::tracing::debug!(link = %HexFmt(link_id.as_bytes()), "link_mgr: recovered from stale");
            true
        } else {
            false
        }
    }

    // Internal: Resource Handlers
    /// Handle a ResourceAdv packet (incoming advertisement).
    fn handle_resource_adv(
        &mut self,
        link_id: LinkId,
        packet: &Packet,
        _raw_packet: &[u8],
        now_ms: u64,
        now_secs: u64,
    ) {
        use crate::resource::{
            incoming::IncomingResource, ResourceAdvertisement, ResourceStrategy,
        };

        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        if link.state() != LinkState::Active {
            return;
        }
        link.record_inbound(now_secs);

        // Already have an incoming resource or pending ADV, ignore
        if link.has_incoming_resource() || link.has_pending_resource() {
            crate::tracing::debug!("Resource ADV on link with active resource, ignoring");
            return;
        }

        // Decrypt the advertisement
        let encrypted_data = packet.data.as_slice();
        let mut plaintext = alloc::vec![0u8; encrypted_data.len()];
        let len = match link.decrypt(encrypted_data, &mut plaintext) {
            Ok(len) => len,
            Err(_) => {
                crate::tracing::debug!("Resource ADV decryption failed");
                return;
            }
        };
        plaintext.truncate(len);

        // Parse advertisement
        let adv = match ResourceAdvertisement::unpack(&plaintext) {
            Ok(adv) => adv,
            Err(_) => {
                crate::tracing::debug!("Resource ADV parse failed");
                return;
            }
        };

        // Python can split large request and response Resources, but the
        // request-correlation path here currently represents one transfer.
        // Reject non-canonical multi-segment forms explicitly instead of
        // accepting bytes that cannot be reassembled with Python semantics.
        if (adv.flags.is_request || adv.flags.is_response)
            && (adv.flags.split || adv.segment_index != 1 || adv.total_segments != 1)
        {
            crate::tracing::debug!(
                segment = adv.segment_index,
                total = adv.total_segments,
                "Split request/response Resources are unsupported"
            );
            return;
        }

        // Request and response Resources are Link protocol internals, not
        // application Resources. Python accepts request Resources regardless
        // of `resource_strategy`, and accepts response Resources only when
        // their advertised request ID belongs to a pending request on this
        // exact link.
        let is_request = adv.request_id.is_some() && adv.flags.is_request;
        let is_response = adv.request_id.is_some() && !is_request && adv.flags.is_response;
        let response_request_id = if is_response {
            adv.request_id.as_deref().and_then(|bytes| {
                <[u8; crate::constants::TRUNCATED_HASHBYTES]>::try_from(bytes).ok()
            })
        } else {
            None
        };
        let strategy = if is_request {
            ResourceStrategy::AcceptAll
        } else if is_response {
            let matching_pending_request = response_request_id
                .and_then(|request_id| self.pending_requests.get(&request_id))
                .is_some_and(|request| request.link_id == link_id);

            if !matching_pending_request {
                crate::tracing::trace!(
                    link = %HexShort(link_id.as_bytes()),
                    "Response Resource has no matching pending request, ignoring"
                );
                return;
            }
            ResourceStrategy::AcceptAll
        } else {
            link.resource_strategy()
        };
        let resource_hash = adv.resource_hash;
        let transfer_size = adv.transfer_size;
        let data_size = adv.data_size;
        let link_mdu = link.mdu();
        let sdu = crate::resource::resource_sdu(link.negotiated_mtu());
        let mut accepted_response_request = None;

        match strategy {
            ResourceStrategy::AcceptAll => {
                // Request Resources and responses matching a pending request
                // bypass the application Resource strategy, matching Python.
                match IncomingResource::from_advertisement(
                    &adv,
                    link_mdu,
                    sdu,
                    now_ms,
                    self.max_incoming_resource_size,
                    self.resource_window_policy,
                ) {
                    Ok((incoming, req_payload)) => {
                        let req_packet = link.build_data_packet_with_context(
                            &req_payload,
                            PacketContext::ResourceReq,
                            &mut self.rng,
                        );
                        match req_packet {
                            Ok(pkt) => {
                                link.set_incoming_resource(incoming);
                                link.record_outbound(now_secs);
                                self.route_link_packet(&link_id, &pkt);
                                self.events.push(NodeEvent::ResourceTransferStarted {
                                    link_id,
                                    resource_hash,
                                    is_sender: false,
                                });
                                accepted_response_request = response_request_id;
                            }
                            Err(e) => {
                                crate::tracing::debug!("Failed to build REQ packet: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        crate::tracing::debug!("Failed to create IncomingResource: {e}");
                    }
                }
            }
            ResourceStrategy::AcceptApp => {
                // Store for application to accept/reject.
                link.set_pending_resource_adv(adv);
                self.events.push(NodeEvent::ResourceAdvertised {
                    link_id,
                    resource_hash,
                    transfer_size,
                    data_size,
                });
            }
            ResourceStrategy::AcceptNone => {
                crate::tracing::trace!("Resource ADV rejected (strategy=AcceptNone)");
            }
        }

        if let Some(request_id) = accepted_response_request {
            self.response_resource_downloads
                .insert(resource_hash, request_id);
            if let Some(pending) = self.pending_requests.get_mut(&request_id) {
                // The Resource transfer watchdog now owns liveness. Leaving
                // the ordinary response timer armed could fail a healthy
                // transfer on slow/single-threaded transports.
                pending.response_started_at_ms = None;
            }
        }

        // Bug #26: ADV arrival is peer progress — reset the
        // pending-request deadline for any in-flight request on this
        // link. The Response packet for a fetch_file request is
        // queued behind the resource data on slow links; resetting
        // here prevents the timer from firing mid-transfer.
        self.reset_pending_requests_on_link(&link_id, now_ms);
    }

    /// Handle a ResourceReq packet (sender receives receiver's request).
    fn handle_resource_req(
        &mut self,
        link_id: LinkId,
        packet: &Packet,
        now_ms: u64,
        now_secs: u64,
    ) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        if link.state() != LinkState::Active {
            // A retry REQ arriving on a not-yet/no-longer Active link is
            // silently dropped here; make that visible (#85).
            crate::tracing::debug!(
                event = "RESOURCE_REQ_DROP",
                reason = "link_not_active",
                link = %HexFmt(&link_id.as_bytes()[..4]),
                state = ?link.state(),
            );
            return;
        }
        link.record_inbound(now_secs);

        // Decrypt REQ
        let encrypted_data = packet.data.as_slice();
        let mut plaintext = alloc::vec![0u8; encrypted_data.len()];
        let len = match link.decrypt(encrypted_data, &mut plaintext) {
            Ok(len) => len,
            Err(_) => {
                crate::tracing::debug!("Resource REQ decryption failed");
                return;
            }
        };
        plaintext.truncate(len);

        // Forward to outgoing resource
        if link.outgoing_resource().is_none() {
            crate::tracing::debug!("Resource REQ but no outgoing resource");
            return;
        }

        // Need immutable link ref for handle_request, extract what we need first
        // Then call handle_request with a fresh borrow
        let packets = {
            let link_mut = self.links.get_mut(&link_id).unwrap();
            let mut res = link_mut.take_outgoing_resource().unwrap();
            let link_ref = &*link_mut; // immutable reborrow

            let result = res.handle_request(&plaintext, link_ref, &mut self.rng, now_ms);

            // Check status to decide what events to emit
            let status = res.status();

            // Capture progress before storing resource back
            let progress = res.progress();
            let transfer_size = res.transfer_size();
            let data_size = res.uncompressed_size();
            let res_hash = *res.resource_hash();

            // Put back
            self.links
                .get_mut(&link_id)
                .unwrap()
                .set_outgoing_resource(res);

            match result {
                Ok(pkts) => {
                    if status == crate::resource::ResourceStatus::Transferring
                        || status == crate::resource::ResourceStatus::AwaitingProof
                    {
                        // Emit progress (skip at 1.0, completion is a separate event)
                        if progress < 1.0 {
                            self.events.push(NodeEvent::ResourceProgress {
                                link_id,
                                resource_hash: res_hash,
                                progress,
                                transfer_size,
                                data_size,
                                is_sender: true,
                            });
                        }
                    }
                    pkts
                }
                Err(e) => {
                    crate::tracing::debug!("Resource handle_request failed: {e}");
                    return;
                }
            }
        };

        // Route all generated packets
        for pkt in &packets {
            self.route_link_packet(&link_id, pkt);
        }

        // Update outbound timestamp
        if let Some(link) = self.links.get_mut(&link_id) {
            link.record_outbound(now_secs);
        }
    }

    /// Handle a Resource data part (receiver gets raw data).
    fn handle_resource_data(
        &mut self,
        link_id: LinkId,
        packet: &Packet,
        now_ms: u64,
        now_secs: u64,
    ) {
        use crate::resource::incoming::ResourcePartResult;

        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        if link.state() != LinkState::Active {
            // A resource part arriving on a not-Active link is silently
            // dropped here; make that visible (#85).
            crate::tracing::debug!(
                event = "RESOURCE_PART_DROP",
                reason = "link_not_active",
                link = %HexFmt(&link_id.as_bytes()[..4]),
                state = ?link.state(),
            );
            return;
        }
        link.record_inbound(now_secs);

        // Resource data parts are NOT encrypted per-packet (context=RESOURCE skips encryption)
        // The data is raw (pre-encrypted in bulk by the sender)
        let part_data = packet.data.as_slice();
        let rtt_ms = link.rtt_ms();

        // Take incoming resource out for processing
        let mut incoming = match link.take_incoming_resource() {
            Some(r) => r,
            None => {
                crate::tracing::debug!("Resource data but no incoming resource");
                return;
            }
        };

        let result = incoming.receive_part(part_data, now_ms, rtt_ms);
        let resource_hash = *incoming.resource_hash();
        let resource_flags = incoming.flags();
        let has_request_id = incoming.request_id().is_some();
        let mut completed_internal_payload: Option<alloc::vec::Vec<u8>> = None;

        match result {
            ResourcePartResult::Continue => {
                link.set_incoming_resource(incoming);
            }
            ResourcePartResult::SendRequest(req_payload) => {
                // Capture progress before storing resource back
                let progress = incoming.progress();
                let transfer_size = incoming.transfer_size();
                let data_size = incoming.data_size();

                // Encrypt and send REQ
                let req_pkt = link.build_data_packet_with_context(
                    &req_payload,
                    PacketContext::ResourceReq,
                    &mut self.rng,
                );
                link.set_incoming_resource(incoming);

                // Emit progress event
                self.events.push(NodeEvent::ResourceProgress {
                    link_id,
                    resource_hash,
                    progress,
                    transfer_size,
                    data_size,
                    is_sender: false,
                });

                match req_pkt {
                    Ok(pkt) => {
                        link.record_outbound(now_secs);
                        self.route_link_packet(&link_id, &pkt);
                    }
                    Err(e) => {
                        crate::tracing::debug!(
                            "Resource REQ packet build failed on link {}: {:?}",
                            link_id,
                            e
                        );
                    }
                }
            }
            ResourcePartResult::Assembling => {
                // All parts received, assemble
                let link_ref = &*link;
                match incoming.assemble(link_ref) {
                    Ok((data, metadata)) => {
                        // Build and send proof, caching for CacheRequest re-send
                        let proof_data = incoming.build_proof();
                        match proof_data {
                            Ok(pd) => {
                                let proof_pkt = link.build_proof_packet_with_context(
                                    &pd,
                                    PacketContext::ResourcePrf,
                                );
                                if let Ok(pkt) = proof_pkt {
                                    let ph = crate::packet::packet_hash(&pkt);
                                    link.cache_resource_proof(ph, pkt.clone());
                                    link.record_outbound(now_secs);
                                    self.route_link_packet(&link_id, &pkt);
                                }
                            }
                            Err(e) => {
                                crate::tracing::debug!("Failed to build resource proof: {e}");
                            }
                        }

                        // Clear incoming resource (segment complete)
                        // (incoming is consumed, not put back)
                        let seg_idx = incoming.segment_index();
                        let total_segs = incoming.total_segments();

                        // Python `response_resource_concluded` (Link.py): a
                        // completed resource whose ADV flagged `is_response` and
                        // that matches an outstanding request is delivered to the
                        // request path as `ResponseReceived` — the SAME shape as
                        // the single-packet response — not the generic
                        // `ResourceCompleted`. A response resource WITH metadata
                        // is a file response (Python's `has_metadata` branch,
                        // NomadNet `serve_file`): its data is the RAW response,
                        // correlated by the request_id carried in the ADV. One
                        // WITHOUT metadata carries the wrapped
                        // `[request_id, response]` and is unpacked here.
                        // Responses up to RESOURCE_MAX_EFFICIENT_SIZE (~1 MiB) are
                        // single-segment, which covers every request response; a
                        // (never-in-practice) multi-segment response falls through
                        // to the generic resource path.
                        let response_delivery = if incoming.is_response() && total_segs == 1 {
                            if metadata.is_some() {
                                incoming
                                    .request_id()
                                    .and_then(|rid| {
                                        <[u8; crate::constants::TRUNCATED_HASHBYTES]>::try_from(rid)
                                            .ok()
                                    })
                                    .filter(|rid| self.pending_requests.contains_key(rid))
                                    .map(|rid| (rid, None))
                            } else {
                                Self::parse_wrapped_response(&data)
                                    .filter(|(rid, _)| self.pending_requests.contains_key(rid))
                                    .map(|(rid, unwrapped)| (rid, Some(unwrapped)))
                            }
                        } else {
                            None
                        };

                        if let Some((request_id, unwrapped)) = response_delivery {
                            self.remove_pending_request(&request_id);
                            // A file response's raw data IS the response value
                            // and its resource metadata (the `{"name": ...}`
                            // blob) rides along; a wrapped one delivers the
                            // unpacked value and has no metadata.
                            let (response_data, metadata) = match unwrapped {
                                Some(unwrapped) => (unwrapped, None),
                                None => (data, metadata),
                            };
                            self.events.push(NodeEvent::ResponseReceived {
                                link_id,
                                request_id,
                                response_data,
                                metadata,
                            });
                        } else {
                            if has_request_id
                                && (resource_flags.is_request || resource_flags.is_response)
                            {
                                completed_internal_payload = Some(data.clone());
                            }

                            self.events.push(NodeEvent::ResourceCompleted {
                                link_id,
                                resource_hash,
                                data,
                                metadata,
                                is_sender: false,
                                segment_index: seg_idx,
                                total_segments: total_segs,
                            });
                        }
                    }
                    Err(e) => {
                        crate::tracing::debug!("Resource assembly failed: {e}");
                        self.events.push(NodeEvent::ResourceFailed {
                            link_id,
                            resource_hash,
                            error: e,
                            is_sender: false,
                        });
                    }
                }
            }
            ResourcePartResult::InvalidPart => {
                link.set_incoming_resource(incoming);
                // Raised from trace to debug (#85): pairs with the detailed
                // RESOURCE_PART_REJECT event emitted inside receive_part.
                crate::tracing::debug!(
                    "Received resource part with no matching hash (rh={}, len={})",
                    HexFmt(&resource_hash[..4]),
                    part_data.len(),
                );
            }
        }

        // Bug #26: any resource-part arrival on this link means the
        // peer is answering. Reset the pending-request deadline so
        // the rtt-derived timer (mod.rs:694) does not fire mid-
        // transfer.
        self.reset_pending_requests_on_link(&link_id, now_ms);

        // Link-level request/response Resources complete through the same
        // parser, authorization and pending-request machinery as their packet
        // counterparts. Keep ResourceCompleted as transfer telemetry while
        // also producing the semantic RequestReceived/ResponseReceived event.
        if let Some(payload) = completed_internal_payload {
            if resource_flags.is_request {
                let request_id = crate::crypto::truncated_hash(&payload);
                self.handle_request_payload(link_id, request_id, &payload);
            } else if resource_flags.is_response {
                self.handle_response_payload(link_id, &payload);
            }
        }
    }

    /// Handle a ResourceHmu packet (hashmap update).
    fn handle_resource_hmu(
        &mut self,
        link_id: LinkId,
        packet: &Packet,
        _raw_packet: &[u8],
        now_ms: u64,
        now_secs: u64,
    ) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };
        if link.state() != LinkState::Active {
            return;
        }
        link.record_inbound(now_secs);

        // Decrypt HMU
        let encrypted_data = packet.data.as_slice();
        let mut plaintext = alloc::vec![0u8; encrypted_data.len()];
        let len = match link.decrypt(encrypted_data, &mut plaintext) {
            Ok(len) => len,
            Err(_) => {
                crate::tracing::debug!("Resource HMU decryption failed");
                return;
            }
        };
        plaintext.truncate(len);

        // Forward to incoming resource
        let mut incoming = match link.take_incoming_resource() {
            Some(r) => r,
            None => {
                crate::tracing::debug!("Resource HMU but no incoming resource");
                return;
            }
        };

        match incoming.handle_hashmap_update(now_ms, &plaintext) {
            Ok(Some(req_payload)) => {
                // Send the new REQ
                let req_pkt = link.build_data_packet_with_context(
                    &req_payload,
                    PacketContext::ResourceReq,
                    &mut self.rng,
                );
                link.set_incoming_resource(incoming);
                if let Ok(pkt) = req_pkt {
                    link.record_outbound(now_secs);
                    self.route_link_packet(&link_id, &pkt);
                }
            }
            Ok(None) => {
                link.set_incoming_resource(incoming);
            }
            Err(e) => {
                crate::tracing::debug!("Resource HMU processing failed: {e}");
                link.set_incoming_resource(incoming);
            }
        }
    }

    /// Handle resource cancellation (ICL or RCL).
    fn handle_resource_cancel(&mut self, link_id: LinkId, is_incoming_cancel: bool) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };

        if is_incoming_cancel {
            // ICL received = initiator (sender) cancelled = we are the receiver
            if let Some(res) = link.incoming_resource() {
                let resource_hash = *res.resource_hash();
                link.clear_incoming_resource();
                self.events.push(NodeEvent::ResourceFailed {
                    link_id,
                    resource_hash,
                    error: crate::resource::ResourceError::Cancelled,
                    is_sender: false,
                });
            }
        } else {
            // RCL received = receiver cancelled = we are the sender
            if let Some(res) = link.outgoing_resource() {
                let resource_hash = *res.resource_hash();
                link.clear_outgoing_resource();
                self.events.push(NodeEvent::ResourceFailed {
                    link_id,
                    resource_hash,
                    error: crate::resource::ResourceError::Cancelled,
                    is_sender: true,
                });
            }
        }
    }

    /// Handle a resource proof (sender receives completion proof).
    fn handle_resource_proof(&mut self, link_id: LinkId, proof_data: &[u8], now_ms: u64) {
        let Some(link) = self.links.get_mut(&link_id) else {
            return;
        };

        let mut res = match link.take_outgoing_resource() {
            Some(r) => r,
            None => {
                crate::tracing::debug!("Resource proof but no outgoing resource");
                return;
            }
        };

        let outcome = res.handle_proof(proof_data);

        // A validated resource proof is link activity (#124, L-0019):
        // refresh the activity clock and recover a Stale link BEFORE
        // branching on the status. The refresh used to sit on the
        // non-Complete `Ok(_)` arm below — but `handle_proof` returns only
        // `Ok(Complete)` or `Err`, so the live success path (the completion
        // proof, and each segment proof of a split transfer) never recorded
        // activity. Python: Link.py:972-984 `receive()` sets `last_inbound`
        // for every link packet, RESOURCE_PRF included, and flips
        // STALE back to ACTIVE.
        if outcome.is_ok() {
            let now_secs = now_ms / MS_PER_SECOND;
            self.try_recover_stale(&link_id, now_secs);
            if let Some(link) = self.links.get_mut(&link_id) {
                link.record_inbound(now_secs);
            }
        }

        match outcome {
            Ok(crate::resource::ResourceStatus::Complete) => {
                let resource_hash = *res.resource_hash();
                // Telemetry (leviculum#35): the completion proof confirms the
                // peer reassembled this whole (segment's) transfer — credit its
                // bytes as delivered on the link. Per-segment for split
                // transfers, so the counter advances as the transfer does.
                let delivered = res.transfer_size();
                if let Some(l) = self.links.get_mut(&link_id) {
                    l.add_resource_bytes_delivered(delivered);
                }
                // Segment complete. If this is a split transfer with more
                // segments to send, advertise the next one instead of signalling
                // completion (mirrors Python's next_segment.advertise()). Only
                // the final segment emits the sender's ResourceCompleted.
                let total_segments = match self.advance_outgoing_segments(&link_id, now_ms) {
                    SegmentAdvance::Sent => return,
                    SegmentAdvance::Done { total_segments } => total_segments,
                    SegmentAdvance::Failed { error } => {
                        // A mid-transfer segment build or link-away: the
                        // transfer stopped short, so report failure instead of
                        // completion. The resource is not put back (Codeberg
                        // #270).
                        crate::tracing::debug!(
                            "Resource segment advance failed: {error}; reporting failure"
                        );
                        self.events.push(NodeEvent::ResourceFailed {
                            link_id,
                            resource_hash,
                            error,
                            is_sender: true,
                        });
                        return;
                    }
                };

                // Don't put the resource back, transfer is complete.
                self.events.push(NodeEvent::ResourceCompleted {
                    link_id,
                    resource_hash,
                    data: Vec::new(), // sender doesn't return data
                    metadata: None,
                    is_sender: true,
                    segment_index: total_segments,
                    total_segments,
                });
            }
            Ok(_) => {
                // Defensive arm for a future non-Complete success status:
                // keep the transfer in flight. (Activity was already
                // refreshed above.)
                if let Some(link) = self.links.get_mut(&link_id) {
                    link.set_outgoing_resource(res);
                }
            }
            Err(e) => {
                let resource_hash = *res.resource_hash();
                crate::tracing::debug!("Resource proof validation failed: {e}");
                // Drop any remaining segment plan on failure.
                if let Some(link) = self.links.get_mut(&link_id) {
                    let _ = link.take_outgoing_segments();
                }
                self.events.push(NodeEvent::ResourceFailed {
                    link_id,
                    resource_hash,
                    error: e,
                    is_sender: true,
                });
            }
        }
    }

    /// After a segment's proof, build and advertise the next segment of a split
    /// transfer, if any.
    ///
    /// The completed segment's OutgoingResource has already been taken by the
    /// caller and is not put back; when a next segment exists it replaces the
    /// resource on the link. Returns [`SegmentAdvance::Sent`] when the next
    /// segment was advertised (the caller must not emit ResourceCompleted yet),
    /// [`SegmentAdvance::Done`] with the transfer's total segment count when
    /// there are no more segments (single-segment transfers report 1), or
    /// [`SegmentAdvance::Failed`] when the next segment could not be built or
    /// the link went away mid-transfer (Codeberg #270).
    fn advance_outgoing_segments(&mut self, link_id: &LinkId, now_ms: u64) -> SegmentAdvance {
        use crate::packet::PacketContext;

        let mut plan = match self
            .links
            .get_mut(link_id)
            .and_then(|l| l.take_outgoing_segments())
        {
            Some(p) => p,
            None => return SegmentAdvance::Done { total_segments: 1 },
        };

        if !plan.has_next() {
            // Last segment just completed; drop the plan and signal completion.
            return SegmentAdvance::Done {
                total_segments: plan.total_segments(),
            };
        }

        let index = plan.next_index();

        // Build the next segment. Needs an immutable link borrow plus rng.
        let build = {
            let Some(link) = self.links.get(link_id) else {
                // Link gone before the next segment could be built: the
                // transfer failed, it did not complete (Codeberg #270).
                return SegmentAdvance::Failed {
                    error: crate::resource::ResourceError::LinkClosed,
                };
            };
            plan.build_segment(index, &link.resource_crypt_params(), &mut self.rng, now_ms)
        };

        let outgoing = match build {
            Ok(o) => o,
            Err(e) => {
                crate::tracing::debug!("Failed to build resource segment {index}: {e}");
                if let Some(link) = self.links.get_mut(link_id) {
                    link.clear_outgoing_resource();
                }
                return SegmentAdvance::Failed { error: e };
            }
        };

        let adv_bytes = outgoing.adv_packet().to_vec();
        plan.advance();

        let Some(link) = self.links.get_mut(link_id) else {
            // Link gone after the segment was built: still a failed transfer.
            return SegmentAdvance::Failed {
                error: crate::resource::ResourceError::LinkClosed,
            };
        };
        link.set_outgoing_resource(outgoing);
        link.set_outgoing_segments(plan);

        let adv_pkt = self.links.get_mut(link_id).and_then(|l| {
            l.build_data_packet_with_context(&adv_bytes, PacketContext::ResourceAdv, &mut self.rng)
                .ok()
        });
        if let Some(pkt) = adv_pkt {
            self.route_link_packet(link_id, &pkt);
        }
        SegmentAdvance::Sent
    }

    // Internal: Timeout / Polling
    /// Check for handshake timeouts on pending links
    pub(super) fn check_timeouts(&mut self, now_ms: u64) {
        let timed_out: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, link)| {
                let started_at = match link.phase() {
                    LinkPhase::PendingOutgoing { created_at_ms } => created_at_ms,
                    LinkPhase::PendingIncoming { proof_sent_at_ms } => proof_sent_at_ms,
                    LinkPhase::Established => return false,
                };
                now_ms.saturating_sub(started_at) > link.establishment_timeout_ms()
            })
            .map(|(id, _)| *id)
            .collect();

        for link_id in timed_out {
            let (is_initiator, destination_hash) = self
                .links
                .get(&link_id)
                .map(|l| (l.is_initiator(), *l.destination_hash()))
                .unwrap_or((false, DestinationHash::new([0; 16])));

            // Link request retry: if this is an initiator link and
            // retries remain, resend the link request and reset the timeout.
            //
            // Codeberg #66: the retry regenerates the ephemeral keys
            // before resending. An identical re-send dies in the FIRST
            // transport node's duplicate-hash dedup (DEDUP_DROP), making
            // a request lost on any later hop unrecoverable. Fresh keys
            // give the retry new bytes and a new link id — on the wire
            // indistinguishable from a Python application retrying with
            // a new RNS.Link (Python itself never retries internally).
            // The link is re-keyed in `links` under the new id; ids
            // already handed to callers resolve via `link_id_aliases`.
            if is_initiator {
                let retries_left = self
                    .link_retry_state
                    .get(&link_id)
                    .map(|r| r.remaining)
                    .unwrap_or(0);
                if retries_left > 0 {
                    let now_ms = self.transport.clock().now_ms();
                    if let Some(mut link) = self.links.remove(&link_id) {
                        let dest_hash_bytes = *destination_hash.as_bytes();
                        let (next_hop, hops) =
                            if let Some(path) = self.transport.path(&dest_hash_bytes) {
                                if path.needs_relay() {
                                    (path.next_hop, path.hops)
                                } else {
                                    (None, path.hops)
                                }
                            } else {
                                (None, 1)
                            };
                        let hw_mtu = self.transport.next_hop_interface_hw_mtu(&dest_hash_bytes);
                        link.regenerate_ephemeral_keys(&mut self.rng);
                        // Codeberg #129: fresh timeout jitter per retry so
                        // concurrent initiators keep de-syncing instead of
                        // retransmitting in lockstep.
                        link.reroll_establishment_jitter(&mut self.rng);
                        let packet =
                            link.build_link_request_packet_with_transport(next_hop, hops, hw_mtu);
                        let new_link_id = *link.id();
                        link.set_phase(LinkPhase::PendingOutgoing {
                            created_at_ms: now_ms,
                        });
                        crate::tracing::debug!(
                            "Link <{}> establishment timed out, retrying with fresh keys as <{}> ({} retries left)",
                            HexShort(link_id.as_bytes()),
                            HexShort(new_link_id.as_bytes()),
                            retries_left - 1
                        );
                        self.links.insert(new_link_id, link);

                        // Keep the insert↔register pairing (see remove_link)
                        // across the re-key.
                        self.transport.unregister_destination(link_id.as_bytes());
                        self.transport.register_destination(*new_link_id.as_bytes());

                        // Rebind retry bookkeeping and caller-visible ids.
                        if let Some(mut retry) = self.link_retry_state.remove(&link_id) {
                            retry.remaining -= 1;
                            self.link_retry_state.insert(new_link_id, retry);
                        }
                        for target in self.link_id_aliases.values_mut() {
                            if *target == link_id {
                                *target = new_link_id;
                            }
                        }
                        self.link_id_aliases.insert(link_id, new_link_id);
                        // Event boundary: outbound events for this link
                        // report the ORIGINAL caller-visible id.
                        let origin = self.link_origin_ids.remove(&link_id).unwrap_or(link_id);
                        self.link_origin_ids.insert(new_link_id, origin);

                        // Route the retried request
                        let was_routed = self
                            .transport
                            .send_to_destination(&dest_hash_bytes, &packet)
                            .is_ok();
                        if !was_routed {
                            self.transport.send_on_all_interfaces(&packet);
                        }
                        continue;
                    }
                }
            }

            // No retries left (or not initiator): original behavior
            crate::tracing::debug!(
                "Link <{}> establishment timed out (no retries left)",
                HexShort(link_id.as_bytes())
            );
            if let Some(l) = self.links.get(&link_id) {
                crate::tracing::debug!(
                    target: "leviculum_core::link",
                    "LINK_DIED link={} reason=other detail=handshake_timeout elapsed_since_activity_ms={} threshold_ms={} rtt_ms={} keepalives_sent={} keepalives_acked={}",
                    HexShort(link_id.as_bytes()),
                    now_ms.saturating_sub(l.last_inbound_secs().saturating_mul(MS_PER_SECOND)),
                    l.establishment_timeout_ms(),
                    l.rtt_ms(),
                    l.keepalives_sent(),
                    l.keepalives_acked(),
                );
            }
            self.remove_link(&link_id);
            self.emit_link_closed(
                link_id,
                LinkCloseReason::Timeout,
                is_initiator,
                destination_hash,
            );
        }

        // Raw Link packets mirror Python PacketReceipt timeout callbacks.
        // Channel receipt expiry remains silent because Channel owns retries
        // and terminal failure independently.
        for expired in self.receipt_tracker.expire(now_ms) {
            if expired.sequence.is_none() {
                self.events.push(NodeEvent::LinkDeliveryFailed {
                    link_id: expired.link_id,
                    packet_hash: expired.full_hash,
                });
            }
        }
    }

    /// Confirm RTT delivery on an initiator link if not yet confirmed.
    ///
    /// Any inbound link traffic (data packet, proof, keepalive) proves the
    /// responder is alive, stopping unnecessary RTT retries.
    fn try_confirm_rtt(&mut self, link_id: &LinkId) {
        if let Some(link) = self.links.get_mut(link_id) {
            if link.is_initiator() && !link.rtt_confirmed() {
                link.confirm_rtt();
                crate::tracing::debug!(
                    "Link <{}> RTT confirmed by inbound traffic",
                    HexShort(link_id.as_bytes())
                );
            }
        }
    }

    /// Resend RTT packet on initiator links where delivery is unconfirmed.
    ///
    /// The initiator sends the RTT packet once in `handle_link_proof()`. If the
    /// packet is lost (common on LoRa), this method retransmits at intervals of
    /// `max(rtt_ms * 3, 10s)` up to [`RTT_RETRY_MAX_ATTEMPTS`] retries. Any
    /// inbound link packet sets `rtt_confirmed`, stopping retries. Retries are
    /// bounded by [`RTT_RETRY_MAX_ATTEMPTS`]; links that exhaust them without
    /// confirmation are left to the keepalive/stale watchdog (Python-parity
    /// reaping), not torn down here.
    pub(super) fn check_rtt_retry(&mut self, now_ms: u64) {
        // Phase 1: Collect link_ids needing an RTT retry
        let need_retry: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, link)| {
                link.needs_rtt_retry()
                    && link.rtt_sent_at_ms().is_some_and(|sent| {
                        now_ms.saturating_sub(sent) >= link.rtt_retry_interval_ms()
                    })
            })
            .map(|(id, _)| *id)
            .collect();

        // Phase 2: Resend RTT for each qualifying link
        for link_id in need_retry {
            if let Some(link) = self.links.get_mut(&link_id) {
                // Use stored RTT if available, otherwise conservative default.
                // The initiator stores rtt_us in handle_link_proof(); if it's
                // None (e.g., sub-ms link), fall back to CHANNEL_DEFAULT_RTT_MS
                // to avoid sending 0.0 which corrupts responder timeouts.
                let rtt_secs = link
                    .rtt_secs()
                    .unwrap_or(CHANNEL_DEFAULT_RTT_MS as f64 / MS_PER_SECOND as f64);
                match link.build_rtt_packet(rtt_secs, &mut self.rng) {
                    Ok(packet) => {
                        link.record_rtt_sent(now_ms);
                        crate::tracing::debug!(
                            "Resending RTT on link <{}> (attempt {}/{})",
                            HexShort(link_id.as_bytes()),
                            link.rtt_send_count(),
                            RTT_RETRY_MAX_ATTEMPTS + 1,
                        );
                        self.route_link_packet(&link_id, &packet);
                    }
                    Err(e) => {
                        crate::tracing::warn!("RTT retry build failed: {:?}", e);
                    }
                }
            }
        }
    }

    /// Check if any active links need to send keepalives (initiator only)
    pub(super) fn check_keepalives(&mut self, now_secs: u64) {
        let need_keepalive: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, link)| link.is_initiator() && link.should_send_keepalive(now_secs))
            .map(|(id, _)| *id)
            .collect();

        for link_id in need_keepalive {
            if let Some(link) = self.links.get_mut(&link_id) {
                if let Ok(packet) = link.build_keepalive_packet() {
                    crate::tracing::trace!(
                        "Sent keepalive on link <{}>",
                        HexShort(link_id.as_bytes())
                    );
                    link.record_keepalive_sent(now_secs);
                    crate::tracing::debug!(
                        target: "leviculum_core::link",
                        "LINK_KEEPALIVE_TX link={} t_ms={} keepalives_sent={}",
                        HexShort(link_id.as_bytes()),
                        now_secs.saturating_mul(MS_PER_SECOND),
                        link.keepalives_sent(),
                    );
                    self.route_link_packet(&link_id, &packet);
                }
            }
        }
    }

    /// Check for stale links and close them if timeout expired
    pub(super) fn check_stale_links(&mut self, now_secs: u64) {
        // First pass: Find links that are active but should become stale
        let newly_stale: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, link)| link.state() == LinkState::Active && link.is_stale(now_secs))
            .map(|(id, _)| *id)
            .collect();

        for link_id in newly_stale {
            if let Some(link) = self.links.get_mut(&link_id) {
                crate::tracing::debug!(
                    "Link <{}> marked stale, no activity",
                    HexShort(link_id.as_bytes())
                );
                link.set_state(LinkState::Stale);
                self.events.push(NodeEvent::LinkStale { link_id });
            }
        }

        // Second pass: Find stale links that should be closed
        let should_close: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, link)| link.state() == LinkState::Stale && link.should_close(now_secs))
            .map(|(id, _)| *id)
            .collect();

        for link_id in should_close {
            // Extract all data from link, build close packet, then drop borrow
            let close_info = if let Some(link) = self.links.get_mut(&link_id) {
                let is_initiator = link.is_initiator();
                let destination_hash = *link.destination_hash();

                // Classify the death before tearing the link down. The single
                // inactivity watchdog (last_inbound) drives both cases; the
                // keepalive counters distinguish them:
                //   keepalive_timeout = initiator sent keepalives that went
                //     unanswered (probed, silence) -> lossy-path margin (case b)
                //   stale = pure inactivity, no unanswered keepalive probe
                let sent = link.keepalives_sent();
                let acked = link.keepalives_acked();
                let reason = if is_initiator && sent > acked {
                    "keepalive_timeout"
                } else {
                    "stale"
                };
                let elapsed_ms = now_secs
                    .saturating_sub(link.last_inbound_secs())
                    .saturating_mul(MS_PER_SECOND);
                crate::tracing::debug!(
                    target: "leviculum_core::link",
                    "LINK_DIED link={} reason={} elapsed_since_activity_ms={} threshold_ms={} rtt_ms={} keepalives_sent={} keepalives_acked={}",
                    HexShort(link_id.as_bytes()),
                    reason,
                    elapsed_ms,
                    link.stale_close_timeout_secs().saturating_mul(MS_PER_SECOND),
                    link.rtt_ms(),
                    sent,
                    acked,
                );

                let close_packet = link.build_close_packet(&mut self.rng).ok();
                link.close();
                Some((is_initiator, destination_hash, close_packet))
            } else {
                None
            };
            // link borrow dropped here

            if let Some((is_initiator, destination_hash, close_packet)) = close_info {
                crate::tracing::debug!("Closing stale link <{}>", HexShort(link_id.as_bytes()));
                // Route close packet BEFORE removing link from map
                if let Some(ref pkt) = close_packet {
                    self.route_link_packet(&link_id, pkt);
                }
                self.remove_link(&link_id);
                self.emit_link_closed(
                    link_id,
                    LinkCloseReason::Stale,
                    is_initiator,
                    destination_hash,
                );
            }
        }
    }

    /// Check for channel envelope timeouts and queue retransmissions
    pub(super) fn check_channel_timeouts(&mut self, now_ms: u64) {
        // Collect link IDs that have channels
        let channel_link_ids: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, link)| link.has_channel())
            .map(|(id, _)| *id)
            .collect();

        for link_id in channel_link_ids {
            // Get link, extract RTT, poll channel, actions is owned Vec, releasing borrow
            let actions = match self.links.get_mut(&link_id) {
                Some(link) => {
                    let rtt_ms = link.rtt_ms();
                    match link.channel_mut() {
                        Some(ch) => ch.poll(now_ms, rtt_ms),
                        None => continue,
                    }
                }
                None => continue,
            };

            // Process actions (re-borrow link for each)
            for action in actions {
                match action {
                    ChannelAction::Retransmit {
                        sequence,
                        data,
                        tries,
                    } => {
                        crate::tracing::debug!(
                            seq = sequence,
                            "link_mgr: queuing channel retransmit"
                        );
                        // Build retransmission packet
                        let packet = if let Some(link) = self.links.get(&link_id) {
                            link.build_data_packet_with_context(
                                &data,
                                PacketContext::Channel,
                                &mut self.rng,
                            )
                            .ok()
                        } else {
                            None
                        };
                        // link borrow released here

                        if let Some(packet) = packet {
                            // Register receipt for the re-encrypted packet
                            self.receipt_tracker
                                .register(&packet, link_id, sequence, now_ms);

                            self.events.push(NodeEvent::ChannelRetransmit {
                                link_id,
                                sequence,
                                tries,
                            });

                            // Route directly via transport
                            self.route_link_packet(&link_id, &packet);
                        }
                    }
                    ChannelAction::TearDownLink => {
                        crate::tracing::debug!("link_mgr: channel teardown — closing link");
                        let (is_initiator, destination_hash) = self
                            .links
                            .get(&link_id)
                            .map(|l| (l.is_initiator(), *l.destination_hash()))
                            .unwrap_or((false, DestinationHash::new([0; 16])));

                        // Build and route close packet BEFORE removing link
                        let close_packet = if let Some(link) = self.links.get_mut(&link_id) {
                            let pkt = link.build_close_packet(&mut self.rng).ok();
                            link.close();
                            pkt
                        } else {
                            None
                        };
                        if let Some(ref pkt) = close_packet {
                            self.route_link_packet(&link_id, pkt);
                        }

                        self.remove_link(&link_id);
                        self.emit_link_closed(
                            link_id,
                            LinkCloseReason::ChannelExhausted,
                            is_initiator,
                            destination_hash,
                        );
                    }
                }
            }
        }
    }

    /// Check for resource transfer timeouts.
    pub(super) fn check_resource_timeouts(&mut self, now_ms: u64) {
        use crate::resource::outgoing::ResourcePollResult;

        let resource_link_ids: Vec<LinkId> = self
            .links
            .iter()
            .filter(|(_, l)| l.has_outgoing_resource() || l.has_incoming_resource())
            .map(|(id, _)| *id)
            .collect();

        let now_secs = now_ms / MS_PER_SECOND;

        for link_id in resource_link_ids {
            let rtt_ms = self.links.get(&link_id).map(|l| l.rtt_ms()).unwrap_or(5000);

            // Poll outgoing resource, collect result into owned enum, release borrow
            let out_result = match self.links.get_mut(&link_id) {
                Some(link) => link
                    .outgoing_resource_mut()
                    .map(|res| res.poll(now_ms, rtt_ms)),
                None => continue,
            };

            if let Some(result) = out_result {
                match result {
                    ResourcePollResult::RetransmitAdv(adv_bytes) => {
                        // Re-send advertisement
                        let pkt = if let Some(link) = self.links.get(&link_id) {
                            link.build_data_packet_with_context(
                                &adv_bytes,
                                PacketContext::ResourceAdv,
                                &mut self.rng,
                            )
                            .ok()
                        } else {
                            None
                        };
                        if let Some(pkt) = pkt {
                            if let Some(link) = self.links.get_mut(&link_id) {
                                link.record_outbound(now_secs);
                            }
                            self.route_link_packet(&link_id, &pkt);
                        }
                    }
                    ResourcePollResult::TimedOut => {
                        let resource_hash = self
                            .links
                            .get(&link_id)
                            .and_then(|l| l.outgoing_resource())
                            .map(|r| *r.resource_hash());
                        if let Some(link) = self.links.get_mut(&link_id) {
                            link.clear_outgoing_resource();
                        }
                        if let Some(resource_hash) = resource_hash {
                            // Tell the peer we gave up (L-0062), or it keeps
                            // REQing into a transfer that no longer exists.
                            // Python: Resource.cancel() (Resource.py:1086-1095)
                            // sends RESOURCE_ICL carrying the resource hash
                            // when it is the initiator; the receiver-side
                            // timeout below stays silent, as in Python.
                            let icl = if let Some(link) = self.links.get(&link_id) {
                                link.build_data_packet_with_context(
                                    &resource_hash,
                                    PacketContext::ResourceIcl,
                                    &mut self.rng,
                                )
                                .ok()
                            } else {
                                None
                            };
                            if let Some(pkt) = icl {
                                if let Some(link) = self.links.get_mut(&link_id) {
                                    link.record_outbound(now_secs);
                                }
                                self.route_link_packet(&link_id, &pkt);
                            }
                            self.events.push(NodeEvent::ResourceFailed {
                                link_id,
                                resource_hash,
                                error: crate::resource::ResourceError::Timeout,
                                is_sender: true,
                            });
                        }
                    }
                    ResourcePollResult::RequestProof { proof_data } => {
                        // Reconstruct expected proof packet to get deterministic packet_hash
                        let packet_hash = if let Some(link) = self.links.get(&link_id) {
                            link.build_proof_packet_with_context(
                                &proof_data,
                                PacketContext::ResourcePrf,
                            )
                            .ok()
                            .map(|mock| crate::packet::packet_hash(&mock))
                        } else {
                            None
                        };
                        if let Some(ph) = packet_hash {
                            // CacheRequest is NOT encrypted (Python Packet.py:209-211)
                            let cache_req = if let Some(link) = self.links.get(&link_id) {
                                link.build_raw_data_packet(&ph, PacketContext::CacheRequest)
                                    .ok()
                            } else {
                                None
                            };
                            if let Some(pkt) = cache_req {
                                if let Some(link) = self.links.get_mut(&link_id) {
                                    link.record_outbound(now_secs);
                                }
                                self.route_link_packet(&link_id, &pkt);
                            }
                        }
                    }
                    ResourcePollResult::Nothing => {}
                }
            }

            // Poll incoming resource, same pattern
            let in_result = match self.links.get_mut(&link_id) {
                Some(link) => link
                    .incoming_resource_mut()
                    .map(|res| res.poll(now_ms, rtt_ms)),
                None => continue,
            };

            if let Some(result) = in_result {
                match result {
                    ResourcePollResult::RetransmitAdv(req_bytes) => {
                        // Re-send last REQ
                        let pkt = if let Some(link) = self.links.get(&link_id) {
                            link.build_data_packet_with_context(
                                &req_bytes,
                                PacketContext::ResourceReq,
                                &mut self.rng,
                            )
                            .ok()
                        } else {
                            None
                        };
                        if let Some(pkt) = pkt {
                            if let Some(link) = self.links.get_mut(&link_id) {
                                link.record_outbound(now_secs);
                            }
                            self.route_link_packet(&link_id, &pkt);
                        }
                    }
                    ResourcePollResult::TimedOut => {
                        let resource_hash = self
                            .links
                            .get(&link_id)
                            .and_then(|l| l.incoming_resource())
                            .map(|r| *r.resource_hash());
                        if let Some(link) = self.links.get_mut(&link_id) {
                            link.clear_incoming_resource();
                        }
                        if let Some(resource_hash) = resource_hash {
                            self.events.push(NodeEvent::ResourceFailed {
                                link_id,
                                resource_hash,
                                error: crate::resource::ResourceError::Timeout,
                                is_sender: false,
                            });
                        }
                    }
                    ResourcePollResult::RequestProof { .. } => {
                        // Incoming resources never produce RequestProof
                    }
                    ResourcePollResult::Nothing => {}
                }
            }
        }
    }

    /// Compute the earliest deadline across all link-layer timers
    pub(super) fn link_next_deadline(&self, now_ms: u64) -> Option<u64> {
        let now_secs = now_ms / MS_PER_SECOND;
        let mut earliest: Option<u64> = None;

        let mut update = |deadline_ms: u64| {
            earliest = Some(match earliest {
                Some(e) => core::cmp::min(e, deadline_ms),
                None => deadline_ms,
            });
        };

        // Pending handshake timeouts
        for link in self.links.values() {
            match link.phase() {
                LinkPhase::PendingOutgoing { created_at_ms } => {
                    update(created_at_ms.saturating_add(link.establishment_timeout_ms()));
                }
                LinkPhase::PendingIncoming { proof_sent_at_ms } => {
                    update(proof_sent_at_ms.saturating_add(link.establishment_timeout_ms()));
                }
                LinkPhase::Established => {}
            }
        }

        // RTT retry deadlines (initiator, unconfirmed)
        for link in self.links.values() {
            if link.needs_rtt_retry() {
                if let Some(sent_at) = link.rtt_sent_at_ms() {
                    update(sent_at.saturating_add(link.rtt_retry_interval_ms()));
                }
            }
        }

        // Data receipt timeouts
        if let Some(deadline) = self.receipt_tracker.earliest_expiry() {
            update(deadline);
        }

        // Active link keepalive and stale deadlines
        for link in self.links.values() {
            match link.state() {
                LinkState::Active => {
                    if link.is_initiator() {
                        let base = if link.last_keepalive_secs() > 0 {
                            link.last_keepalive_secs()
                        } else {
                            link.established_at_secs().unwrap_or(now_secs)
                        };
                        let next_keepalive_secs = base.saturating_add(link.keepalive_secs());
                        update(next_keepalive_secs.saturating_mul(MS_PER_SECOND));
                    }
                    if link.last_inbound_secs() > 0 {
                        let stale_at_secs = link
                            .last_inbound_secs()
                            .saturating_add(link.stale_time_secs());
                        update(stale_at_secs.saturating_mul(MS_PER_SECOND));
                    }
                }
                LinkState::Stale => {
                    update(now_ms.saturating_add(MS_PER_SECOND));
                }
                _ => {}
            }
        }

        // Channel retransmit deadlines
        if self.links.values().any(|l| l.has_channel()) {
            update(now_ms.saturating_add(MS_PER_SECOND));
        }

        // Pending request timeout deadlines
        for pr in self.pending_requests.values() {
            if let Some(started) = pr.response_started_at_ms {
                update(started.saturating_add(pr.timeout_ms));
            }
        }

        // Resource transfer deadlines
        for link in self.links.values() {
            let rtt_ms = link.rtt_ms();
            if let Some(res) = link.outgoing_resource() {
                if let Some(deadline) = res.next_deadline(rtt_ms) {
                    update(deadline);
                }
            }
            if let Some(res) = link.incoming_resource() {
                if let Some(deadline) = res.next_deadline(rtt_ms) {
                    update(deadline);
                }
            }
        }

        earliest
    }

    // Internal: Helpers
    /// Route a link packet via attached interface, with path lookup fallback.
    pub(super) fn route_link_packet(&mut self, link_id: &LinkId, data: &[u8]) {
        if let Some(link) = self.links.get(link_id) {
            if let Some(iface_idx) = link.attached_interface() {
                if let Err(e) = self.transport.send_on_interface(iface_idx, data) {
                    crate::tracing::debug!(%e, "send_on_interface failed");
                }
            } else {
                let dest_hash = *link.destination_hash();
                if let Err(e) = self
                    .transport
                    .send_to_destination(dest_hash.as_bytes(), data)
                {
                    crate::tracing::warn!(%e, "send_to_destination failed — packet lost");
                }
            }
        }
    }

    /// Emit LinkClosed event and run path recovery.
    ///
    /// INVARIANT: The link has already been removed from self.links
    /// at all call sites. This method must not access the link.
    fn emit_link_closed(
        &mut self,
        link_id: LinkId,
        reason: LinkCloseReason,
        is_initiator: bool,
        destination_hash: DestinationHash,
    ) {
        // Translate a re-keyed wire id back to the caller-visible
        // original and consume the mapping — the close is the last
        // event for this link (Codeberg #66). Registries may hold
        // either id (receipts register with the resolved wire id,
        // requests with whatever the caller passed), so clean up both.
        let wire_id = link_id;
        let link_id = self.link_origin_ids.remove(&wire_id).unwrap_or(wire_id);

        // Clean up all receipt entries for this link
        self.receipt_tracker.remove_for_link(&wire_id);
        if wire_id != link_id {
            self.receipt_tracker.remove_for_link(&link_id);
        }

        // Clean up pending requests for this link (no timeout events. LinkClosed suffices)
        self.remove_pending_requests_for_link(wire_id, link_id);

        // Path recovery for locally-initiated links that never activated
        // (Python Transport.py:472-494)
        if reason == LinkCloseReason::Timeout
            && is_initiator
            && !self.transport.config().enable_transport
        {
            let dest_hash = destination_hash.into_bytes();
            self.transport.expire_path(&dest_hash);
            let now = self.transport.clock().now_ms();
            let mut tag = [0u8; TRUNCATED_HASHBYTES];
            tag[..8].copy_from_slice(&now.to_be_bytes());
            tag[8..16].copy_from_slice(&dest_hash[..8]);
            if let Err(e) = self.transport.request_path(&dest_hash, None, &tag) {
                crate::tracing::debug!(%e, "path request failed (best-effort)");
            }
        }

        self.events.push(NodeEvent::LinkClosed {
            link_id,
            reason,
            is_initiator,
            destination_hash,
        });
    }

    pub(super) fn repack_packet(&self, packet: &Packet) -> Option<Vec<u8>> {
        let size = packet.packed_size();
        let mut buf = alloc::vec![0u8; size];
        match packet.pack(&mut buf) {
            Ok(len) => {
                buf.truncate(len);
                Some(buf)
            }
            Err(e) => {
                crate::tracing::warn!(?e, "repack_packet failed — dropping packet");
                None
            }
        }
    }
}

#[cfg(test)]
mod receipt_tracker_tests {
    use super::*;

    fn make_link_id(b: u8) -> LinkId {
        LinkId::new([b; 16])
    }

    #[test]
    fn test_register_retransmit_replaces_entry() {
        let mut tracker = ReceiptTracker::new();
        let link = make_link_id(0xAA);

        // Use data with distinct hashable parts (packet_hash strips first 2 bytes)
        let data_v1 = [0x00u8; 64];
        let data_v2 = [0xFFu8; 64];

        tracker.register(&data_v1, link, 1, 1000);
        assert_eq!(tracker.len(), 1);

        // Re-register same (link, seq) with different data
        tracker.register(&data_v2, link, 1, 2000);
        assert_eq!(tracker.len(), 1);

        // Entry should have the new hashes
        let new_hash = packet_hash(&data_v2);
        let mut new_truncated = [0u8; TRUNCATED_HASHBYTES];
        new_truncated.copy_from_slice(&new_hash[..TRUNCATED_HASHBYTES]);
        assert!(tracker.contains_truncated(&new_truncated));

        // Old entry should be gone
        let old_hash = packet_hash(&data_v1);
        let mut old_truncated = [0u8; TRUNCATED_HASHBYTES];
        old_truncated.copy_from_slice(&old_hash[..TRUNCATED_HASHBYTES]);
        assert!(!tracker.contains_truncated(&old_truncated));
    }

    #[test]
    fn test_confirm_delivery_unknown_hash_returns_none() {
        let mut tracker = ReceiptTracker::new();
        let link = make_link_id(0xBB);

        tracker.register(b"some_packet", link, 5, 1000);
        assert_eq!(tracker.len(), 1);

        let unknown = [0xFFu8; TRUNCATED_HASHBYTES];
        assert!(tracker.confirm_delivery(&unknown).is_none());
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_remove_for_link_only_affects_target() {
        let mut tracker = ReceiptTracker::new();
        let link_a = make_link_id(0xAA);
        let link_b = make_link_id(0xBB);

        tracker.register(b"a1", link_a, 1, 1000);
        tracker.register(b"a2", link_a, 2, 1000);
        tracker.register(b"b1", link_b, 1, 1000);
        assert_eq!(tracker.count_for_link(&link_a), 2);
        assert_eq!(tracker.count_for_link(&link_b), 1);

        tracker.remove_for_link(&link_a);
        assert_eq!(tracker.count_for_link(&link_a), 0);
        assert_eq!(tracker.count_for_link(&link_b), 1);
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_expire_partial() {
        let mut tracker = ReceiptTracker::new();
        let link = make_link_id(0xCC);

        tracker.register(b"old_packet", link, 1, 1000);
        tracker.register(b"new_packet", link, 2, 1000 + DATA_RECEIPT_TIMEOUT_MS);

        // Advance time so only the first has expired
        let now = 1000 + DATA_RECEIPT_TIMEOUT_MS + 1;
        tracker.expire(now);

        assert_eq!(tracker.len(), 1);
        // The surviving entry should be the newer one (seq=2)
        let new_hash = packet_hash(b"new_packet");
        let mut new_truncated = [0u8; TRUNCATED_HASHBYTES];
        new_truncated.copy_from_slice(&new_hash[..TRUNCATED_HASHBYTES]);
        assert!(tracker.contains_truncated(&new_truncated));
    }

    #[test]
    fn test_register_then_confirm_then_register_same_seq() {
        let mut tracker = ReceiptTracker::new();
        let link = make_link_id(0xDD);

        // Register, confirm delivery, register again with same sequence
        tracker.register(b"msg_v1", link, 1, 1000);
        let hash1 = packet_hash(b"msg_v1");
        let mut trunc1 = [0u8; TRUNCATED_HASHBYTES];
        trunc1.copy_from_slice(&hash1[..TRUNCATED_HASHBYTES]);

        let seq = tracker.confirm_delivery(&trunc1);
        assert_eq!(seq, Some(Some(1)));
        assert_eq!(tracker.len(), 0);

        // Re-register same sequence (after wraparound or new message)
        tracker.register(b"msg_v2", link, 1, 2000);
        assert_eq!(tracker.len(), 1);

        let hash2 = packet_hash(b"msg_v2");
        let mut trunc2 = [0u8; TRUNCATED_HASHBYTES];
        trunc2.copy_from_slice(&hash2[..TRUNCATED_HASHBYTES]);
        assert!(tracker.contains_truncated(&trunc2));
    }
}
