//! Sans-I/O integration between LXMF delivery and [`leviculum_core::NodeCore`].
//!
//! LXMF deliberately uses raw Reticulum packets on both SINGLE destinations
//! and Links. It does not use the Link Channel multiplexer. Small direct
//! messages therefore go through `NodeCore::send_packet_on_link`, while large
//! direct messages use the Resource engine.
//!
//! This adapter owns only LXMF-specific correlation state. Network I/O remains
//! represented by [`TickOutput`], exactly as it is in `leviculum-core`.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use leviculum_core::resource::{ResourceError, ResourceStrategy, RESOURCE_MAX_EFFICIENT_SIZE};
use leviculum_core::{
    Clock, DeliveryError, Destination, DestinationError, DestinationHash, DestinationType,
    Direction, Identity, LinkCloseReason, LinkId, NodeCore, NodeEvent, ProofStrategy, SendError,
    Storage, TickOutput,
};
use rand_core::CryptoRngCore;

use crate::constants::{
    DELIVERY_LIMIT_BYTES, DESTINATION_LENGTH, ENCRYPTED_PACKET_MAX_CONTENT,
    LINK_PACKET_MAX_CONTENT, LXMF_OVERHEAD,
};
use crate::{DeliveryMethod, Message, MessageError};

/// LXMF Reticulum application name.
pub const APP_NAME: &str = "lxmf";

/// Aspect used for an LXMF delivery destination.
pub const DELIVERY_ASPECT: &str = "delivery";

/// Largest opportunistic on-air plaintext accepted by Python LXMF.
pub const OPPORTUNISTIC_PACKET_MDU: usize =
    ENCRYPTED_PACKET_MAX_CONTENT + LXMF_OVERHEAD - DESTINATION_LENGTH;

/// Largest fully packed LXMF message sent as one raw Link packet.
pub const DIRECT_PACKET_MDU: usize = LINK_PACKET_MAX_CONTENT + LXMF_OVERHEAD;

/// Runtime policy for the NodeCore adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LxmfNodeConfig {
    /// Maximum uncompressed size of one incoming LXMF Resource, in bytes.
    ///
    /// Python's default is not unlimited: `delivery_resource_advertised`
    /// (`reference/LXMF/LXMF/LXMRouter.py:1977`) refuses an advertised
    /// delivery Resource strictly above `delivery_per_transfer_limit*1000`
    /// bytes — 1 000 000 with the default
    /// [`DELIVERY_LIMIT_KB`](crate::constants::DELIVERY_LIMIT_KB). Exactly at
    /// the limit is accepted, matching the reference's `size > limit`.
    /// Embedded applications should set this as well as NodeCore's global
    /// incoming Resource limit.
    pub max_incoming_resource_size: Option<u64>,
    /// Maximum packed size of one outgoing delivery, in bytes.
    ///
    /// A Python peer refuses an advertised delivery Resource strictly above
    /// its own limit before a byte of payload moves, so packing, compressing,
    /// encrypting and advertising an over-limit message is pure waste. The
    /// send paths refuse such a message up front with
    /// [`LxmfNodeError::DeliveryLimitExceeded`]. The measure is the packed
    /// message size — what the receiver's `resource.get_data_size()` reports,
    /// since the reference hands `self.packed` to the Resource uncompressed
    /// (`reference/LXMF/LXMF/LXMessage.py:654`) and the advertised data size
    /// is the pre-compression size.
    pub max_outgoing_delivery_size: Option<u64>,
    /// Ask Reticulum's Resource engine to use BZ2 when beneficial.
    pub auto_compress_resources: bool,
}

impl Default for LxmfNodeConfig {
    fn default() -> Self {
        Self {
            max_incoming_resource_size: Some(DELIVERY_LIMIT_BYTES),
            max_outgoing_delivery_size: Some(DELIVERY_LIMIT_BYTES),
            auto_compress_resources: true,
        }
    }
}

/// Reticulum representation selected for an LXMF message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryRepresentation {
    OpportunisticPacket,
    DirectPacket,
    DirectResource,
}

/// Stable identifier returned by the underlying Reticulum send operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionId {
    /// Truncated packet receipt hash used by SINGLE packet delivery.
    Packet([u8; 16]),
    /// Full packet hash used by raw Link data proofs.
    LinkPacket([u8; 32]),
    /// Resource hash returned when the transfer is created.
    Resource([u8; 32]),
}

/// State returned while preparing a direct link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectLinkState {
    Ready(LinkId),
    /// A link request was started by this call.
    Started(LinkId),
    /// A link request started by an earlier call is still pending.
    Connecting(LinkId),
    PathRequested,
}

/// Why an outbound message did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFailure {
    Opportunistic(DeliveryError),
    DirectPacketTimeout,
    LinkClosed(LinkCloseReason),
    Resource(ResourceError),
}

/// Why an inbound transport payload was not emitted as an LXMF message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundRejection {
    Message(MessageError),
    WrongDestination,
    ResourceSequence,
    ResourceTooLarge,
    Resource(ResourceError),
}

impl core::fmt::Display for DeliveryFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Opportunistic(error) => write!(f, "opportunistic delivery: {error}"),
            Self::DirectPacketTimeout => write!(f, "direct packet was not acknowledged"),
            // `LinkCloseReason` has no `Display` in leviculum-core; its
            // variant name is the whole reason.
            Self::LinkClosed(reason) => write!(f, "delivery link closed ({reason:?})"),
            Self::Resource(error) => write!(f, "delivery transfer: {error}"),
        }
    }
}

impl core::fmt::Display for InboundRejection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Message(error) => write!(f, "inbound message: {error}"),
            Self::WrongDestination => write!(f, "inbound message is for another destination"),
            Self::ResourceSequence => write!(f, "inbound transfer arrived out of sequence"),
            Self::ResourceTooLarge => write!(f, "inbound transfer exceeds the accepted size"),
            Self::Resource(error) => write!(f, "inbound transfer: {error}"),
        }
    }
}

/// LXMF-level events derived from [`NodeEvent`] values.
#[derive(Debug, Clone, PartialEq)]
pub enum LxmfNodeEvent {
    PeerAnnounced {
        destination: DestinationHash,
    },
    PathRequested {
        destination: DestinationHash,
    },
    DirectLinkEstablished {
        destination: Option<DestinationHash>,
        link_id: LinkId,
        is_initiator: bool,
    },
    Submitted {
        message_id: [u8; 32],
        method: DeliveryMethod,
        representation: DeliveryRepresentation,
        submission: SubmissionId,
    },
    Progress {
        message_id: [u8; 32],
        progress: f32,
    },
    Delivered {
        message_id: [u8; 32],
    },
    DeliveryFailed {
        message_id: [u8; 32],
        reason: DeliveryFailure,
    },
    MessageReceived(Message),
    InboundRejected {
        method: DeliveryMethod,
        reason: InboundRejection,
    },
}

/// Combined output of one LXMF adapter operation.
#[derive(Debug, Default)]
#[must_use = "LXMF output contains NodeCore actions and application events"]
pub struct LxmfNodeOutput {
    pub core: TickOutput,
    pub events: Vec<LxmfNodeEvent>,
}

impl LxmfNodeOutput {
    pub fn merge(&mut self, other: Self) {
        self.core.merge(other.core);
        self.events.extend(other.events);
    }
}

/// Link-derived inputs for one resource send, captured under a brief borrow by
/// [`LxmfNode::resource_send_params`].
pub struct LxmfResourceSendParams {
    core: leviculum_core::resource::ResourceSendParams,
    link_id: LinkId,
    message_id: [u8; 32],
    /// [`LxmfNodeConfig::max_outgoing_delivery_size`] captured in phase 1, so
    /// the off-lock phase 2 can refuse an over-limit message before building.
    max_outgoing_delivery_size: Option<u64>,
}

/// A message packed and built into a transfer off the node, ready for
/// [`LxmfNode::commit_resource_send`].
pub struct PreparedLxmfSend {
    prepared: leviculum_core::resource::PreparedResourceSend,
    link_id: LinkId,
    message_id: [u8; 32],
    packed_len: usize,
}

/// Synchronous adapter errors. Inbound wire errors are reported as
/// [`LxmfNodeEvent::InboundRejected`] instead, so malformed network traffic
/// never aborts an event loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LxmfNodeError {
    InvalidDeliveryDestination,
    Destination(DestinationError),
    Message(MessageError),
    UnsupportedMethod,
    UnknownPeer,
    DirectLinkUnavailable,
    /// The packed message exceeds
    /// [`LxmfNodeConfig::max_outgoing_delivery_size`]; a Python peer would
    /// refuse the transfer at the advertisement, so nothing was packed into a
    /// transfer, compressed, encrypted or advertised.
    DeliveryLimitExceeded {
        size: u64,
        limit: u64,
    },
    Send(SendError),
    Resource(ResourceError),
    ProofFailed,
    IdentityUnavailable,
}

impl core::fmt::Display for LxmfNodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidDeliveryDestination => write!(f, "not an LXMF delivery destination"),
            Self::Destination(error) => write!(f, "destination: {error}"),
            Self::Message(error) => write!(f, "message: {error}"),
            Self::UnsupportedMethod => write!(f, "delivery method is not supported here"),
            Self::UnknownPeer => write!(f, "peer identity is unknown"),
            Self::DirectLinkUnavailable => write!(f, "no direct link to the peer"),
            Self::DeliveryLimitExceeded { size, limit } => write!(
                f,
                "packed message of {size} bytes exceeds the {limit} byte delivery transfer limit"
            ),
            Self::Send(error) => write!(f, "send: {error}"),
            Self::Resource(error) => write!(f, "resource: {error}"),
            Self::ProofFailed => write!(f, "delivery proof was not accepted"),
            Self::IdentityUnavailable => write!(f, "no identity for this destination"),
        }
    }
}

impl core::error::Error for LxmfNodeError {}

impl From<DestinationError> for LxmfNodeError {
    fn from(value: DestinationError) -> Self {
        Self::Destination(value)
    }
}

impl From<MessageError> for LxmfNodeError {
    fn from(value: MessageError) -> Self {
        Self::Message(value)
    }
}

impl From<SendError> for LxmfNodeError {
    fn from(value: SendError) -> Self {
        Self::Send(value)
    }
}

impl From<ResourceError> for LxmfNodeError {
    fn from(value: ResourceError) -> Self {
        Self::Resource(value)
    }
}

/// Refuse a packed message a Python peer would reject at the Resource
/// advertisement. Strictly greater refuses; exactly at the limit passes,
/// matching `delivery_resource_advertised`
/// (`reference/LXMF/LXMF/LXMRouter.py:1980`).
fn check_outgoing_delivery_size(
    limit: Option<u64>,
    packed_len: usize,
) -> Result<(), LxmfNodeError> {
    match limit {
        Some(limit) if packed_len as u64 > limit => Err(LxmfNodeError::DeliveryLimitExceeded {
            size: packed_len as u64,
            limit,
        }),
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingLinkPacket {
    message_id: [u8; 32],
    link_id: LinkId,
}

#[derive(Debug, Clone, Copy)]
struct PendingResource {
    message_id: [u8; 32],
    current_resource_hash: [u8; 32],
    completed_size: u64,
    current_size: u64,
    total_size: u64,
}

#[derive(Debug)]
struct IncomingResourceAssembly {
    next_segment: u32,
    total_segments: u32,
    data: Vec<u8>,
}

/// Read-only snapshot of an active incoming LXMF Resource transfer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IncomingResourceTransfer {
    pub link_id: LinkId,
    pub resource_hash: [u8; 32],
    pub transfer_size: u64,
    pub data_size: u64,
    pub progress: f32,
}

/// LXMF delivery state layered directly on a [`NodeCore`].
pub struct LxmfNode {
    delivery_destination: DestinationHash,
    config: LxmfNodeConfig,
    wanted_links: BTreeSet<DestinationHash>,
    pending_links: BTreeMap<DestinationHash, LinkId>,
    link_destinations: BTreeMap<LinkId, DestinationHash>,
    direct_links: BTreeMap<DestinationHash, LinkId>,
    lxmf_links: BTreeSet<LinkId>,
    identified_links: BTreeSet<LinkId>,
    packet_receipts: BTreeMap<[u8; 16], [u8; 32]>,
    link_packet_receipts: BTreeMap<[u8; 32], PendingLinkPacket>,
    resources: BTreeMap<LinkId, PendingResource>,
    incoming_resources: BTreeMap<LinkId, IncomingResourceAssembly>,
    incoming_resource_transfers: BTreeMap<[u8; 32], IncomingResourceTransfer>,
}

impl LxmfNode {
    /// Construct a correctly named delivery destination. The caller can enable
    /// ratchets on the returned destination before registering it.
    pub fn delivery_destination(identity: Identity) -> Result<Destination, DestinationError> {
        let mut destination = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            APP_NAME,
            &[DELIVERY_ASPECT],
        )?;
        destination.set_proof_strategy(ProofStrategy::All);
        destination.set_accepts_links(true);
        Ok(destination)
    }

    /// Validate, configure and register an LXMF delivery destination.
    pub fn register<R, C, S>(
        node: &mut NodeCore<R, C, S>,
        mut destination: Destination,
        config: LxmfNodeConfig,
    ) -> Result<Self, LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if destination.direction() != Direction::In
            || destination.dest_type() != DestinationType::Single
            || destination.name_hash()
                != &Destination::compute_name_hash(APP_NAME, &[DELIVERY_ASPECT])
        {
            return Err(LxmfNodeError::InvalidDeliveryDestination);
        }

        destination.set_proof_strategy(ProofStrategy::All);
        destination.set_accepts_links(true);
        let delivery_destination = *destination.hash();
        node.register_destination(destination);
        Ok(Self::new(delivery_destination, config))
    }

    /// Attach to an already registered LXMF delivery destination.
    pub fn attach<R, C, S>(
        node: &mut NodeCore<R, C, S>,
        delivery_destination: DestinationHash,
        config: LxmfNodeConfig,
    ) -> Result<Self, LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let destination = node
            .destination_mut(&delivery_destination)
            .ok_or(LxmfNodeError::InvalidDeliveryDestination)?;
        if destination.direction() != Direction::In
            || destination.dest_type() != DestinationType::Single
            || destination.name_hash()
                != &Destination::compute_name_hash(APP_NAME, &[DELIVERY_ASPECT])
        {
            return Err(LxmfNodeError::InvalidDeliveryDestination);
        }
        destination.set_proof_strategy(ProofStrategy::All);
        destination.set_accepts_links(true);
        Ok(Self::new(delivery_destination, config))
    }

    fn new(delivery_destination: DestinationHash, config: LxmfNodeConfig) -> Self {
        Self {
            delivery_destination,
            config,
            wanted_links: BTreeSet::new(),
            pending_links: BTreeMap::new(),
            link_destinations: BTreeMap::new(),
            direct_links: BTreeMap::new(),
            lxmf_links: BTreeSet::new(),
            identified_links: BTreeSet::new(),
            packet_receipts: BTreeMap::new(),
            link_packet_receipts: BTreeMap::new(),
            resources: BTreeMap::new(),
            incoming_resources: BTreeMap::new(),
            incoming_resource_transfers: BTreeMap::new(),
        }
    }

    pub const fn delivery_destination_hash(&self) -> DestinationHash {
        self.delivery_destination
    }

    /// Return the active direct or identified backchannel Link for a peer.
    pub fn direct_link(&self, destination: &DestinationHash) -> Option<LinkId> {
        self.direct_links.get(destination).copied()
    }

    /// Return whether an established Link is owned by LXMF delivery.
    ///
    /// This becomes `true` when [`NodeEvent::LinkEstablished`] associates the
    /// Link with the local delivery destination or an outgoing direct-delivery
    /// attempt, and becomes `false` again when the Link closes.
    pub fn owns_link(&self, link_id: &LinkId) -> bool {
        self.lxmf_links.contains(link_id)
    }

    /// Return snapshots of active incoming Resources on LXMF delivery Links.
    ///
    /// Cancellation is intentionally not exposed here until `leviculum-core`
    /// can cancel an already accepted inbound Resource by hash.
    pub fn incoming_resource_transfers(&self) -> impl Iterator<Item = &IncomingResourceTransfer> {
        self.incoming_resource_transfers.values()
    }

    pub fn incoming_resource_count(&self) -> usize {
        self.incoming_resource_transfers.len()
    }

    pub fn config(&self) -> LxmfNodeConfig {
        self.config
    }

    /// Select the exact Python-compatible delivery representation.
    pub fn representation(message: &Message) -> Result<DeliveryRepresentation, LxmfNodeError> {
        Self::representation_of(message.method, message.pack().len())
    }

    /// [`representation`](Self::representation) against a length the caller
    /// already packed for, so a send does not pack the message twice.
    fn representation_of(
        method: DeliveryMethod,
        packed_len: usize,
    ) -> Result<DeliveryRepresentation, LxmfNodeError> {
        match method {
            DeliveryMethod::Opportunistic
                if packed_len.saturating_sub(DESTINATION_LENGTH) <= OPPORTUNISTIC_PACKET_MDU =>
            {
                Ok(DeliveryRepresentation::OpportunisticPacket)
            }
            DeliveryMethod::Opportunistic | DeliveryMethod::Direct
                if packed_len <= DIRECT_PACKET_MDU =>
            {
                Ok(DeliveryRepresentation::DirectPacket)
            }
            DeliveryMethod::Opportunistic | DeliveryMethod::Direct => {
                Ok(DeliveryRepresentation::DirectResource)
            }
            DeliveryMethod::Propagated | DeliveryMethod::Paper => {
                Err(LxmfNodeError::UnsupportedMethod)
            }
        }
    }

    /// Ensure that a direct link exists, requesting a path first when needed.
    pub fn ensure_direct_link<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        destination: DestinationHash,
    ) -> Result<(DirectLinkState, LxmfNodeOutput), LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if let Some(link_id) = self.direct_links.get(&destination).copied() {
            if node.link(&link_id).is_some_and(|link| link.is_active()) {
                return Ok((DirectLinkState::Ready(link_id), LxmfNodeOutput::default()));
            }
            self.direct_links.remove(&destination);
        }
        if let Some(link_id) = self.pending_links.get(&destination).copied() {
            return Ok((
                DirectLinkState::Connecting(link_id),
                LxmfNodeOutput::default(),
            ));
        }
        if node
            .storage()
            .get_identity(destination.as_bytes())
            .is_none()
        {
            return Err(LxmfNodeError::UnknownPeer);
        }

        self.wanted_links.insert(destination);
        if !node.has_path(&destination) {
            let mut output = LxmfNodeOutput {
                core: node.request_path(&destination),
                events: Vec::new(),
            };
            output
                .events
                .push(LxmfNodeEvent::PathRequested { destination });
            return Ok((DirectLinkState::PathRequested, output));
        }

        let (link_id, output) = self.start_direct_link(node, destination)?;
        Ok((DirectLinkState::Started(link_id), output))
    }

    fn start_direct_link<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        destination: DestinationHash,
    ) -> Result<(LinkId, LxmfNodeOutput), LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let signing_key = node
            .storage()
            .get_identity(destination.as_bytes())
            .map(|identity| identity.ed25519_verifying().to_bytes())
            .ok_or(LxmfNodeError::UnknownPeer)?;
        let (link_id, _routed, core) = node.connect(destination, &signing_key);
        self.pending_links.insert(destination, link_id);
        self.link_destinations.insert(link_id, destination);
        let output = LxmfNodeOutput {
            core,
            events: Vec::new(),
        };
        Ok((link_id, output))
    }

    /// Submit one opportunistic or direct message to NodeCore.
    ///
    /// Oversized opportunistic messages fall back to direct delivery, matching
    /// Python `LXMessage.pack()`. Call [`ensure_direct_link`](Self::ensure_direct_link)
    /// first when this method returns [`LxmfNodeError::DirectLinkUnavailable`].
    pub fn send<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        message: &Message,
    ) -> Result<LxmfNodeOutput, LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let packed = message.pack();
        check_outgoing_delivery_size(self.config.max_outgoing_delivery_size, packed.len())?;
        let representation = Self::representation_of(message.method, packed.len())?;
        let destination = DestinationHash::new(message.destination_hash);

        match representation {
            DeliveryRepresentation::OpportunisticPacket => {
                let data = message.on_air().map_err(LxmfNodeError::Message)?;
                let (packet_hash, core) = node.send_single_packet(&destination, &data)?;
                self.packet_receipts.insert(packet_hash, message.message_id);
                let output = LxmfNodeOutput {
                    core,
                    events: vec![LxmfNodeEvent::Submitted {
                        message_id: message.message_id,
                        method: DeliveryMethod::Opportunistic,
                        representation,
                        submission: SubmissionId::Packet(packet_hash),
                    }],
                };
                Ok(output)
            }
            DeliveryRepresentation::DirectPacket => {
                let link_id = self.active_direct_link(node, &destination)?;
                let (packet_hash, core) = node.send_packet_on_link(&link_id, &packed)?;
                self.link_packet_receipts.insert(
                    packet_hash,
                    PendingLinkPacket {
                        message_id: message.message_id,
                        link_id,
                    },
                );
                let output = LxmfNodeOutput {
                    core,
                    events: vec![LxmfNodeEvent::Submitted {
                        message_id: message.message_id,
                        method: DeliveryMethod::Direct,
                        representation,
                        submission: SubmissionId::LinkPacket(packet_hash),
                    }],
                };
                Ok(output)
            }
            DeliveryRepresentation::DirectResource => {
                let link_id = self.active_direct_link(node, &destination)?;
                let (resource_hash, core) = node.send_resource(
                    &link_id,
                    &packed,
                    None,
                    self.config.auto_compress_resources,
                )?;
                Ok(self.record_resource_submission(
                    link_id,
                    message.message_id,
                    resource_hash,
                    packed.len(),
                    core,
                ))
            }
        }
    }

    /// Phase 1 of the off-lock resource send: resolve the link and snapshot
    /// what [`prepare_resource_send`](Self::prepare_resource_send) needs.
    ///
    /// The caller must already know the message is a
    /// [`DeliveryRepresentation::DirectResource`] — deciding that packs, and
    /// packing belongs off the lock with the rest of the build.
    pub fn resource_send_params<R, C, S>(
        &mut self,
        node: &NodeCore<R, C, S>,
        message: &Message,
    ) -> Result<LxmfResourceSendParams, LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let destination = DestinationHash::new(message.destination_hash);
        let link_id = self.active_direct_link(node, &destination)?;
        Ok(LxmfResourceSendParams {
            core: node.resource_send_params(&link_id)?,
            link_id,
            message_id: message.message_id,
            max_outgoing_delivery_size: self.config.max_outgoing_delivery_size,
        })
    }

    /// Phase 2: pack and build the transfer without touching the node, so the
    /// cost that scales with the message runs off the caller's lock.
    pub fn prepare_resource_send(
        params: &LxmfResourceSendParams,
        message: &Message,
        auto_compress: bool,
        rng: &mut impl CryptoRngCore,
    ) -> Result<PreparedLxmfSend, LxmfNodeError> {
        let packed = message.pack();
        check_outgoing_delivery_size(params.max_outgoing_delivery_size, packed.len())?;
        let prepared = leviculum_core::resource::prepare_resource_send(
            &params.core,
            &packed,
            None,
            auto_compress,
            rng,
        )?;
        Ok(PreparedLxmfSend {
            prepared,
            link_id: params.link_id,
            message_id: params.message_id,
            packed_len: packed.len(),
        })
    }

    /// Phase 3: install the built transfer and emit its advertisement.
    ///
    /// Propagates [`ResourceError::LinkStateChanged`] when the link re-keyed
    /// while the build ran; the caller re-runs the three phases once, as the
    /// std driver does for its own resource sends.
    pub fn commit_resource_send<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        prepared: PreparedLxmfSend,
    ) -> Result<LxmfNodeOutput, LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let (resource_hash, core) = node.commit_resource_send(prepared.prepared)?;
        Ok(self.record_resource_submission(
            prepared.link_id,
            prepared.message_id,
            resource_hash,
            prepared.packed_len,
            core,
        ))
    }

    /// Track a submitted Resource and report it. Shared by the composed
    /// [`send`](Self::send) and phase 3 so the two cannot drift apart.
    fn record_resource_submission(
        &mut self,
        link_id: LinkId,
        message_id: [u8; 32],
        resource_hash: [u8; 32],
        packed_len: usize,
        core: TickOutput,
    ) -> LxmfNodeOutput {
        self.resources.insert(
            link_id,
            PendingResource {
                message_id,
                current_resource_hash: resource_hash,
                completed_size: 0,
                current_size: packed_len.min(RESOURCE_MAX_EFFICIENT_SIZE) as u64,
                total_size: packed_len as u64,
            },
        );
        LxmfNodeOutput {
            core,
            events: vec![LxmfNodeEvent::Submitted {
                message_id,
                method: DeliveryMethod::Direct,
                representation: DeliveryRepresentation::DirectResource,
                submission: SubmissionId::Resource(resource_hash),
            }],
        }
    }

    /// Stop tracking an outbound LXMF submission and abort an active Resource.
    ///
    /// Packets that have already left an interface cannot be recalled, but
    /// their proof correlation is removed immediately. A Resource is aborted
    /// by closing its Link, which makes the core emit the normal cancellation
    /// frames and prevents the remaining parts from being transmitted.
    pub fn cancel_outbound<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        message_id: &[u8; 32],
    ) -> TickOutput
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        self.packet_receipts
            .retain(|_, pending| pending != message_id);
        self.link_packet_receipts
            .retain(|_, pending| &pending.message_id != message_id);

        let resource_links: Vec<LinkId> = self
            .resources
            .iter()
            .filter_map(|(link_id, pending)| {
                (&pending.message_id == message_id).then_some(*link_id)
            })
            .collect();
        let mut output = TickOutput::default();
        for link_id in resource_links {
            self.resources.remove(&link_id);
            output.merge(node.close_link(&link_id));
        }
        output
    }

    fn active_direct_link<R, C, S>(
        &mut self,
        node: &NodeCore<R, C, S>,
        destination: &DestinationHash,
    ) -> Result<LinkId, LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let link_id = self
            .direct_links
            .get(destination)
            .copied()
            .ok_or(LxmfNodeError::DirectLinkUnavailable)?;
        if node.link(&link_id).is_some_and(|link| link.is_active()) {
            Ok(link_id)
        } else {
            self.direct_links.remove(destination);
            Err(LxmfNodeError::DirectLinkUnavailable)
        }
    }

    /// Consume one NodeCore event and return any follow-up NodeCore actions and
    /// LXMF-level events.
    pub fn handle_event<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        event: &NodeEvent,
    ) -> Result<LxmfNodeOutput, LxmfNodeError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let mut output = LxmfNodeOutput::default();
        match event {
            NodeEvent::AnnounceReceived { announce, .. }
                if announce.name_hash()
                    == &Destination::compute_name_hash(APP_NAME, &[DELIVERY_ASPECT]) =>
            {
                let destination = *announce.destination_hash();
                output
                    .events
                    .push(LxmfNodeEvent::PeerAnnounced { destination });

                if self.wanted_links.contains(&destination)
                    && !self.pending_links.contains_key(&destination)
                    && node.has_path(&destination)
                {
                    let (_, follow_up) = self.start_direct_link(node, destination)?;
                    output.merge(follow_up);
                }
            }
            NodeEvent::PathFound {
                destination_hash, ..
            } if self.wanted_links.contains(destination_hash)
                && !self.pending_links.contains_key(destination_hash) =>
            {
                let (_, follow_up) = self.start_direct_link(node, *destination_hash)?;
                output.merge(follow_up);
            }
            NodeEvent::LinkEstablished {
                link_id,
                is_initiator,
                ..
            } => {
                let link_destination = node.link(link_id).map(|link| *link.destination_hash());
                let tracked_destination = self.link_destinations.get(link_id).copied();
                let is_delivery_link = link_destination == Some(self.delivery_destination)
                    || tracked_destination.is_some();
                if is_delivery_link {
                    self.lxmf_links.insert(*link_id);
                    node.set_resource_strategy(link_id, ResourceStrategy::AcceptApp)?;

                    let destination = if *is_initiator {
                        tracked_destination.or(link_destination)
                    } else {
                        None
                    };
                    if let Some(destination) = destination {
                        self.pending_links.remove(&destination);
                        self.direct_links.insert(destination, *link_id);
                        self.link_destinations.insert(*link_id, destination);
                    }
                    output.events.push(LxmfNodeEvent::DirectLinkEstablished {
                        destination,
                        link_id: *link_id,
                        is_initiator: *is_initiator,
                    });
                }
            }
            NodeEvent::LinkIdentified {
                link_id,
                identity_hash,
            } if self.lxmf_links.contains(link_id) => {
                let destination = Destination::compute_destination_hash(
                    &Destination::compute_name_hash(APP_NAME, &[DELIVERY_ASPECT]),
                    identity_hash,
                );
                self.direct_links.insert(destination, *link_id);
                self.link_destinations.insert(*link_id, destination);
                output.events.push(LxmfNodeEvent::DirectLinkEstablished {
                    destination: Some(destination),
                    link_id: *link_id,
                    is_initiator: false,
                });
            }
            NodeEvent::PacketProofRequested {
                packet_hash,
                destination_hash,
                interface_index,
                peer,
            } if *destination_hash == self.delivery_destination => {
                // #376: prove back at the peer the message arrived from,
                // not at every link on the carrier. On a BLE mesh the
                // proof for a delivered LXMF message would otherwise
                // reach the neighbour boards too, which forward it.
                output.core.merge(
                    node.send_proof_on_peer(packet_hash, destination_hash, *interface_index, *peer)
                        .map_err(|_| LxmfNodeError::ProofFailed)?,
                );
            }
            NodeEvent::LinkProofRequested {
                link_id,
                packet_hash,
            } if self.lxmf_links.contains(link_id) => {
                output.core.merge(
                    node.send_data_proof(link_id, packet_hash)
                        .map_err(|_| LxmfNodeError::ProofFailed)?,
                );
            }
            NodeEvent::PacketReceived {
                destination, data, ..
            } if *destination == self.delivery_destination => {
                self.emit_inbound(
                    node.storage(),
                    data,
                    DeliveryMethod::Opportunistic,
                    Some(destination.into_bytes()),
                    &mut output.events,
                );
            }
            NodeEvent::LinkDataReceived { link_id, data } if self.lxmf_links.contains(link_id) => {
                self.emit_inbound(
                    node.storage(),
                    data,
                    DeliveryMethod::Direct,
                    None,
                    &mut output.events,
                );
            }
            NodeEvent::ResourceAdvertised {
                link_id,
                resource_hash,
                transfer_size,
                data_size,
            } if self.lxmf_links.contains(link_id) => {
                let accept = self
                    .config
                    .max_incoming_resource_size
                    .is_none_or(|limit| *data_size <= limit);
                if accept {
                    output.core.merge(node.accept_resource(link_id)?);
                    self.incoming_resource_transfers.insert(
                        *resource_hash,
                        IncomingResourceTransfer {
                            link_id: *link_id,
                            resource_hash: *resource_hash,
                            transfer_size: *transfer_size,
                            data_size: *data_size,
                            progress: 0.0,
                        },
                    );
                } else {
                    output.core.merge(node.reject_resource(link_id)?);
                    output.events.push(LxmfNodeEvent::InboundRejected {
                        method: DeliveryMethod::Direct,
                        reason: InboundRejection::ResourceTooLarge,
                    });
                }
            }
            NodeEvent::ResourceTransferStarted {
                link_id,
                resource_hash,
                is_sender: false,
            } if self.lxmf_links.contains(link_id) => {
                self.incoming_resource_transfers
                    .entry(*resource_hash)
                    .or_insert(IncomingResourceTransfer {
                        link_id: *link_id,
                        resource_hash: *resource_hash,
                        transfer_size: 0,
                        data_size: 0,
                        progress: 0.0,
                    });
            }
            NodeEvent::ResourceProgress {
                link_id,
                resource_hash,
                progress,
                transfer_size,
                data_size,
                is_sender: false,
            } if self.lxmf_links.contains(link_id) => {
                self.incoming_resource_transfers.insert(
                    *resource_hash,
                    IncomingResourceTransfer {
                        link_id: *link_id,
                        resource_hash: *resource_hash,
                        transfer_size: *transfer_size,
                        data_size: *data_size,
                        progress: *progress,
                    },
                );
            }
            NodeEvent::ResourceProgress {
                link_id,
                resource_hash,
                progress,
                data_size,
                is_sender: true,
                ..
            } => {
                if let Some(pending) = self.resources.get_mut(link_id) {
                    if pending.current_resource_hash != *resource_hash {
                        pending.completed_size =
                            pending.completed_size.saturating_add(pending.current_size);
                        pending.current_resource_hash = *resource_hash;
                        pending.current_size = *data_size;
                    }
                    let transferred = pending.completed_size as f64
                        + f64::from(*progress) * pending.current_size as f64;
                    let aggregate_progress = if pending.total_size == 0 {
                        0.0
                    } else {
                        (transferred / pending.total_size as f64).clamp(0.0, 1.0) as f32
                    };
                    output.events.push(LxmfNodeEvent::Progress {
                        message_id: pending.message_id,
                        progress: aggregate_progress,
                    });
                }
            }
            NodeEvent::ResourceCompleted {
                link_id,
                is_sender: true,
                ..
            } => {
                if let Some(pending) = self.resources.remove(link_id) {
                    output.events.push(LxmfNodeEvent::Delivered {
                        message_id: pending.message_id,
                    });
                    self.identify_backchannel(node, *link_id, &mut output.core);
                }
            }
            NodeEvent::ResourceCompleted {
                link_id,
                resource_hash,
                data,
                is_sender: false,
                segment_index,
                total_segments,
                ..
            } if self.lxmf_links.contains(link_id) => {
                self.incoming_resource_transfers.remove(resource_hash);
                self.handle_resource_segment(
                    node.storage(),
                    *link_id,
                    data,
                    *segment_index,
                    *total_segments,
                    &mut output.events,
                );
            }
            NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error,
                is_sender,
                ..
            } => {
                if *is_sender {
                    if let Some(pending) = self.resources.remove(link_id) {
                        output.events.push(LxmfNodeEvent::DeliveryFailed {
                            message_id: pending.message_id,
                            reason: DeliveryFailure::Resource(*error),
                        });
                        // Python tears down the Resource's Link before making
                        // a failed direct transfer eligible for another
                        // attempt. A receiver cancellation is a rejection and
                        // keeps the reusable Link alive; LinkClosed needs no
                        // second teardown.
                        if !matches!(error, ResourceError::Cancelled | ResourceError::LinkClosed) {
                            output.core.merge(node.close_link(link_id));
                        }
                    }
                } else if self.lxmf_links.contains(link_id) {
                    self.incoming_resources.remove(link_id);
                    self.incoming_resource_transfers.remove(resource_hash);
                    output.events.push(LxmfNodeEvent::InboundRejected {
                        method: DeliveryMethod::Direct,
                        reason: InboundRejection::Resource(*error),
                    });
                }
            }
            NodeEvent::PacketDeliveryConfirmed { packet_hash } => {
                if let Some(message_id) = self.packet_receipts.remove(packet_hash) {
                    output.events.push(LxmfNodeEvent::Delivered { message_id });
                }
            }
            NodeEvent::DeliveryFailed { packet_hash, error } => {
                if let Some(message_id) = self.packet_receipts.remove(packet_hash) {
                    output.events.push(LxmfNodeEvent::DeliveryFailed {
                        message_id,
                        reason: DeliveryFailure::Opportunistic(*error),
                    });
                }
            }
            NodeEvent::LinkDeliveryConfirmed {
                link_id,
                packet_hash,
            } => {
                if let Some(pending) = self.link_packet_receipts.remove(packet_hash) {
                    output.events.push(LxmfNodeEvent::Delivered {
                        message_id: pending.message_id,
                    });
                    self.identify_backchannel(node, *link_id, &mut output.core);
                }
            }
            NodeEvent::LinkDeliveryFailed {
                link_id,
                packet_hash,
            } => {
                if let Some(pending) = self.link_packet_receipts.remove(packet_hash) {
                    output.events.push(LxmfNodeEvent::DeliveryFailed {
                        message_id: pending.message_id,
                        reason: DeliveryFailure::DirectPacketTimeout,
                    });
                    // Python LXMessage.__link_packet_timed_out() tears down
                    // the Link after the PacketReceipt timeout callback.
                    output.core.merge(node.close_link(link_id));
                }
            }
            NodeEvent::LinkClosed {
                link_id, reason, ..
            } => {
                self.handle_link_closed(*link_id, *reason, &mut output.events);
            }
            _ => {}
        }
        Ok(output)
    }

    fn emit_inbound<S: Storage>(
        &self,
        storage: &S,
        data: &[u8],
        method: DeliveryMethod,
        inferred_destination: Option<[u8; 16]>,
        events: &mut Vec<LxmfNodeEvent>,
    ) {
        let source_offset = if method == DeliveryMethod::Opportunistic {
            0
        } else {
            DESTINATION_LENGTH
        };
        let source_hash = data
            .get(source_offset..source_offset + DESTINATION_LENGTH)
            .and_then(|source| <[u8; 16]>::try_from(source).ok());
        let source_identity = source_hash
            .as_ref()
            .and_then(|source| storage.get_identity(source));

        match Message::unpack(data, inferred_destination, source_identity, method) {
            Ok(message) if message.destination_hash == self.delivery_destination.into_bytes() => {
                events.push(LxmfNodeEvent::MessageReceived(message));
            }
            Ok(_) => events.push(LxmfNodeEvent::InboundRejected {
                method,
                reason: InboundRejection::WrongDestination,
            }),
            Err(error) => events.push(LxmfNodeEvent::InboundRejected {
                method,
                reason: InboundRejection::Message(error),
            }),
        }
    }

    fn handle_resource_segment<S: Storage>(
        &mut self,
        storage: &S,
        link_id: LinkId,
        data: &[u8],
        segment_index: u32,
        total_segments: u32,
        events: &mut Vec<LxmfNodeEvent>,
    ) {
        if segment_index == 0 || total_segments == 0 || segment_index > total_segments {
            self.incoming_resources.remove(&link_id);
            events.push(LxmfNodeEvent::InboundRejected {
                method: DeliveryMethod::Direct,
                reason: InboundRejection::ResourceSequence,
            });
            return;
        }

        if total_segments == 1 {
            self.emit_inbound(storage, data, DeliveryMethod::Direct, None, events);
            return;
        }

        if segment_index == 1 {
            self.incoming_resources.insert(
                link_id,
                IncomingResourceAssembly {
                    next_segment: 2,
                    total_segments,
                    data: data.to_vec(),
                },
            );
            return;
        }

        let complete = match self.incoming_resources.get_mut(&link_id) {
            Some(assembly)
                if assembly.next_segment == segment_index
                    && assembly.total_segments == total_segments =>
            {
                assembly.data.extend_from_slice(data);
                assembly.next_segment = assembly.next_segment.saturating_add(1);
                segment_index == total_segments
            }
            _ => {
                self.incoming_resources.remove(&link_id);
                events.push(LxmfNodeEvent::InboundRejected {
                    method: DeliveryMethod::Direct,
                    reason: InboundRejection::ResourceSequence,
                });
                return;
            }
        };

        if complete {
            if let Some(assembly) = self.incoming_resources.remove(&link_id) {
                self.emit_inbound(
                    storage,
                    &assembly.data,
                    DeliveryMethod::Direct,
                    None,
                    events,
                );
            }
        }
    }

    fn handle_link_closed(
        &mut self,
        link_id: LinkId,
        reason: LinkCloseReason,
        events: &mut Vec<LxmfNodeEvent>,
    ) {
        self.lxmf_links.remove(&link_id);
        self.identified_links.remove(&link_id);
        self.incoming_resources.remove(&link_id);
        self.incoming_resource_transfers
            .retain(|_, transfer| transfer.link_id != link_id);
        self.link_destinations.remove(&link_id);
        self.pending_links.retain(|_, id| *id != link_id);
        self.direct_links.retain(|_, id| *id != link_id);

        let packet_hashes: Vec<[u8; 32]> = self
            .link_packet_receipts
            .iter()
            .filter_map(|(hash, pending)| (pending.link_id == link_id).then_some(*hash))
            .collect();
        for hash in packet_hashes {
            if let Some(pending) = self.link_packet_receipts.remove(&hash) {
                events.push(LxmfNodeEvent::DeliveryFailed {
                    message_id: pending.message_id,
                    reason: DeliveryFailure::LinkClosed(reason),
                });
            }
        }
        if let Some(pending) = self.resources.remove(&link_id) {
            events.push(LxmfNodeEvent::DeliveryFailed {
                message_id: pending.message_id,
                reason: DeliveryFailure::LinkClosed(reason),
            });
        }
    }

    fn identify_backchannel<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        link_id: LinkId,
        output: &mut TickOutput,
    ) where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if self.identified_links.contains(&link_id)
            || !node.link(&link_id).is_some_and(|link| link.is_initiator())
        {
            return;
        }
        let private_key = node
            .destination(&self.delivery_destination)
            .and_then(|destination| destination.identity())
            .and_then(|identity| identity.private_key_bytes().ok());
        let Some(private_key) = private_key else {
            return;
        };
        let Ok(identity) = Identity::from_private_key_bytes(&private_key) else {
            return;
        };
        if let Ok(follow_up) = node.identify_link(&link_id, &identity) {
            self.identified_links.insert(link_id);
            output.merge(follow_up);
        }
    }
}

/// The named payload classes behind `docs/src/concepts/core-lock-budget.md`,
/// shared with the router harness in `tests/direct_delivery_attempts.rs` so
/// both measure the same bytes under the same column names. Declared here
/// rather than inside `mod tests` because `#[path]` resolves against a
/// `src/node/` directory that does not exist.
#[cfg(test)]
#[path = "../tests/common/payloads.rs"]
mod payloads;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Verification;
    use alloc::collections::VecDeque;
    use alloc::vec;
    use core::cell::Cell;
    use leviculum_core::{Action, InterfaceId, MemoryStorage, NodeCoreBuilder};
    use rand_core::OsRng;

    struct TestClock(Cell<u64>);

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
    }

    fn test_node() -> NodeCore<OsRng, TestClock, MemoryStorage> {
        NodeCoreBuilder::new().build(
            OsRng,
            TestClock(Cell::new(1_000)),
            MemoryStorage::with_defaults(),
        )
    }

    fn identity_from(byte: u8) -> Identity {
        let mut private = [0u8; 64];
        for (index, value) in private.iter_mut().enumerate() {
            *value = byte.wrapping_add(index as u8);
        }
        Identity::from_private_key_bytes(&private).expect("valid deterministic identity")
    }

    fn setup() -> (
        NodeCore<OsRng, TestClock, MemoryStorage>,
        LxmfNode,
        Identity,
        DestinationHash,
    ) {
        let mut node = test_node();
        let local = identity_from(1);
        let local_private = local.private_key_bytes().expect("private key");
        let destination = LxmfNode::delivery_destination(local).expect("delivery destination");
        let local_hash = *destination.hash();
        let lxmf = LxmfNode::register(&mut node, destination, LxmfNodeConfig::default())
            .expect("register LXMF");

        let source = identity_from(101);
        let source_hash = Destination::compute_destination_hash(
            &Destination::compute_name_hash(APP_NAME, &[DELIVERY_ASPECT]),
            source.hash(),
        );
        let source_private = source.private_key_bytes().expect("source key");
        node.remember_identity(
            source_hash,
            Identity::from_private_key_bytes(&source_private).expect("source copy"),
        );

        // Prove the registered destination still owns the intended identity.
        assert_eq!(
            node.destination(&local_hash)
                .and_then(|d| d.identity())
                .and_then(|i| i.private_key_bytes().ok()),
            Some(local_private)
        );
        (node, lxmf, source, source_hash)
    }

    fn message(
        destination: DestinationHash,
        source: &Identity,
        source_hash: DestinationHash,
        method: DeliveryMethod,
        content_len: usize,
    ) -> Message {
        message_with_body(
            destination,
            source,
            source_hash,
            method,
            payloads::degenerate(content_len),
        )
    }

    /// `message` with the payload class named by the caller. The correctness
    /// tests do not care which bytes they carry and keep taking the
    /// `degenerate` fill; the timing harness does care, and says so.
    fn message_with_body(
        destination: DestinationHash,
        source: &Identity,
        source_hash: DestinationHash,
        method: DeliveryMethod,
        body: Vec<u8>,
    ) -> Message {
        Message::create(
            destination.into_bytes(),
            source_hash.into_bytes(),
            source,
            1_700_000_000.0,
            b"title".to_vec(),
            body,
            Vec::new(),
            method,
        )
        .expect("message")
    }

    #[test]
    fn opportunistic_node_event_decodes_and_verifies() {
        let (mut node, mut lxmf, source, source_hash) = setup();
        let msg = message(
            lxmf.delivery_destination_hash(),
            &source,
            source_hash,
            DeliveryMethod::Opportunistic,
            5,
        );
        let event = NodeEvent::PacketReceived {
            destination: lxmf.delivery_destination_hash(),
            data: msg.on_air().expect("opportunistic on-air"),
            interface_index: 0,
        };
        let output = lxmf.handle_event(&mut node, &event).expect("handle");
        assert!(matches!(
            output.events.as_slice(),
            [LxmfNodeEvent::MessageReceived(received)]
                if received.message_id == msg.message_id
                    && received.method == DeliveryMethod::Opportunistic
                    && received.verification == Verification::Valid
        ));
    }

    #[test]
    fn direct_raw_link_event_decodes_full_packed_message() {
        let (mut node, mut lxmf, source, source_hash) = setup();
        let link_id = LinkId::new([0x44; 16]);
        lxmf.lxmf_links.insert(link_id);
        let msg = message(
            lxmf.delivery_destination_hash(),
            &source,
            source_hash,
            DeliveryMethod::Direct,
            5,
        );
        let event = NodeEvent::LinkDataReceived {
            link_id,
            data: msg.pack(),
        };
        let output = lxmf.handle_event(&mut node, &event).expect("handle");
        assert!(matches!(
            output.events.as_slice(),
            [LxmfNodeEvent::MessageReceived(received)]
                if received.message_id == msg.message_id
                    && received.method == DeliveryMethod::Direct
                    && received.verification == Verification::Valid
        ));
    }

    #[test]
    fn link_ownership_is_removed_when_the_link_closes() {
        let (_, mut lxmf, _, _) = setup();
        let link_id = LinkId::new([0x45; 16]);

        assert!(!lxmf.owns_link(&link_id));
        lxmf.lxmf_links.insert(link_id);
        assert!(lxmf.owns_link(&link_id));

        lxmf.handle_link_closed(link_id, LinkCloseReason::Normal, &mut Vec::new());
        assert!(!lxmf.owns_link(&link_id));
    }

    #[test]
    fn split_resource_is_reassembled_before_decode() {
        let (mut node, mut lxmf, source, source_hash) = setup();
        let link_id = LinkId::new([0x55; 16]);
        lxmf.lxmf_links.insert(link_id);
        let msg = message(
            lxmf.delivery_destination_hash(),
            &source,
            source_hash,
            DeliveryMethod::Direct,
            700,
        );
        let packed = msg.pack();
        let split = packed.len() / 2;
        let first = NodeEvent::ResourceCompleted {
            link_id,
            resource_hash: [1; 32],
            data: packed[..split].to_vec(),
            metadata: None,
            is_sender: false,
            segment_index: 1,
            total_segments: 2,
        };
        let second = NodeEvent::ResourceCompleted {
            link_id,
            resource_hash: [2; 32],
            data: packed[split..].to_vec(),
            metadata: None,
            is_sender: false,
            segment_index: 2,
            total_segments: 2,
        };

        let first_output = lxmf.handle_event(&mut node, &first).expect("first segment");
        assert!(first_output.events.is_empty());
        let second_output = lxmf
            .handle_event(&mut node, &second)
            .expect("second segment");
        assert!(matches!(
            second_output.events.as_slice(),
            [LxmfNodeEvent::MessageReceived(received)] if received.message_id == msg.message_id
        ));
    }

    #[test]
    fn representation_matches_python_thresholds_and_fallback() {
        let (_, lxmf, source, source_hash) = setup();
        let small = message(
            lxmf.delivery_destination_hash(),
            &source,
            source_hash,
            DeliveryMethod::Opportunistic,
            1,
        );
        assert_eq!(
            LxmfNode::representation(&small).expect("small"),
            DeliveryRepresentation::OpportunisticPacket
        );

        let fallback = message(
            lxmf.delivery_destination_hash(),
            &source,
            source_hash,
            DeliveryMethod::Opportunistic,
            310,
        );
        assert_eq!(
            LxmfNode::representation(&fallback).expect("fallback"),
            DeliveryRepresentation::DirectPacket
        );

        let large = message(
            lxmf.delivery_destination_hash(),
            &source,
            source_hash,
            DeliveryMethod::Direct,
            700,
        );
        assert_eq!(
            LxmfNode::representation(&large).expect("large"),
            DeliveryRepresentation::DirectResource
        );
    }

    #[test]
    fn out_of_order_resource_is_rejected_and_discarded() {
        let (mut node, mut lxmf, _, _) = setup();
        let link_id = LinkId::new([0x66; 16]);
        lxmf.lxmf_links.insert(link_id);
        let event = NodeEvent::ResourceCompleted {
            link_id,
            resource_hash: [3; 32],
            data: vec![1, 2, 3],
            metadata: None,
            is_sender: false,
            segment_index: 2,
            total_segments: 3,
        };
        let output = lxmf.handle_event(&mut node, &event).expect("handle");
        assert!(matches!(
            output.events.as_slice(),
            [LxmfNodeEvent::InboundRejected {
                method: DeliveryMethod::Direct,
                reason: InboundRejection::ResourceSequence,
            }]
        ));
    }

    #[test]
    fn core_raw_packet_timeout_fails_direct_delivery_once() {
        let (mut node, mut lxmf, _, _) = setup();
        let link_id = LinkId::new([0x77; 16]);
        let message_id = [0xa5; 32];
        let packet_hash = [0x33; 32];
        lxmf.link_packet_receipts.insert(
            packet_hash,
            PendingLinkPacket {
                message_id,
                link_id,
            },
        );
        let event = NodeEvent::LinkDeliveryFailed {
            link_id,
            packet_hash,
        };
        let output = lxmf.handle_event(&mut node, &event).expect("handle");
        assert!(matches!(
            output.events.as_slice(),
            [LxmfNodeEvent::DeliveryFailed {
                message_id: id,
                reason: DeliveryFailure::DirectPacketTimeout,
            }] if id == &message_id
        ));
        assert!(lxmf
            .handle_event(&mut node, &event)
            .expect("repeat handle")
            .events
            .is_empty());
    }

    #[test]
    fn split_resource_sender_progress_is_aggregated_across_segments() {
        let (mut node, mut lxmf, _, _) = setup();
        let link_id = LinkId::new([0x44; 16]);
        let message_id = [0x55; 32];
        let first_hash = [0x66; 32];
        let second_hash = [0x77; 32];
        let segment_size = RESOURCE_MAX_EFFICIENT_SIZE as u64;
        lxmf.resources.insert(
            link_id,
            PendingResource {
                message_id,
                current_resource_hash: first_hash,
                completed_size: 0,
                current_size: segment_size,
                total_size: segment_size * 2,
            },
        );

        let first = lxmf
            .handle_event(
                &mut node,
                &NodeEvent::ResourceProgress {
                    link_id,
                    resource_hash: first_hash,
                    progress: 0.8,
                    transfer_size: segment_size,
                    data_size: segment_size,
                    is_sender: true,
                },
            )
            .expect("first segment progress");
        assert!(matches!(
            first.events.as_slice(),
            [LxmfNodeEvent::Progress { message_id: id, progress }]
                if *id == message_id && (*progress - 0.4).abs() < 0.0001
        ));

        let second = lxmf
            .handle_event(
                &mut node,
                &NodeEvent::ResourceProgress {
                    link_id,
                    resource_hash: second_hash,
                    progress: 0.2,
                    transfer_size: segment_size,
                    data_size: segment_size,
                    is_sender: true,
                },
            )
            .expect("second segment progress");
        assert!(matches!(
            second.events.as_slice(),
            [LxmfNodeEvent::Progress { message_id: id, progress }]
                if *id == message_id && (*progress - 0.6).abs() < 0.0001
        ));
    }

    #[test]
    fn incoming_resource_transfers_are_tracked_until_failure() {
        let (mut node, mut lxmf, _, _) = setup();
        let link_id = LinkId::new([0x81; 16]);
        let resource_hash = [0x82; 32];
        lxmf.lxmf_links.insert(link_id);

        let _ = lxmf
            .handle_event(
                &mut node,
                &NodeEvent::ResourceTransferStarted {
                    link_id,
                    resource_hash,
                    is_sender: false,
                },
            )
            .expect("start incoming resource");
        assert_eq!(lxmf.incoming_resource_count(), 1);

        let _ = lxmf
            .handle_event(
                &mut node,
                &NodeEvent::ResourceProgress {
                    link_id,
                    resource_hash,
                    progress: 0.25,
                    transfer_size: 1_024,
                    data_size: 2_048,
                    is_sender: false,
                },
            )
            .expect("update incoming resource");
        assert_eq!(
            lxmf.incoming_resource_transfers()
                .copied()
                .collect::<Vec<_>>(),
            vec![IncomingResourceTransfer {
                link_id,
                resource_hash,
                transfer_size: 1_024,
                data_size: 2_048,
                progress: 0.25,
            }]
        );

        let _ = lxmf
            .handle_event(
                &mut node,
                &NodeEvent::ResourceFailed {
                    link_id,
                    resource_hash,
                    error: ResourceError::Timeout,
                    is_sender: false,
                },
            )
            .expect("fail incoming resource");
        assert_eq!(lxmf.incoming_resource_count(), 0);
    }

    struct Peer {
        node: NodeCore<OsRng, TestClock, MemoryStorage>,
        lxmf: LxmfNode,
        signing_identity: Identity,
        destination: DestinationHash,
    }

    fn peer(seed: u8) -> Peer {
        let mut node = test_node();
        let identity = identity_from(seed);
        let private = identity.private_key_bytes().expect("private delivery key");
        let destination = LxmfNode::delivery_destination(identity).expect("delivery destination");
        let destination_hash = *destination.hash();
        let lxmf = LxmfNode::register(&mut node, destination, LxmfNodeConfig::default())
            .expect("register delivery destination");
        Peer {
            node,
            lxmf,
            signing_identity: Identity::from_private_key_bytes(&private)
                .expect("signing identity copy"),
            destination: destination_hash,
        }
    }

    fn take_packets(actions: Vec<Action>) -> Vec<Vec<u8>> {
        actions
            .into_iter()
            .map(|action| match action {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
            })
            .collect()
    }

    fn absorb_core(
        peer: &mut Peer,
        core: TickOutput,
        app_events: &mut Vec<LxmfNodeEvent>,
    ) -> Vec<Vec<u8>> {
        let mut actions = core.actions;
        let mut events: VecDeque<NodeEvent> = core.events.into();
        while let Some(event) = events.pop_front() {
            let follow_up = peer
                .lxmf
                .handle_event(&mut peer.node, &event)
                .expect("handle NodeCore event");
            app_events.extend(follow_up.events);
            actions.extend(follow_up.core.actions);
            events.extend(follow_up.core.events);
        }
        take_packets(actions)
    }

    fn receive(
        peer: &mut Peer,
        packets: Vec<Vec<u8>>,
        app_events: &mut Vec<LxmfNodeEvent>,
    ) -> Vec<Vec<u8>> {
        let mut outbound = Vec::new();
        for packet in packets {
            let core = peer.node.handle_packet(InterfaceId(0), &packet);
            outbound.extend(absorb_core(peer, core, app_events));
        }
        outbound
    }

    fn pump(
        a: &mut Peer,
        b: &mut Peer,
        mut to_b: Vec<Vec<u8>>,
        a_events: &mut Vec<LxmfNodeEvent>,
        b_events: &mut Vec<LxmfNodeEvent>,
    ) {
        let mut to_a = Vec::new();
        for _ in 0..256 {
            if !to_b.is_empty() {
                to_a.extend(receive(b, core::mem::take(&mut to_b), b_events));
            }
            if !to_a.is_empty() {
                to_b.extend(receive(a, core::mem::take(&mut to_a), a_events));
            }
            if to_a.is_empty() && to_b.is_empty() {
                return;
            }
        }
        panic!("NodeCore exchange did not quiesce");
    }

    fn pump_bidirectional(
        a: &mut Peer,
        b: &mut Peer,
        mut to_b: Vec<Vec<u8>>,
        mut to_a: Vec<Vec<u8>>,
        a_events: &mut Vec<LxmfNodeEvent>,
        b_events: &mut Vec<LxmfNodeEvent>,
    ) {
        for _ in 0..512 {
            if !to_b.is_empty() {
                to_a.extend(receive(b, core::mem::take(&mut to_b), b_events));
            }
            if !to_a.is_empty() {
                to_b.extend(receive(a, core::mem::take(&mut to_a), a_events));
            }
            if to_a.is_empty() && to_b.is_empty() {
                return;
            }
        }
        panic!("bidirectional NodeCore exchange did not quiesce");
    }

    fn exchange_announces(a: &mut Peer, b: &mut Peer) {
        let mut a_events = Vec::new();
        let mut b_events = Vec::new();
        let a_announce = a
            .node
            .announce_destination(&a.destination, None)
            .expect("announce a");
        let b_announce = b
            .node
            .announce_destination(&b.destination, None)
            .expect("announce b");
        let to_b = absorb_core(a, a_announce, &mut a_events);
        let to_a = absorb_core(b, b_announce, &mut b_events);
        let replies_to_a = receive(b, to_b, &mut b_events);
        let replies_to_b = receive(a, to_a, &mut a_events);
        pump(a, b, replies_to_b, &mut a_events, &mut b_events);
        pump(b, a, replies_to_a, &mut b_events, &mut a_events);
    }

    #[test]
    fn failed_outgoing_resource_closes_its_direct_link_before_retry() {
        let mut sender = peer(8);
        let mut receiver = peer(88);
        exchange_announces(&mut sender, &mut receiver);

        let mut sender_events = Vec::new();
        let mut receiver_events = Vec::new();
        let (state, link_output) = sender
            .lxmf
            .ensure_direct_link(&mut sender.node, receiver.destination)
            .expect("establish direct link");
        let DirectLinkState::Started(link_id) = state else {
            panic!("expected a new direct link");
        };
        pump(
            &mut sender,
            &mut receiver,
            take_packets(link_output.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(sender.node.link(&link_id).is_some());

        let message_id = [0x91; 32];
        let resource_hash = [0x92; 32];
        sender.lxmf.resources.insert(
            link_id,
            PendingResource {
                message_id,
                current_resource_hash: resource_hash,
                completed_size: 0,
                current_size: 1,
                total_size: 1,
            },
        );

        let output = sender
            .lxmf
            .handle_event(
                &mut sender.node,
                &NodeEvent::ResourceFailed {
                    link_id,
                    resource_hash,
                    error: ResourceError::Timeout,
                    is_sender: true,
                },
            )
            .expect("fail outgoing Resource");

        assert!(matches!(
            output.events.as_slice(),
            [LxmfNodeEvent::DeliveryFailed {
                message_id: id,
                reason: DeliveryFailure::Resource(ResourceError::Timeout),
            }] if *id == message_id
        ));
        assert!(sender.node.link(&link_id).is_none());
        assert!(output.core.events.iter().any(|event| matches!(
            event,
            NodeEvent::LinkClosed {
                link_id: closed,
                reason: LinkCloseReason::Normal,
                ..
            } if *closed == link_id
        )));
    }

    #[test]
    fn receiver_cancelled_resource_keeps_the_direct_link_reusable() {
        let mut sender = peer(9);
        let mut receiver = peer(89);
        exchange_announces(&mut sender, &mut receiver);

        let mut sender_events = Vec::new();
        let mut receiver_events = Vec::new();
        let (state, link_output) = sender
            .lxmf
            .ensure_direct_link(&mut sender.node, receiver.destination)
            .expect("establish direct link");
        let DirectLinkState::Started(link_id) = state else {
            panic!("expected a new direct link");
        };
        pump(
            &mut sender,
            &mut receiver,
            take_packets(link_output.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );

        let message_id = [0x93; 32];
        let resource_hash = [0x94; 32];
        sender.lxmf.resources.insert(
            link_id,
            PendingResource {
                message_id,
                current_resource_hash: resource_hash,
                completed_size: 0,
                current_size: 1,
                total_size: 1,
            },
        );

        let output = sender
            .lxmf
            .handle_event(
                &mut sender.node,
                &NodeEvent::ResourceFailed {
                    link_id,
                    resource_hash,
                    error: ResourceError::Cancelled,
                    is_sender: true,
                },
            )
            .expect("reject outgoing Resource");

        assert!(matches!(
            output.events.as_slice(),
            [LxmfNodeEvent::DeliveryFailed {
                message_id: id,
                reason: DeliveryFailure::Resource(ResourceError::Cancelled),
            }] if *id == message_id
        ));
        assert!(sender.node.link(&link_id).is_some());
        assert!(output.core.events.is_empty());
    }

    #[test]
    fn nodecore_round_trip_opportunistic_direct_packet_and_resource() {
        let mut sender = peer(10);
        let mut receiver = peer(90);
        exchange_announces(&mut sender, &mut receiver);

        let mut sender_events = Vec::new();
        let mut receiver_events = Vec::new();

        let opportunistic = message(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            DeliveryMethod::Opportunistic,
            12,
        );
        let sent = sender
            .lxmf
            .send(&mut sender.node, &opportunistic)
            .expect("send opportunistic");
        sender_events.extend(sent.events);
        pump(
            &mut sender,
            &mut receiver,
            take_packets(sent.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(sender_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::Delivered { message_id } if *message_id == opportunistic.message_id
        )));
        assert!(receiver_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::MessageReceived(received)
                if received.message_id == opportunistic.message_id
                    && received.verification == Verification::Valid
        )));

        let (link_state, link_output) = sender
            .lxmf
            .ensure_direct_link(&mut sender.node, receiver.destination)
            .expect("ensure direct link");
        assert!(matches!(link_state, DirectLinkState::Started(_)));
        sender_events.extend(link_output.events);
        pump(
            &mut sender,
            &mut receiver,
            take_packets(link_output.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(sender.lxmf.direct_link(&receiver.destination).is_some());

        let direct = message(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            DeliveryMethod::Direct,
            12,
        );
        let sent = sender
            .lxmf
            .send(&mut sender.node, &direct)
            .expect("send direct packet");
        sender_events.extend(sent.events);
        pump(
            &mut sender,
            &mut receiver,
            take_packets(sent.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(sender_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::Delivered { message_id } if *message_id == direct.message_id
        )));
        assert!(receiver_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::MessageReceived(received) if received.message_id == direct.message_id
        )));

        let resource = message(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            DeliveryMethod::Direct,
            1_200,
        );
        assert_eq!(
            LxmfNode::representation(&resource).expect("representation"),
            DeliveryRepresentation::DirectResource
        );
        let sent = sender
            .lxmf
            .send(&mut sender.node, &resource)
            .expect("send direct resource");
        sender_events.extend(sent.events);
        pump(
            &mut sender,
            &mut receiver,
            take_packets(sent.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(sender_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::Delivered { message_id } if *message_id == resource.message_id
        )));
        assert!(receiver_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::MessageReceived(received)
                if received.message_id == resource.message_id
                    && received.content.len() == 1_200
        )));
    }

    /// Why only the Resource path is phased: the other two submit one packet
    /// each, bounded by their MDU, so neither carries a build that scales with
    /// the message.
    #[test]
    fn opportunistic_and_direct_packet_submit_one_packet_each() {
        let mut sender = peer(41);
        let mut receiver = peer(131);
        exchange_announces(&mut sender, &mut receiver);
        let mut sender_events = Vec::new();
        let mut receiver_events = Vec::new();

        let opportunistic = message(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            DeliveryMethod::Opportunistic,
            250,
        );
        assert_eq!(
            LxmfNode::representation(&opportunistic).expect("representation"),
            DeliveryRepresentation::OpportunisticPacket
        );
        let sent = sender
            .lxmf
            .send(&mut sender.node, &opportunistic)
            .expect("send opportunistic");
        assert_eq!(sent.core.actions.len(), 1);

        let (_, link_output) = sender
            .lxmf
            .ensure_direct_link(&mut sender.node, receiver.destination)
            .expect("ensure direct link");
        pump(
            &mut sender,
            &mut receiver,
            take_packets(link_output.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );

        let direct = message(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            DeliveryMethod::Direct,
            300,
        );
        assert_eq!(
            LxmfNode::representation(&direct).expect("representation"),
            DeliveryRepresentation::DirectPacket
        );
        let sent = sender
            .lxmf
            .send(&mut sender.node, &direct)
            .expect("send direct packet");
        assert_eq!(sent.core.actions.len(), 1);
    }

    /// The phased path is a second build path beside the composed `send`; a
    /// drift between them would deliver nothing, and the composed round-trip
    /// above cannot see it.
    #[test]
    fn phased_resource_send_delivers_what_the_composed_one_does() {
        let mut sender = peer(40);
        let mut receiver = peer(130);
        exchange_announces(&mut sender, &mut receiver);

        let mut sender_events = Vec::new();
        let mut receiver_events = Vec::new();

        let (_, link_output) = sender
            .lxmf
            .ensure_direct_link(&mut sender.node, receiver.destination)
            .expect("ensure direct link");
        sender_events.extend(link_output.events);
        pump(
            &mut sender,
            &mut receiver,
            take_packets(link_output.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );

        let resource = message(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            DeliveryMethod::Direct,
            1_200,
        );
        assert_eq!(
            LxmfNode::representation(&resource).expect("representation"),
            DeliveryRepresentation::DirectResource
        );

        let params = sender
            .lxmf
            .resource_send_params(&sender.node, &resource)
            .expect("phase 1");
        let prepared =
            LxmfNode::prepare_resource_send(&params, &resource, true, &mut OsRng).expect("phase 2");
        let sent = sender
            .lxmf
            .commit_resource_send(&mut sender.node, prepared)
            .expect("phase 3");

        sender_events.extend(sent.events);
        pump(
            &mut sender,
            &mut receiver,
            take_packets(sent.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(sender_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::Delivered { message_id } if *message_id == resource.message_id
        )));
        assert!(receiver_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::MessageReceived(received)
                if received.message_id == resource.message_id
                    && received.content.len() == 1_200
        )));
    }

    #[test]
    fn direct_link_uses_identity_cached_by_core_without_lxmf_peer_state() {
        let mut sender = peer(30);
        let mut receiver = peer(120);
        let announce = receiver
            .node
            .announce_destination(&receiver.destination, None)
            .expect("announce receiver");

        // Feed the verified announce only to Core. LXMF deliberately does not
        // observe its event, proving link setup has no separate peer-key cache.
        for packet in take_packets(announce.actions) {
            let _ = sender.node.handle_packet(InterfaceId(0), &packet);
        }

        assert!(sender
            .node
            .storage()
            .get_identity(receiver.destination.as_bytes())
            .is_some());
        let (state, _) = sender
            .lxmf
            .ensure_direct_link(&mut sender.node, receiver.destination)
            .expect("start direct link from Core identity");
        assert!(matches!(state, DirectLinkState::Started(_)));
    }

    #[test]
    fn nodecore_direct_resources_are_full_duplex_on_one_link() {
        let mut a = peer(20);
        let mut b = peer(110);
        exchange_announces(&mut a, &mut b);

        let mut a_events = Vec::new();
        let mut b_events = Vec::new();
        let (_, link_output) = a
            .lxmf
            .ensure_direct_link(&mut a.node, b.destination)
            .expect("establish direct link");
        pump(
            &mut a,
            &mut b,
            take_packets(link_output.core.actions),
            &mut a_events,
            &mut b_events,
        );

        // Delivering one packet identifies the initiator over the backchannel,
        // allowing both peers to address the same established link.
        let introduction = message(
            b.destination,
            &a.signing_identity,
            a.destination,
            DeliveryMethod::Direct,
            12,
        );
        let sent = a
            .lxmf
            .send(&mut a.node, &introduction)
            .expect("send introduction");
        pump(
            &mut a,
            &mut b,
            take_packets(sent.core.actions),
            &mut a_events,
            &mut b_events,
        );
        assert_eq!(
            a.lxmf.direct_link(&b.destination),
            b.lxmf.direct_link(&a.destination)
        );

        let large = message(
            b.destination,
            &a.signing_identity,
            a.destination,
            DeliveryMethod::Direct,
            12_000,
        );
        let small = message(
            a.destination,
            &b.signing_identity,
            b.destination,
            DeliveryMethod::Direct,
            700,
        );
        assert_eq!(
            LxmfNode::representation(&large).expect("large representation"),
            DeliveryRepresentation::DirectResource
        );
        assert_eq!(
            LxmfNode::representation(&small).expect("small representation"),
            DeliveryRepresentation::DirectResource
        );

        let large_output = a
            .lxmf
            .send(&mut a.node, &large)
            .expect("send large resource");
        let small_output = b
            .lxmf
            .send(&mut b.node, &small)
            .expect("send reverse resource");
        pump_bidirectional(
            &mut a,
            &mut b,
            take_packets(large_output.core.actions),
            take_packets(small_output.core.actions),
            &mut a_events,
            &mut b_events,
        );

        assert!(a_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::Delivered { message_id } if *message_id == large.message_id
        )));
        assert!(b_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::MessageReceived(received)
                if received.message_id == large.message_id
                    && received.content.len() == 12_000
        )));
        assert!(b_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::Delivered { message_id } if *message_id == small.message_id
        )));
        assert!(a_events.iter().any(|event| matches!(
            event,
            LxmfNodeEvent::MessageReceived(received)
                if received.message_id == small.message_id
                    && received.content.len() == 700
        )));
    }
    /// Reports what each delivery representation costs a caller that holds the
    /// core, so `docs/src/concepts/core-lock-budget.md` can cite the adapter's
    /// numbers instead of assuming them (Codeberg #196).
    ///
    /// Run it deliberately, on a release build:
    ///
    /// ```text
    /// cargo test -p leviculum-lxmf --release --lib measure_send_lock_costs \
    ///     -- --ignored --nocapture
    /// ```
    ///
    /// `#[ignore]`d because it reports timings: an assertion over them would
    /// fail on a loaded CI machine for reasons that have nothing to do with the
    /// code. The invariants it exists to defend are asserted, without a clock,
    /// by `opportunistic_and_direct_packet_submit_one_packet_each` and
    /// `phased_resource_send_delivers_what_the_composed_one_does`.
    ///
    /// Every resource row names its payload class ([`payloads::GENERATORS`]),
    /// because the cost being reported is a compressor's and a compressor's
    /// cost is a property of the bytes. Each cell is the median of
    /// [`MEASURE_SAMPLES`] timed runs after one discarded warm-up, each run on
    /// a freshly linked pair — a second Resource to the same link cannot build
    /// at all (`TransferInProgress`), so re-using one would not be measuring
    /// the same thing twice.
    ///
    /// The 1 MiB row is segment 1 of a two-segment transfer: the split
    /// boundary is `RESOURCE_MAX_EFFICIENT_SIZE` (1 048 575 B) applied to the
    /// *packed, uncompressed* length (`leviculum-core/src/resource/
    /// outgoing.rs:123`), which a 1 MiB body exceeds in every payload class.
    /// Segments 2..N are built on the receive path, under the caller's lock.
    #[test]
    #[ignore]
    fn measure_send_lock_costs() {
        use std::time::Instant;

        fn linked() -> (Peer, Peer) {
            let mut sender = peer(70);
            let mut receiver = peer(170);
            exchange_announces(&mut sender, &mut receiver);
            let (_, out) = sender
                .lxmf
                .ensure_direct_link(&mut sender.node, receiver.destination)
                .expect("link");
            let mut a = Vec::new();
            let mut b = Vec::new();
            pump(
                &mut sender,
                &mut receiver,
                take_packets(out.core.actions),
                &mut a,
                &mut b,
            );
            (sender, receiver)
        }

        for (label, method, len) in [
            ("opportunistic", DeliveryMethod::Opportunistic, 250usize),
            ("direct packet", DeliveryMethod::Direct, 300),
        ] {
            let (mut s, r) = linked();
            let m = message(
                r.destination,
                &s.signing_identity,
                s.destination,
                method,
                len,
            );
            assert_ne!(
                LxmfNode::representation(&m).expect("rep"),
                DeliveryRepresentation::DirectResource
            );
            // Discard the first send: it pays one-time setup this measurement
            // is not about.
            let _ = s.lxmf.send(&mut s.node, &m).expect("warm-up send");
            let m = message(
                r.destination,
                &s.signing_identity,
                s.destination,
                method,
                len,
            );
            let t = Instant::now();
            let sent = s.lxmf.send(&mut s.node, &m).expect("send");
            let elapsed = t.elapsed();
            assert_eq!(sent.core.actions.len(), 1, "{label} must be one packet");
            std::println!("{label}: 1 packet, {elapsed:?}");
        }

        // One composed send on a fresh pair, timed.
        let composed_once = |body: Vec<u8>| {
            let (mut s, r) = linked();
            let m = message_with_body(
                r.destination,
                &s.signing_identity,
                s.destination,
                DeliveryMethod::Direct,
                body,
            );
            assert_eq!(
                LxmfNode::representation(&m),
                Ok(DeliveryRepresentation::DirectResource),
                "the timed payload must take the Resource path"
            );
            let t = Instant::now();
            let _ = s.lxmf.send(&mut s.node, &m).expect("composed");
            t.elapsed()
        };

        // The same send in three phases, timing the two locked ones and the
        // off-lock build separately.
        let phased_once = |body: Vec<u8>| {
            let (mut s, r) = linked();
            let m = message_with_body(
                r.destination,
                &s.signing_identity,
                s.destination,
                DeliveryMethod::Direct,
                body,
            );
            let t = Instant::now();
            let params = s.lxmf.resource_send_params(&s.node, &m).expect("phase 1");
            let phase1 = t.elapsed();
            let t = Instant::now();
            let prepared =
                LxmfNode::prepare_resource_send(&params, &m, true, &mut OsRng).expect("phase 2");
            let build = t.elapsed();
            let t = Instant::now();
            let _ = s
                .lxmf
                .commit_resource_send(&mut s.node, prepared)
                .expect("phase 3");
            let phase3 = t.elapsed();
            (phase1 + phase3, build)
        };

        for (class, generate) in payloads::GENERATORS {
            for len in [16 * 1024usize, 256 * 1024, 1024 * 1024] {
                // What the class name claims, as a number: the same bz2
                // settings the Resource build uses
                // (`leviculum-core/src/compression.rs:65-71`).
                let shrunk = leviculum_core::compression::compress(&generate(len))
                    .expect("compress")
                    .len();
                std::println!(
                    "         {len:>7}B {class:>14}: bz2 {shrunk}B, {:.1}x",
                    len as f64 / shrunk as f64
                );
                // The discarded first run is reported, not just dropped: an
                // n=1 harness reports exactly that number, and the gap
                // between it and the median is how much such a harness is
                // wrong by.
                let mut composed_samples = Vec::with_capacity(MEASURE_SAMPLES + 1);
                for _ in 0..=MEASURE_SAMPLES {
                    composed_samples.push(composed_once(generate(len)));
                }
                let cold = composed_samples.remove(0);
                let composed = median(composed_samples);
                // One pass over the phased arm, two medians out of it: the
                // locked halves and the build they moved off the lock come
                // from the same runs, so the pair can be read together.
                let mut phased = Vec::with_capacity(MEASURE_SAMPLES + 1);
                for _ in 0..=MEASURE_SAMPLES {
                    phased.push(phased_once(generate(len)));
                }
                phased.remove(0);
                let locked = median(phased.iter().map(|(locked, _)| *locked).collect());
                let build = median(phased.iter().map(|(_, build)| *build).collect());
                std::println!(
                    "resource {len:>7}B {class:>14}: composed(locked) {composed:?} | \
                     phased(locked) {locked:?} | off-lock build {build:?} | \
                     composed cold run {cold:?}"
                );
            }
        }
    }

    /// Timed samples per reported cell, after one discarded warm-up.
    const MEASURE_SAMPLES: usize = 5;

    /// Median of the [`MEASURE_SAMPLES`] runs left after the warm-up is
    /// dropped. The router harness in `tests/direct_delivery_attempts.rs`
    /// has the same shape over ticks.
    fn median(mut samples: Vec<core::time::Duration>) -> core::time::Duration {
        samples.sort();
        samples[samples.len() / 2]
    }

    /// `peer` with an explicit adapter config, for the transfer-limit tests.
    fn peer_with_config(seed: u8, config: LxmfNodeConfig) -> Peer {
        let mut node = test_node();
        let identity = identity_from(seed);
        let private = identity.private_key_bytes().expect("private delivery key");
        let destination = LxmfNode::delivery_destination(identity).expect("delivery destination");
        let destination_hash = *destination.hash();
        let lxmf = LxmfNode::register(&mut node, destination, config)
            .expect("register delivery destination");
        Peer {
            node,
            lxmf,
            signing_identity: Identity::from_private_key_bytes(&private)
                .expect("signing identity copy"),
            destination: destination_hash,
        }
    }

    /// A direct message whose packed wire size is exactly `target` bytes.
    ///
    /// Two-pass: measure the fixed packing overhead once, then size the
    /// content so `pack()` lands on the byte. Both passes stay in msgpack's
    /// bin32 range so the header width cannot change between them.
    fn message_packed_exactly(
        destination: DestinationHash,
        source: &Identity,
        source_hash: DestinationHash,
        target: usize,
    ) -> Message {
        let probe_len = 100_000;
        let probe = message(
            destination,
            source,
            source_hash,
            DeliveryMethod::Direct,
            probe_len,
        );
        let overhead = probe.pack().len() - probe_len;
        let m = message(
            destination,
            source,
            source_hash,
            DeliveryMethod::Direct,
            target - overhead,
        );
        assert_eq!(m.pack().len(), target, "two-pass sizing must land exactly");
        m
    }

    fn linked_pair(sender: Peer, receiver: Peer) -> (Peer, Peer) {
        let mut sender = sender;
        let mut receiver = receiver;
        exchange_announces(&mut sender, &mut receiver);
        let (_, out) = sender
            .lxmf
            .ensure_direct_link(&mut sender.node, receiver.destination)
            .expect("link");
        let mut a = Vec::new();
        let mut b = Vec::new();
        pump(
            &mut sender,
            &mut receiver,
            take_packets(out.core.actions),
            &mut a,
            &mut b,
        );
        (sender, receiver)
    }

    /// The defaults are Python's: `DELIVERY_LIMIT` kilobytes times 1000 in
    /// both directions (`reference/LXMF/LXMF/LXMRouter.py:60`).
    #[test]
    fn default_config_carries_python_delivery_limits() {
        let config = LxmfNodeConfig::default();
        assert_eq!(config.max_incoming_resource_size, Some(1_000_000));
        assert_eq!(config.max_outgoing_delivery_size, Some(1_000_000));
    }

    /// An over-limit direct send is refused before any build: the error names
    /// the sizes, no core actions are produced, no advertisement reaches the
    /// receiver, and no outbound Resource is tracked.
    #[test]
    fn over_limit_direct_send_is_refused_and_nothing_advertised() {
        let (mut sender, mut receiver) = linked_pair(peer(31), peer(131));
        let m = message_packed_exactly(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            1_000_001,
        );
        assert_eq!(
            LxmfNode::representation(&m),
            Ok(DeliveryRepresentation::DirectResource)
        );

        let refused = sender.lxmf.send(&mut sender.node, &m);
        assert_eq!(
            refused.err(),
            Some(LxmfNodeError::DeliveryLimitExceeded {
                size: 1_000_001,
                limit: 1_000_000,
            })
        );
        assert!(
            sender.lxmf.resources.is_empty(),
            "no outbound Resource may be tracked for a refused send"
        );

        // Action level: drive both nodes; the receiver must never observe an
        // advertisement or any other LXMF event for the refused message.
        let mut sender_events = Vec::new();
        let mut receiver_events = Vec::new();
        let core = sender.node.handle_timeout();
        let to_receiver = absorb_core(&mut sender, core, &mut sender_events);
        pump(
            &mut sender,
            &mut receiver,
            to_receiver,
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(
            receiver_events.is_empty(),
            "receiver saw LXMF events for a refused send: {receiver_events:?}"
        );
        assert_eq!(receiver.lxmf.incoming_resource_count(), 0);
    }

    /// Exactly at the limit passes, as in the reference: the comparison is
    /// strictly greater (`reference/LXMF/LXMF/LXMRouter.py:1980`), so both
    /// our send side and a default-configured receiver accept 1 000 000.
    #[test]
    fn at_limit_direct_send_is_advertised_and_accepted() {
        let (mut sender, mut receiver) = linked_pair(peer(32), peer(132));
        let m = message_packed_exactly(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            1_000_000,
        );

        let output = sender.lxmf.send(&mut sender.node, &m).expect("send");
        assert!(
            !output.core.actions.is_empty(),
            "an at-limit send must produce the advertisement"
        );

        let mut sender_events = Vec::new();
        let mut receiver_events = Vec::new();
        pump(
            &mut sender,
            &mut receiver,
            take_packets(output.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(
            !receiver_events
                .iter()
                .any(|e| matches!(e, LxmfNodeEvent::InboundRejected { .. })),
            "default receiver must accept an at-limit advertisement: {receiver_events:?}"
        );
        assert!(
            receiver.lxmf.incoming_resource_count() > 0
                || receiver_events
                    .iter()
                    .any(|e| matches!(e, LxmfNodeEvent::MessageReceived(_))),
            "the at-limit transfer must have been accepted"
        );
    }

    /// The receive default matches the reference: an unrestricted sender's
    /// Resource one byte over 1 000 000 is refused by a default-constructed
    /// node at the advertisement.
    #[test]
    fn default_receiver_rejects_resource_above_python_delivery_limit() {
        let unrestricted = LxmfNodeConfig {
            max_outgoing_delivery_size: None,
            ..LxmfNodeConfig::default()
        };
        let (mut sender, mut receiver) = linked_pair(peer_with_config(33, unrestricted), peer(133));
        let m = message_packed_exactly(
            receiver.destination,
            &sender.signing_identity,
            sender.destination,
            1_000_001,
        );

        let output = sender.lxmf.send(&mut sender.node, &m).expect("send");
        let mut sender_events = Vec::new();
        let mut receiver_events = Vec::new();
        pump(
            &mut sender,
            &mut receiver,
            take_packets(output.core.actions),
            &mut sender_events,
            &mut receiver_events,
        );
        assert!(
            receiver_events.iter().any(|e| matches!(
                e,
                LxmfNodeEvent::InboundRejected {
                    reason: InboundRejection::ResourceTooLarge,
                    ..
                }
            )),
            "default receiver must refuse an over-limit advertisement: {receiver_events:?}"
        );
        assert_eq!(receiver.lxmf.incoming_resource_count(), 0);
        assert!(
            !receiver_events
                .iter()
                .any(|e| matches!(e, LxmfNodeEvent::MessageReceived(_))),
            "no over-limit message may be delivered"
        );
    }
}
