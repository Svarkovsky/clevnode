//! Node events - unified event system for all node operations
//!
//! This module provides a unified [`NodeEvent`] enum that combines events from
//! transport, link management, and channels into a single stream, simplifying
//! event handling for application developers.

use alloc::string::String;
use alloc::vec::Vec;

use crate::announce::ReceivedAnnounce;
use crate::constants::TRUNCATED_HASHBYTES;
use crate::destination::DestinationHash;
use crate::link::{LinkCloseReason, LinkId};

/// Why the driver destroyed frames bound to an interface (#25).
///
/// Both variants mean the same thing to a sender: **these bytes are gone; re-send
/// on a fresh link.** They are distinguished so an operator can tell an in-flight
/// dispatch failure from the purge of an already-queued backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameDropReason {
    /// `dispatch_actions` returned `InterfaceError::Disconnected` for this frame —
    /// the interface died with the frame in flight.
    DispatchDisconnected,
    /// The interface's retry queue was purged when the interface was removed.
    /// These frames had been queued for a later drain that will never come.
    RetryQueuePurged,
}

/// Unified event enum for all node operations
///
/// This combines events from transport, link management, and channels into a
/// single stream that applications can handle uniformly.
///
/// `Clone` exists for secondary observers (the std driver's event tap feeds
/// subscribers clones at its dispatch layer); the primary event path still
/// moves events, never copies them.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum NodeEvent {
    // Path Discovery Events
    /// A new announce was received and validated
    AnnounceReceived {
        /// Parsed announce data
        announce: ReceivedAnnounce,
        /// Interface it arrived on
        interface_index: usize,
    },

    /// Path to a destination was found (from announce)
    PathFound {
        /// Destination hash
        destination_hash: DestinationHash,
        /// Number of hops
        hops: u8,
        /// Interface index
        interface_index: usize,
    },

    /// A remote node requested the path to one of our local destinations
    ///
    /// This is informational, the transport layer already handles
    /// auto-re-announce internally (Transport.py:1843-1853).
    /// No application action is required.
    PathRequestReceived {
        /// The destination hash that was requested
        destination_hash: DestinationHash,
    },

    /// Path to a destination expired
    PathLost {
        /// Destination hash
        destination_hash: DestinationHash,
    },

    // Single-Packet Events
    /// Incoming single-packet data (not via a Link)
    PacketReceived {
        /// The destination hash that received this packet
        destination: DestinationHash,
        /// The decrypted data
        data: Vec<u8>,
        /// Interface it arrived on
        interface_index: usize,
    },

    /// Delivery confirmation for a sent packet
    PacketDeliveryConfirmed {
        /// The packet hash identifying the sent packet
        packet_hash: [u8; TRUNCATED_HASHBYTES],
    },

    /// Delivery failed for a sent packet
    DeliveryFailed {
        /// The packet hash identifying the sent packet
        packet_hash: [u8; TRUNCATED_HASHBYTES],
        /// The error that occurred
        error: DeliveryError,
    },

    // Link Events
    /// Link established (handshake completed)
    LinkEstablished {
        /// The link ID
        link_id: LinkId,
        /// Whether we initiated this link
        is_initiator: bool,
        /// The local destination this link was established for. Lets apps that
        /// register several destinations route an inbound link to the right one
        /// (mirrors the `destination_hash` on `LinkClosed`).
        destination_hash: DestinationHash,
    },

    /// Message received on a link via the Channel multiplexer
    ///
    /// Emitted when the peer sends a channel message (with `PacketContext::Channel`).
    /// Most link-based applications use this variant for structured message exchange.
    MessageReceived {
        /// The link ID
        link_id: LinkId,
        /// Message type identifier
        msgtype: u16,
        /// Message sequence number
        sequence: u16,
        /// The message data
        data: Vec<u8>,
    },

    /// Raw data received on a link without Channel framing
    ///
    /// Emitted when the peer sends a plain link data packet (not via Channel).
    /// This is the lower-level variant, use [`MessageReceived`](NodeEvent::MessageReceived)
    /// for channel-multiplexed messaging.
    LinkDataReceived {
        /// The link ID
        link_id: LinkId,
        /// The decrypted data
        data: Vec<u8>,
    },

    /// Link became stale (no activity for too long)
    LinkStale {
        /// The link ID
        link_id: LinkId,
    },

    /// Link recovered from stale state (traffic resumed)
    LinkRecovered {
        /// The link ID
        link_id: LinkId,
    },

    /// Observability event, a channel message was retransmitted due to timeout.
    ///
    /// No application action is required. Useful for logging and diagnostics.
    ChannelRetransmit {
        /// The link ID
        link_id: LinkId,
        /// Message sequence number
        sequence: u16,
        /// Retry attempt number (2 = first retry, etc.)
        tries: u8,
    },

    /// The remote peer has proven their identity on a link.
    ///
    /// Emitted on the responder (non-initiator) side when the initiator sends
    /// a valid LINKIDENTIFY packet. The full identity can be queried via
    /// `get_remote_identity(link_id)`.
    LinkIdentified {
        /// The link on which identification occurred
        link_id: LinkId,
        /// Truncated hash of the identified identity (16 bytes)
        identity_hash: [u8; TRUNCATED_HASHBYTES],
    },

    /// Link closed
    LinkClosed {
        /// The link ID
        link_id: LinkId,
        /// Why the link was closed
        reason: LinkCloseReason,
        /// Whether we initiated this link
        is_initiator: bool,
        /// The destination hash this link was to
        destination_hash: DestinationHash,
    },

    // Proof Events
    /// Application should decide whether to prove this packet
    ///
    /// Emitted when a packet is received at a destination with `ProofStrategy::App`.
    /// Call `NodeCore::send_proof()` if the application decides to prove delivery.
    /// Not emitted for `ProofStrategy::All` (handled automatically by the library).
    PacketProofRequested {
        /// Full SHA256 hash of the packet to potentially prove
        packet_hash: [u8; 32],
        /// Destination that received the packet
        destination_hash: DestinationHash,
        /// Interface the packet was received on
        interface_index: usize,
        /// The peer link the packet arrived through, when the interface
        /// named one (Codeberg #376). Hand it back together with
        /// `interface_index` to
        /// [`NodeCore::send_proof_on_peer`](crate::node::NodeCore::send_proof_on_peer)
        /// so the proof reaches that one neighbour instead of every live
        /// link; `None` on every single-peer interface.
        peer: Option<[u8; TRUNCATED_HASHBYTES]>,
    },

    /// Application should decide whether to prove this link data packet
    ///
    /// Emitted when data is received on a link whose destination has
    /// `ProofStrategy::App`. Call `send_data_proof()` to confirm delivery.
    LinkProofRequested {
        /// The link that received the data
        link_id: LinkId,
        /// Full SHA256 hash of the packet to potentially prove
        packet_hash: [u8; 32],
    },

    /// Delivery confirmation for a link data packet (PROVE_ALL)
    ///
    /// Emitted when a proof is received for a data packet sent on a link.
    /// This confirms the peer received and decrypted the data.
    LinkDeliveryConfirmed {
        /// The link that sent the data
        link_id: LinkId,
        /// Full SHA256 hash of the delivered packet
        packet_hash: [u8; 32],
    },

    /// Delivery proof timeout for a plain link data packet.
    ///
    /// The deadline is derived from the Link RTT in the same way as Python's
    /// `PacketReceipt`: `max(rtt * traffic_timeout_factor,
    /// TRAFFIC_TIMEOUT_MIN_MS)`.
    LinkDeliveryFailed {
        /// The link that sent the data
        link_id: LinkId,
        /// Full SHA256 hash of the packet whose receipt timed out
        packet_hash: [u8; 32],
    },

    // Resource Events
    /// Resource advertisement received (for AcceptApp strategy).
    /// Application should call `accept_resource()` or `reject_resource()`.
    ResourceAdvertised {
        /// The link that received the advertisement
        link_id: LinkId,
        /// Hash identifying this resource
        resource_hash: [u8; 32],
        /// Total encrypted transfer size
        transfer_size: u64,
        /// Original uncompressed data size
        data_size: u64,
    },

    /// Resource transfer started (receiver accepted, first REQ sent).
    ResourceTransferStarted {
        /// The link carrying the transfer
        link_id: LinkId,
        /// Hash identifying this resource
        resource_hash: [u8; 32],
        /// True if we are the sender, false if receiver
        is_sender: bool,
    },

    /// Progress update during resource transfer.
    /// Emitted each time a part is sent (sender) or a REQ is sent (receiver).
    ResourceProgress {
        /// The link carrying the transfer
        link_id: LinkId,
        /// Hash identifying this resource
        resource_hash: [u8; 32],
        /// Progress as a fraction 0.0..1.0
        progress: f32,
        /// Total encrypted transfer size in bytes
        transfer_size: u64,
        /// Original uncompressed data size in bytes
        data_size: u64,
        /// True if we are the sender
        is_sender: bool,
    },

    /// Resource transfer completed successfully.
    ///
    /// For multi-segment resources, this fires once per segment.
    /// `segment_index` and `total_segments` indicate position within
    /// the overall transfer. Metadata is only present in segment 1.
    ResourceCompleted {
        /// The link that carried the transfer
        link_id: LinkId,
        /// Hash identifying this resource
        resource_hash: [u8; 32],
        /// Assembled data (receiver only; empty Vec for sender)
        data: Vec<u8>,
        /// Extracted metadata (receiver only; None for sender).
        /// Contains raw msgpack-encoded bytes as received on the wire.
        /// Decode with a msgpack library to obtain the original value.
        /// Only present in segment 1 of multi-segment transfers.
        metadata: Option<Vec<u8>>,
        /// True if we were the sender
        is_sender: bool,
        /// Segment index (1-based)
        segment_index: u32,
        /// Total number of segments
        total_segments: u32,
    },

    /// Resource transfer failed.
    ResourceFailed {
        /// The link that carried the transfer
        link_id: LinkId,
        /// Hash identifying this resource
        resource_hash: [u8; 32],
        /// The error that caused the failure
        error: crate::resource::ResourceError,
        /// True if we were the sender
        is_sender: bool,
    },

    // Request/Response Events
    /// Request received on a link for a registered handler.
    /// Call `send_response()` with the request_id to reply.
    RequestReceived {
        /// The link that received the request
        link_id: LinkId,
        /// The destination this request was addressed to. Two destinations
        /// may share the same request path (see `register_request_handler`);
        /// this field tells the responder which one to serve.
        destination_hash: DestinationHash,
        /// Unique request identifier (truncated packet hash)
        request_id: [u8; TRUNCATED_HASHBYTES],
        /// The request path string
        path: String,
        /// Truncated hash of the path
        path_hash: [u8; TRUNCATED_HASHBYTES],
        /// Raw msgpack-encoded request data (or empty for nil)
        data: Vec<u8>,
        /// Timestamp from requester (seconds since epoch)
        requested_at: f64,
    },

    /// Response received for a previously sent request.
    ResponseReceived {
        /// The link that received the response
        link_id: LinkId,
        /// The request identifier matching the original request
        request_id: [u8; TRUNCATED_HASHBYTES],
        /// Raw msgpack-encoded response data
        response_data: Vec<u8>,
        /// Resource metadata from a file response (the msgpack
        /// `{"name": ...}` blob), `None` for non-file responses.
        metadata: Option<Vec<u8>>,
    },

    /// Pending request timed out without receiving a response.
    RequestTimedOut {
        /// The link the request was sent on
        link_id: LinkId,
        /// The request identifier that timed out
        request_id: [u8; TRUNCATED_HASHBYTES],
    },

    // Interface Events
    /// An interface went offline
    InterfaceDown(usize),

    /// Frames were **destroyed** because their interface died (#25).
    ///
    /// The driver cannot re-home these bytes: a queued/in-flight frame is bound
    /// to the interface it was dispatched on (and, for link traffic, to the link
    /// that died with it). They are gone, and the SENDER must re-send on a fresh
    /// link. Emitting this is what makes the loss observable above the driver —
    /// without it, a consumer's (correct) retry/backoff path is never told the
    /// dispatch failed and so can never engage, which is exactly how #25's field
    /// trace lost 8 frames invisibly across three repos.
    ///
    /// For an accept-only server talking to a NAT'd initiator, interface
    /// replacement is the STEADY STATE (NAT rebinding, ~60 s) — so this event is
    /// load-bearing for reliable delivery to any mobile peer, not an edge case.
    FramesDropped {
        /// The interface whose death destroyed the frames.
        iface_id: usize,
        /// How many frames were destroyed.
        count: usize,
        /// Why they were destroyed.
        reason: FrameDropReason,
    },

    // Control-plane backpressure signalling (Codeberg #71)
    /// One or more control-plane events were dropped because the bounded
    /// control channel was full.
    ///
    /// The std driver splits node events into a lossless-by-default control
    /// plane and a droppable data plane. When the control channel overflows,
    /// the lost events cannot be recovered, but the loss is never silent:
    /// the driver counts the drops and emits this single marker as soon as
    /// the control channel has room again. `dropped_count` is the number of
    /// control events lost since the previous marker. The marker itself is
    /// only enqueued when there is room, so it is never dropped.
    ControlPlaneOverflow {
        /// Number of control-plane events dropped since the last marker.
        dropped_count: u64,
    },

    // In-driver core processor (Codeberg #196)
    /// The registered in-driver core processor panicked and has been detached.
    ///
    /// The std driver can run a consumer-supplied synchronous state machine
    /// inside its own tick. That code is third-party, and before this event
    /// existed a panic in it unwound through the driver's event loop and killed
    /// the node — while the node kept its event-channel senders alive, so a
    /// consumer awaiting the event stream never saw closure and waited forever.
    ///
    /// The driver now catches the unwind, drops the processor, and continues
    /// serving the mesh without it. Every core-driving job the processor owned
    /// (for LXMF: message delivery) has stopped; the node itself has not.
    /// Re-registration is builder-only, so recovering means building a new
    /// node.
    ///
    /// `hook` names which method panicked, `on_event` or `on_tick`.
    CoreProcessorPanicked {
        /// The processor method that panicked: `"on_event"` or `"on_tick"`.
        hook: &'static str,
    },
}

/// Delivery plane a [`NodeEvent`] belongs to.
///
/// The std driver (Codeberg #71) carries events on two independent bounded
/// channels: a [`Control`](EventClass::Control) plane that is lossless until
/// its channel overflows (and surfaces any overflow via
/// [`NodeEvent::ControlPlaneOverflow`]), and a [`Data`](EventClass::Data)
/// plane that drops silently under load as normal backpressure. Embedded
/// (core-only) builds never construct the channels; this classification is
/// the single source of truth they share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventClass {
    /// Never dropped silently. Discovery- and lifecycle-critical events.
    Control,
    /// May be dropped under load (backpressure). Bulk data delivery.
    Data,
}

impl NodeEvent {
    /// Mutable access to the event's link id, if it carries one.
    ///
    /// Codeberg #66: link-establishment retries re-key a link's wire id;
    /// the event drain rewrites ids back to the caller-visible original
    /// so applications can correlate every event with the id they got
    /// from `connect()`. Exhaustive match (no wildcard) so a new variant
    /// forces an explicit decision here.
    pub(crate) fn link_id_mut(&mut self) -> Option<&mut crate::link::LinkId> {
        match self {
            NodeEvent::LinkEstablished { link_id, .. }
            | NodeEvent::MessageReceived { link_id, .. }
            | NodeEvent::LinkDataReceived { link_id, .. }
            | NodeEvent::LinkStale { link_id, .. }
            | NodeEvent::LinkRecovered { link_id, .. }
            | NodeEvent::ChannelRetransmit { link_id, .. }
            | NodeEvent::LinkIdentified { link_id, .. }
            | NodeEvent::LinkClosed { link_id, .. }
            | NodeEvent::LinkProofRequested { link_id, .. }
            | NodeEvent::LinkDeliveryConfirmed { link_id, .. }
            | NodeEvent::LinkDeliveryFailed { link_id, .. }
            | NodeEvent::ResourceAdvertised { link_id, .. }
            | NodeEvent::ResourceTransferStarted { link_id, .. }
            | NodeEvent::ResourceProgress { link_id, .. }
            | NodeEvent::ResourceCompleted { link_id, .. }
            | NodeEvent::ResourceFailed { link_id, .. }
            | NodeEvent::RequestReceived { link_id, .. }
            | NodeEvent::ResponseReceived { link_id, .. }
            | NodeEvent::RequestTimedOut { link_id, .. } => Some(link_id),
            NodeEvent::AnnounceReceived { .. }
            | NodeEvent::PathFound { .. }
            | NodeEvent::PathRequestReceived { .. }
            | NodeEvent::PathLost { .. }
            | NodeEvent::PacketReceived { .. }
            | NodeEvent::PacketDeliveryConfirmed { .. }
            | NodeEvent::DeliveryFailed { .. }
            | NodeEvent::PacketProofRequested { .. }
            | NodeEvent::ControlPlaneOverflow { .. }
            | NodeEvent::CoreProcessorPanicked { .. }
            | NodeEvent::InterfaceDown(_)
            | NodeEvent::FramesDropped { .. } => None,
        }
    }

    /// Classify this event for the split control/data event channels
    /// (Codeberg #71).
    ///
    /// The rule, by volume rather than by importance:
    ///
    /// * CONTROL — the low-volume mesh, lifecycle, and outcome events: at most
    ///   one per link, transfer, or request, and discovery- or
    ///   control-critical. These are never silently dropped, so the control
    ///   plane must stay low-volume to keep that guarantee meaningful.
    /// * DATA — the high-volume or payload-bearing events (per-packet,
    ///   per-chunk, stream-volume). Their loss under extreme load is
    ///   backpressure, not data loss: reliability of the actual data is owned
    ///   by the lower layers (link acks, resource retransmit, channel
    ///   sequencing), not by this notification channel.
    ///
    /// The match is exhaustive (no wildcard) so adding a `NodeEvent` variant
    /// forces an explicit decision here rather than defaulting silently.
    pub fn event_class(&self) -> EventClass {
        match self {
            // Path discovery — losing one stalls discovery (the #71 bug).
            NodeEvent::AnnounceReceived { .. }
            | NodeEvent::PathFound { .. }
            | NodeEvent::PathRequestReceived { .. }
            | NodeEvent::PathLost { .. } => EventClass::Control,

            // High-volume / payload-bearing — droppable under load
            // (backpressure is the point; lower layers own reliability).
            // Single-packet delivery and its confirmations, link-borne stream
            // payloads and their per-packet confirmation, the per-chunk
            // resource progress tick, and the disposable retransmit notice.
            NodeEvent::PacketReceived { .. }
            | NodeEvent::PacketDeliveryConfirmed { .. }
            | NodeEvent::DeliveryFailed { .. }
            | NodeEvent::MessageReceived { .. }
            | NodeEvent::LinkDataReceived { .. }
            | NodeEvent::LinkDeliveryConfirmed { .. }
            | NodeEvent::LinkDeliveryFailed { .. }
            | NodeEvent::ChannelRetransmit { .. }
            | NodeEvent::ResourceProgress { .. } => EventClass::Data,

            // Link lifecycle and identity — at most one per link, must not be
            // lost or links wedge.
            NodeEvent::LinkEstablished { .. }
            | NodeEvent::LinkStale { .. }
            | NodeEvent::LinkRecovered { .. }
            | NodeEvent::LinkIdentified { .. }
            | NodeEvent::LinkClosed { .. } => EventClass::Control,

            // Proof decisions — require an application call to make progress.
            NodeEvent::PacketProofRequested { .. } | NodeEvent::LinkProofRequested { .. } => {
                EventClass::Control
            }

            // Resource transfers — advertise/started gate the transfer,
            // completed/failed carry the outcome. One per transfer (or
            // per segment) and control-critical: a dropped completion loses
            // the outcome. The high-volume per-chunk progress tick is DATA
            // (classified above).
            NodeEvent::ResourceAdvertised { .. }
            | NodeEvent::ResourceTransferStarted { .. }
            | NodeEvent::ResourceCompleted { .. }
            | NodeEvent::ResourceFailed { .. } => EventClass::Control,

            // Request/response — RequestReceived needs a reply, the rest carry
            // correlation state. One per request. CONTROL.
            NodeEvent::RequestReceived { .. }
            | NodeEvent::ResponseReceived { .. }
            | NodeEvent::RequestTimedOut { .. } => EventClass::Control,

            // Interface lifecycle, the overflow marker itself, and the
            // notice that the in-driver processor died. The last one is at
            // most once per node lifetime and is the only way a consumer
            // learns its state machine stopped: losing it silently would be
            // the #196 defect it exists to report.
            NodeEvent::InterfaceDown(_)
            | NodeEvent::FramesDropped { .. }
            | NodeEvent::ControlPlaneOverflow { .. }
            | NodeEvent::CoreProcessorPanicked { .. } => EventClass::Control,
        }
    }

    /// Stable string identifier for the event variant.
    ///
    /// Used as a scalar token in structured tracing fields where a full
    /// `Debug` rendering would break Stage-6 event-log line tokenisation
    /// (whitespace) or expose volatile internal data. The returned string
    /// has no whitespace and is stable across releases — adding a new
    /// `NodeEvent` variant requires extending the match.
    pub fn variant_name(&self) -> &'static str {
        match self {
            NodeEvent::AnnounceReceived { .. } => "AnnounceReceived",
            NodeEvent::PathFound { .. } => "PathFound",
            NodeEvent::PathRequestReceived { .. } => "PathRequestReceived",
            NodeEvent::PathLost { .. } => "PathLost",
            NodeEvent::PacketReceived { .. } => "PacketReceived",
            NodeEvent::PacketDeliveryConfirmed { .. } => "PacketDeliveryConfirmed",
            NodeEvent::DeliveryFailed { .. } => "DeliveryFailed",
            NodeEvent::LinkEstablished { .. } => "LinkEstablished",
            NodeEvent::MessageReceived { .. } => "MessageReceived",
            NodeEvent::LinkDataReceived { .. } => "LinkDataReceived",
            NodeEvent::LinkStale { .. } => "LinkStale",
            NodeEvent::LinkRecovered { .. } => "LinkRecovered",
            NodeEvent::ChannelRetransmit { .. } => "ChannelRetransmit",
            NodeEvent::LinkIdentified { .. } => "LinkIdentified",
            NodeEvent::LinkClosed { .. } => "LinkClosed",
            NodeEvent::PacketProofRequested { .. } => "PacketProofRequested",
            NodeEvent::LinkProofRequested { .. } => "LinkProofRequested",
            NodeEvent::LinkDeliveryConfirmed { .. } => "LinkDeliveryConfirmed",
            NodeEvent::LinkDeliveryFailed { .. } => "LinkDeliveryFailed",
            NodeEvent::ResourceAdvertised { .. } => "ResourceAdvertised",
            NodeEvent::ResourceTransferStarted { .. } => "ResourceTransferStarted",
            NodeEvent::ResourceProgress { .. } => "ResourceProgress",
            NodeEvent::ResourceCompleted { .. } => "ResourceCompleted",
            NodeEvent::ResourceFailed { .. } => "ResourceFailed",
            NodeEvent::RequestReceived { .. } => "RequestReceived",
            NodeEvent::ResponseReceived { .. } => "ResponseReceived",
            NodeEvent::RequestTimedOut { .. } => "RequestTimedOut",
            NodeEvent::InterfaceDown(_) => "InterfaceDown",
            NodeEvent::FramesDropped { .. } => "FramesDropped",
            NodeEvent::ControlPlaneOverflow { .. } => "ControlPlaneOverflow",
            NodeEvent::CoreProcessorPanicked { .. } => "CoreProcessorPanicked",
        }
    }
}

/// Reason why a delivery failed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeliveryError {
    /// Delivery timed out without proof
    Timeout,
    /// Link failed during delivery
    LinkFailed,
    /// A proof for the packet arrived but did not verify against the
    /// destination's identity.
    ///
    /// Distinct from [`Timeout`](DeliveryError::Timeout) and
    /// [`LinkFailed`](DeliveryError::LinkFailed) in what it asks of the sender:
    /// the peer answered, so the path works and re-sending on it will keep
    /// producing the same unverifiable proof. Either the recalled identity for
    /// the destination is stale (re-resolve it) or something on the path is
    /// forging proofs. Reported as `LinkFailed` until this variant existed,
    /// which told a caller to retry a send that cannot succeed.
    InvalidProof,
}

impl core::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DeliveryError::Timeout => write!(f, "delivery timed out"),
            DeliveryError::LinkFailed => write!(f, "link failed during delivery"),
            DeliveryError::InvalidProof => write!(f, "delivery proof did not verify"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delivery_error_variants() {
        // Ensure all variants are copyable
        let err = DeliveryError::Timeout;
        let err2 = err;
        assert_eq!(err, err2);
    }

    #[test]
    fn variant_name_no_whitespace() {
        // Stage-6 event-log fields tokenise on whitespace; the
        // dropped_event_type field carries this string verbatim, so no
        // variant_name may contain whitespace, '=', or non-printable
        // bytes. A constructable sample of every variant would need
        // every payload type — instead we sanity-check the obvious
        // ones plus the contract on a unit-payload variant that is
        // always constructable.
        let e = NodeEvent::InterfaceDown(0);
        let name = e.variant_name();
        assert_eq!(name, "InterfaceDown");
        assert!(!name.contains(char::is_whitespace));
        assert!(!name.contains('='));
        assert!(name.chars().all(|c| !c.is_control()));
    }
}
