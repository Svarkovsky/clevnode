//! NodeCore transport adapter for the LXMF propagation mailbox client.
//!
//! The adapter owns only live transport correlation: propagation announces,
//! outgoing Link establishment, Link identification, originator uploads and
//! `/get` requests. It deliberately does not accept incoming links, register
//! request handlers, serve propagation data, or implement propagation-node
//! peering. Durable state remains the responsibility of
//! [`crate::router::LxmfRouter`].

use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    vec::Vec,
};

use leviculum_core::{
    Clock, Destination, DestinationError, DestinationHash, DestinationType, Direction, Identity,
    LinkCloseReason, LinkId, NodeCore, NodeEvent, ReceivedAnnounce, RequestError, ResourceError,
    SendError, Storage, TickOutput,
};
use rand_core::CryptoRngCore;

use crate::{
    constants::LINK_PACKET_MAX_CONTENT,
    node::APP_NAME,
    propagation::{PropagationNodeAnnounce, PropagationSignal, MESSAGE_GET_PATH},
};

/// Reticulum destination aspect used by LXMF propagation nodes.
pub const PROPAGATION_ASPECT: &str = "propagation";

/// Semantic stage of an outgoing propagation `/get` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PropagationRequestKind {
    List,
    Get,
    Acknowledge,
}

/// Reticulum representation selected for one propagation upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropagationUploadRepresentation {
    Packet,
    Resource,
}

/// Why a submitted propagation upload failed before node acceptance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropagationUploadFailure {
    PacketTimeout,
    Resource(ResourceError),
    LinkClosed(LinkCloseReason),
}

impl core::fmt::Display for PropagationUploadFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PacketTimeout => write!(f, "upload packet was not acknowledged"),
            Self::Resource(error) => write!(f, "upload transfer: {error}"),
            // `LinkCloseReason` has no `Display` in leviculum-core; its
            // variant name is the whole reason.
            Self::LinkClosed(reason) => write!(f, "upload link closed ({reason:?})"),
        }
    }
}

/// A verified remote propagation-node announce remembered by the adapter.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownPropagationNode {
    pub destination: DestinationHash,
    pub announce: PropagationNodeAnnounce,
}

/// Transport events relevant to a propagation client.
#[derive(Debug, Clone, PartialEq)]
pub enum PropagationTransportEvent {
    NodeAnnounced(KnownPropagationNode),
    LinkEstablished {
        destination: DestinationHash,
        link_id: LinkId,
    },
    ResponseReceived {
        link_id: LinkId,
        kind: PropagationRequestKind,
        data: Vec<u8>,
    },
    RequestTimedOut {
        link_id: LinkId,
        kind: PropagationRequestKind,
    },
    RequestProgress {
        link_id: LinkId,
        kind: PropagationRequestKind,
        progress: f32,
        transfer_size: u64,
    },
    UploadSubmitted {
        link_id: LinkId,
        message_id: [u8; 32],
        representation: PropagationUploadRepresentation,
    },
    UploadProgress {
        link_id: LinkId,
        message_id: [u8; 32],
        progress: f32,
    },
    UploadCompleted {
        link_id: LinkId,
        message_id: [u8; 32],
    },
    UploadFailed {
        link_id: LinkId,
        message_id: [u8; 32],
        reason: PropagationUploadFailure,
    },
    UploadRejected {
        link_id: LinkId,
        message_id: [u8; 32],
        signal: PropagationSignal,
    },
    LinkClosed {
        link_id: LinkId,
        destination: DestinationHash,
        reason: LinkCloseReason,
    },
}

/// NodeCore actions plus semantic propagation-client events.
#[derive(Debug, Default)]
#[must_use = "propagation output contains NodeCore actions and client events"]
pub struct PropagationTransportOutput {
    pub core: TickOutput,
    pub events: Vec<PropagationTransportEvent>,
}

impl PropagationTransportOutput {
    fn merge(&mut self, mut other: Self) {
        self.core.merge(other.core);
        self.events.append(&mut other.events);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropagationTransportError {
    InvalidDestination,
    Destination(DestinationError),
    UnknownNode,
    LinkUnavailable,
    UploadInProgress,
    Request(RequestError),
    Resource(ResourceError),
    Send(SendError),
}

impl core::fmt::Display for PropagationTransportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidDestination => write!(f, "not a propagation node destination"),
            Self::Destination(error) => write!(f, "propagation destination: {error}"),
            Self::UnknownNode => write!(f, "propagation node is not known"),
            Self::LinkUnavailable => write!(f, "no link to the propagation node"),
            Self::UploadInProgress => write!(f, "an upload is already in progress"),
            Self::Request(error) => write!(f, "propagation request: {error}"),
            Self::Resource(error) => write!(f, "propagation transfer: {error}"),
            Self::Send(error) => write!(f, "propagation send: {error}"),
        }
    }
}

impl core::error::Error for PropagationTransportError {}

impl From<DestinationError> for PropagationTransportError {
    fn from(value: DestinationError) -> Self {
        Self::Destination(value)
    }
}

impl From<RequestError> for PropagationTransportError {
    fn from(value: RequestError) -> Self {
        Self::Request(value)
    }
}

impl From<ResourceError> for PropagationTransportError {
    fn from(value: ResourceError) -> Self {
        Self::Resource(value)
    }
}

impl From<SendError> for PropagationTransportError {
    fn from(value: SendError) -> Self {
        Self::Send(value)
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingRequest {
    kind: PropagationRequestKind,
    link_id: LinkId,
    /// Sender Resource hash while an oversized `/get` request body uploads.
    request_resource_hash: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Copy)]
enum PendingUploadTransfer {
    Packet { packet_hash: [u8; 32] },
    Resource,
}

#[derive(Debug, Clone, Copy)]
struct PendingUpload {
    message_id: [u8; 32],
    transfer: PendingUploadTransfer,
}

/// Live propagation-client transport state.
/// Link-derived inputs for one mailbox upload, captured by
/// [`PropagationTransport::upload_send_params`].
#[must_use = "a dropped capture leaves the upload unsent"]
pub struct UploadSendParams {
    link_id: LinkId,
    message_id: [u8; 32],
    representation: PropagationUploadRepresentation,
    resource: Option<leviculum_core::resource::ResourceSendParams>,
}

impl UploadSendParams {
    /// Phase 2: build the transfer off the caller's lock. A `Packet`-sized
    /// upload has nothing to build and is carried through as-is.
    pub fn build(
        self,
        data: &[u8],
        rng: &mut impl CryptoRngCore,
    ) -> Result<PreparedUpload, PropagationTransportError> {
        let kind = match self.resource {
            None => PreparedUploadKind::Packet(data.to_vec()),
            Some(params) => PreparedUploadKind::Resource(Box::new(
                leviculum_core::resource::prepare_resource_send(&params, data, None, true, rng)?,
            )),
        };
        Ok(PreparedUpload {
            link_id: self.link_id,
            message_id: self.message_id,
            representation: self.representation,
            kind,
        })
    }
}

/// A built upload on its way back to [`PropagationTransport::commit_upload`].
#[must_use = "a dropped upload leaves its message unsent"]
pub struct PreparedUpload {
    link_id: LinkId,
    message_id: [u8; 32],
    representation: PropagationUploadRepresentation,
    kind: PreparedUploadKind,
}

enum PreparedUploadKind {
    Packet(Vec<u8>),
    Resource(Box<leviculum_core::resource::PreparedResourceSend>),
}

pub struct PropagationTransport {
    destination: DestinationHash,
    identity_hash: [u8; 16],
    known_nodes: BTreeMap<DestinationHash, KnownPropagationNode>,
    wanted_links: BTreeSet<DestinationHash>,
    pending_links: BTreeMap<DestinationHash, LinkId>,
    link_destinations: BTreeMap<LinkId, DestinationHash>,
    active_links: BTreeMap<DestinationHash, LinkId>,
    pending_requests: BTreeMap<[u8; 16], PendingRequest>,
    pending_uploads: BTreeMap<LinkId, PendingUpload>,
}

impl PropagationTransport {
    /// Build the local `lxmf.propagation` destination used for Link identity.
    ///
    /// A client destination never accepts incoming links. The same identity is
    /// later presented to a selected remote propagation node with
    /// [`identify`](Self::identify).
    pub fn destination(identity: Identity) -> Result<Destination, DestinationError> {
        let mut destination = Destination::new(
            Some(identity),
            Direction::In,
            DestinationType::Single,
            APP_NAME,
            &[PROPAGATION_ASPECT],
        )?;
        destination.set_accepts_links(false);
        Ok(destination)
    }

    /// Register and take ownership of a local `lxmf.propagation` destination.
    pub fn register<R, C, S>(
        node: &mut NodeCore<R, C, S>,
        mut destination: Destination,
    ) -> Result<Self, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        validate_destination(&destination)?;
        destination.set_accepts_links(false);
        let hash = *destination.hash();
        let identity_hash = *destination
            .identity()
            .ok_or(PropagationTransportError::InvalidDestination)?
            .hash();
        node.register_destination(destination);
        Ok(Self::new(hash, identity_hash))
    }

    /// Attach to an already registered local `lxmf.propagation` destination.
    pub fn attach<R, C, S>(
        node: &mut NodeCore<R, C, S>,
        destination: DestinationHash,
    ) -> Result<Self, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let registered = node
            .destination_mut(&destination)
            .ok_or(PropagationTransportError::InvalidDestination)?;
        validate_destination(registered)?;
        registered.set_accepts_links(false);
        let identity_hash = *registered
            .identity()
            .ok_or(PropagationTransportError::InvalidDestination)?
            .hash();
        Ok(Self::new(destination, identity_hash))
    }

    fn new(destination: DestinationHash, identity_hash: [u8; 16]) -> Self {
        Self {
            destination,
            identity_hash,
            known_nodes: BTreeMap::new(),
            wanted_links: BTreeSet::new(),
            pending_links: BTreeMap::new(),
            link_destinations: BTreeMap::new(),
            active_links: BTreeMap::new(),
            pending_requests: BTreeMap::new(),
            pending_uploads: BTreeMap::new(),
        }
    }

    pub const fn destination_hash(&self) -> DestinationHash {
        self.destination
    }

    /// Identity presented to the remote node and therefore the mailbox owner.
    pub const fn identity_hash(&self) -> [u8; 16] {
        self.identity_hash
    }

    pub fn known_node(&self, destination: &DestinationHash) -> Option<&KnownPropagationNode> {
        self.known_nodes.get(destination)
    }

    pub fn known_nodes(&self) -> impl Iterator<Item = &KnownPropagationNode> {
        self.known_nodes.values()
    }

    pub fn retain_known_nodes(&mut self, destinations: &BTreeSet<DestinationHash>) {
        self.known_nodes
            .retain(|destination, _| destinations.contains(destination));
    }

    pub fn restore_known_node(&mut self, known: KnownPropagationNode) {
        self.known_nodes.insert(known.destination, known);
    }

    pub(crate) fn forget_known_node(&mut self, destination: &DestinationHash) -> bool {
        self.known_nodes.remove(destination).is_some()
    }

    /// Restore or update a propagation node from a previously validated
    /// Reticulum announce. Callers restoring persistent caches must validate
    /// the destination hash and signature before invoking this method.
    pub fn remember_announce(
        &mut self,
        announce: &ReceivedAnnounce,
    ) -> Option<KnownPropagationNode> {
        if announce.name_hash() != &Destination::compute_name_hash(APP_NAME, &[PROPAGATION_ASPECT])
        {
            return None;
        }
        let decoded = PropagationNodeAnnounce::decode(announce.app_data()).ok()?;
        let destination = *announce.destination_hash();
        let known = KnownPropagationNode {
            destination,
            announce: decoded,
        };
        self.known_nodes.insert(destination, known.clone());
        Some(known)
    }

    pub fn active_link(&self, destination: &DestinationHash) -> Option<LinkId> {
        self.active_links.get(destination).copied()
    }

    /// Return whether this adapter owns an outgoing Link.
    pub fn owns_link(&self, link_id: &LinkId) -> bool {
        self.link_destinations.contains_key(link_id)
    }

    /// Return the clear LXMF message ID currently uploading on one Link.
    pub fn active_upload(&self, link_id: &LinkId) -> Option<[u8; 32]> {
        self.pending_uploads
            .get(link_id)
            .map(|pending| pending.message_id)
    }

    /// Return whether a message already has an active upload on any Link.
    pub fn has_active_upload(&self, message_id: &[u8; 32]) -> bool {
        self.pending_uploads
            .values()
            .any(|pending| &pending.message_id == message_id)
    }

    /// Ensure an outgoing Link is being established to the selected node.
    ///
    /// If no path is known, the returned core output contains a path request;
    /// the adapter connects automatically when `PathFound` or a matching
    /// propagation announce is subsequently passed to [`handle_event`](Self::handle_event).
    pub fn ensure_link<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        destination: DestinationHash,
    ) -> Result<PropagationTransportOutput, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if let Some(link_id) = self.active_links.get(&destination).copied() {
            if node.link(&link_id).is_some_and(|link| link.is_active()) {
                return Ok(PropagationTransportOutput::default());
            }
            self.active_links.remove(&destination);
            self.link_destinations.remove(&link_id);
        }
        if self.pending_links.contains_key(&destination) {
            return Ok(PropagationTransportOutput::default());
        }

        self.wanted_links.insert(destination);
        if !node.has_path(&destination) {
            return Ok(PropagationTransportOutput {
                core: node.request_path(&destination),
                events: Vec::new(),
            });
        }
        self.connect(node, destination)
    }

    fn connect<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        destination: DestinationHash,
    ) -> Result<PropagationTransportOutput, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let signing_key = node
            .storage()
            .get_identity(destination.as_bytes())
            .map(|identity| identity.ed25519_verifying().to_bytes())
            .ok_or(PropagationTransportError::UnknownNode)?;
        let (link_id, _, core) = node.connect(destination, &signing_key);
        self.pending_links.insert(destination, link_id);
        self.link_destinations.insert(link_id, destination);
        Ok(PropagationTransportOutput {
            core,
            events: Vec::new(),
        })
    }

    /// Identify the local propagation identity on an established outgoing Link.
    pub fn identify<R, C, S>(
        &self,
        node: &mut NodeCore<R, C, S>,
        link_id: LinkId,
    ) -> Result<TickOutput, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if !self.link_destinations.contains_key(&link_id) {
            return Err(PropagationTransportError::LinkUnavailable);
        }
        let identity = node
            .destination(&self.destination)
            .and_then(Destination::identity)
            .cloned()
            .ok_or(PropagationTransportError::InvalidDestination)?;
        node.identify_link(&link_id, &identity)
            .map_err(|_| PropagationTransportError::LinkUnavailable)
    }

    /// Submit one encoded [`crate::propagation::PropagationUpload`] on an
    /// established propagation Link.
    ///
    /// Python LXMF uses a raw Link packet up to
    /// [`LINK_PACKET_MAX_CONTENT`] bytes and a Resource above that protocol
    /// threshold. Only one upload may be active per Link, and the same clear
    /// message ID cannot be correlated to multiple Links simultaneously.
    pub fn submit_upload<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        link_id: LinkId,
        message_id: [u8; 32],
        data: &[u8],
    ) -> Result<PropagationTransportOutput, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if !self.link_destinations.contains_key(&link_id)
            || !node.link(&link_id).is_some_and(|link| link.is_active())
        {
            return Err(PropagationTransportError::LinkUnavailable);
        }
        if self.pending_uploads.contains_key(&link_id) || self.has_active_upload(&message_id) {
            return Err(PropagationTransportError::UploadInProgress);
        }

        let representation = upload_representation(data.len());
        let (transfer, core) = match representation {
            PropagationUploadRepresentation::Packet => {
                let (packet_hash, core) = node.send_packet_on_link(&link_id, data)?;
                (PendingUploadTransfer::Packet { packet_hash }, core)
            }
            PropagationUploadRepresentation::Resource => {
                let (_, core) = node.send_resource(&link_id, data, None, true)?;
                (PendingUploadTransfer::Resource, core)
            }
        };
        Ok(self.record_upload(link_id, message_id, representation, transfer, core))
    }

    /// Phase 1 of an off-lock upload: validate the link exactly as
    /// [`submit_upload`](Self::submit_upload) does, then snapshot what the
    /// build needs.
    ///
    /// A `Packet`-sized upload carries no build, so its params hold nothing and
    /// [`commit_upload`](Self::commit_upload) sends it there instead.
    pub fn upload_send_params<R, C, S>(
        &self,
        node: &NodeCore<R, C, S>,
        link_id: LinkId,
        message_id: [u8; 32],
        data_len: usize,
    ) -> Result<UploadSendParams, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if !self.link_destinations.contains_key(&link_id)
            || !node.link(&link_id).is_some_and(|link| link.is_active())
        {
            return Err(PropagationTransportError::LinkUnavailable);
        }
        if self.pending_uploads.contains_key(&link_id) || self.has_active_upload(&message_id) {
            return Err(PropagationTransportError::UploadInProgress);
        }
        let representation = upload_representation(data_len);
        let resource = match representation {
            PropagationUploadRepresentation::Packet => None,
            PropagationUploadRepresentation::Resource => Some(node.resource_send_params(&link_id)?),
        };
        Ok(UploadSendParams {
            link_id,
            message_id,
            representation,
            resource,
        })
    }

    /// Phase 3: put the built upload on the link and record it.
    pub fn commit_upload<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        prepared: PreparedUpload,
    ) -> Result<PropagationTransportOutput, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let PreparedUpload {
            link_id,
            message_id,
            representation,
            kind,
        } = prepared;
        let (transfer, core) = match kind {
            PreparedUploadKind::Packet(data) => {
                let (packet_hash, core) = node.send_packet_on_link(&link_id, &data)?;
                (PendingUploadTransfer::Packet { packet_hash }, core)
            }
            PreparedUploadKind::Resource(resource) => {
                let (_, core) = node.commit_resource_send(*resource)?;
                (PendingUploadTransfer::Resource, core)
            }
        };
        Ok(self.record_upload(link_id, message_id, representation, transfer, core))
    }

    fn record_upload(
        &mut self,
        link_id: LinkId,
        message_id: [u8; 32],
        representation: PropagationUploadRepresentation,
        transfer: PendingUploadTransfer,
        core: TickOutput,
    ) -> PropagationTransportOutput {
        self.pending_uploads.insert(
            link_id,
            PendingUpload {
                message_id,
                transfer,
            },
        );
        PropagationTransportOutput {
            core,
            events: alloc::vec![PropagationTransportEvent::UploadSubmitted {
                link_id,
                message_id,
                representation,
            }],
        }
    }

    /// Submit one typed `/get` mailbox request and remember its correlation.
    ///
    /// The three protocol phases share the same wire path and transport
    /// mechanics; `kind` only selects the semantic event returned later.
    pub fn submit_mailbox_request<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        link_id: LinkId,
        data: &[u8],
        kind: PropagationRequestKind,
        timeout_ms: Option<u64>,
    ) -> Result<TickOutput, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if !self.link_destinations.contains_key(&link_id)
            || !node.link(&link_id).is_some_and(|link| link.is_active())
        {
            return Err(PropagationTransportError::LinkUnavailable);
        }
        let (request_id, request_resource_hash, output) =
            match node.send_request(&link_id, MESSAGE_GET_PATH, Some(data), timeout_ms) {
                Ok((request_id, output)) => (request_id, None, output),
                Err(RequestError::PayloadTooLarge) => {
                    let (request_id, resource_hash, output) = node
                        .send_request_resource(&link_id, MESSAGE_GET_PATH, Some(data), timeout_ms)
                        .map_err(PropagationTransportError::Resource)?;
                    (request_id, Some(resource_hash), output)
                }
                Err(error) => return Err(PropagationTransportError::Request(error)),
            };
        self.pending_requests.insert(
            request_id,
            PendingRequest {
                kind,
                link_id,
                request_resource_hash,
            },
        );
        Ok(output)
    }

    /// Close and deselect an outgoing propagation Link.
    pub fn close<R, C, S>(&mut self, node: &mut NodeCore<R, C, S>, link_id: LinkId) -> TickOutput
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if let Some(destination) = self.link_destinations.get(&link_id) {
            self.wanted_links.remove(destination);
        }
        node.close_link(&link_id)
    }

    /// Cancel path discovery or Link establishment for one remote node.
    pub fn cancel<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        destination: DestinationHash,
    ) -> TickOutput
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        self.wanted_links.remove(&destination);
        self.active_links
            .get(&destination)
            .or_else(|| self.pending_links.get(&destination))
            .copied()
            .map_or_else(TickOutput::default, |link_id| node.close_link(&link_id))
    }

    /// Map a NodeCore event into propagation-client state and events.
    pub fn handle_event<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        event: &NodeEvent,
    ) -> Result<PropagationTransportOutput, PropagationTransportError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let mut output = PropagationTransportOutput::default();
        match event {
            NodeEvent::AnnounceReceived { announce, .. }
                if announce.name_hash()
                    == &Destination::compute_name_hash(APP_NAME, &[PROPAGATION_ASPECT]) =>
            {
                let destination = *announce.destination_hash();
                if let Some(known) = self.remember_announce(announce) {
                    output
                        .events
                        .push(PropagationTransportEvent::NodeAnnounced(known));
                }

                if self.wanted_links.contains(&destination)
                    && !self.pending_links.contains_key(&destination)
                    && node.has_path(&destination)
                {
                    output.merge(self.connect(node, destination)?);
                }
            }
            NodeEvent::PathFound {
                destination_hash, ..
            } if self.wanted_links.contains(destination_hash)
                && !self.pending_links.contains_key(destination_hash) =>
            {
                output.merge(self.connect(node, *destination_hash)?);
            }
            NodeEvent::LinkEstablished {
                link_id,
                is_initiator: true,
                ..
            } => {
                let destination = self.link_destinations.get(link_id).copied().or_else(|| {
                    node.link(link_id)
                        .map(|link| *link.destination_hash())
                        .filter(|destination| self.wanted_links.contains(destination))
                });
                if let Some(destination) = destination {
                    self.pending_links.remove(&destination);
                    self.wanted_links.remove(&destination);
                    self.active_links.insert(destination, *link_id);
                    self.link_destinations
                        .retain(|known_link, known_destination| {
                            *known_link == *link_id || *known_destination != destination
                        });
                    self.link_destinations.insert(*link_id, destination);
                    output
                        .events
                        .push(PropagationTransportEvent::LinkEstablished {
                            destination,
                            link_id: *link_id,
                        });
                }
            }
            NodeEvent::ResponseReceived {
                link_id,
                request_id,
                response_data,
                ..
            } => {
                let matches_link = self
                    .pending_requests
                    .get(request_id)
                    .is_some_and(|pending| pending.link_id == *link_id);
                if matches_link {
                    if let Some(pending) = self.pending_requests.remove(request_id) {
                        output
                            .events
                            .push(PropagationTransportEvent::ResponseReceived {
                                link_id: *link_id,
                                kind: pending.kind,
                                data: response_data.clone(),
                            });
                    }
                }
            }
            NodeEvent::RequestTimedOut {
                link_id,
                request_id,
            } => {
                let matches_link = self
                    .pending_requests
                    .get(request_id)
                    .is_some_and(|pending| pending.link_id == *link_id);
                if matches_link {
                    if let Some(pending) = self.pending_requests.remove(request_id) {
                        output
                            .events
                            .push(PropagationTransportEvent::RequestTimedOut {
                                link_id: *link_id,
                                kind: pending.kind,
                            });
                    }
                }
            }
            NodeEvent::LinkDeliveryConfirmed {
                link_id,
                packet_hash,
            } => {
                let matches_packet = self.pending_uploads.get(link_id).is_some_and(|pending| {
                    matches!(
                        pending.transfer,
                        PendingUploadTransfer::Packet {
                            packet_hash: pending_hash,
                            ..
                        } if pending_hash == *packet_hash
                    )
                });
                if matches_packet {
                    if let Some(pending) = self.pending_uploads.remove(link_id) {
                        output
                            .events
                            .push(PropagationTransportEvent::UploadCompleted {
                                link_id: *link_id,
                                message_id: pending.message_id,
                            });
                    }
                }
            }
            NodeEvent::LinkDeliveryFailed {
                link_id,
                packet_hash,
            } => {
                let matches_packet = self.pending_uploads.get(link_id).is_some_and(|pending| {
                    matches!(
                        pending.transfer,
                        PendingUploadTransfer::Packet {
                            packet_hash: pending_hash,
                        } if pending_hash == *packet_hash
                    )
                });
                if matches_packet {
                    if let Some(pending) = self.pending_uploads.remove(link_id) {
                        output.events.push(PropagationTransportEvent::UploadFailed {
                            link_id: *link_id,
                            message_id: pending.message_id,
                            reason: PropagationUploadFailure::PacketTimeout,
                        });
                        // Python uses the same LXMessage packet-receipt
                        // callback for direct and propagated sends, and tears
                        // down the propagation Link on timeout.
                        output.core.merge(node.close_link(link_id));
                    }
                }
            }
            NodeEvent::LinkDataReceived { link_id, data } => {
                if let Ok(signal) = PropagationSignal::decode(data) {
                    if let Some(pending) = self.pending_uploads.remove(link_id) {
                        output
                            .events
                            .push(PropagationTransportEvent::UploadRejected {
                                link_id: *link_id,
                                message_id: pending.message_id,
                                signal,
                            });
                    }
                }
            }
            NodeEvent::ResourceProgress {
                link_id,
                progress,
                is_sender: true,
                ..
            } => {
                if let Some(pending) = self
                    .pending_uploads
                    .get(link_id)
                    .filter(|pending| matches!(pending.transfer, PendingUploadTransfer::Resource))
                {
                    output
                        .events
                        .push(PropagationTransportEvent::UploadProgress {
                            link_id: *link_id,
                            message_id: pending.message_id,
                            progress: *progress,
                        });
                }
            }
            NodeEvent::ResourceProgress {
                link_id,
                progress,
                transfer_size,
                is_sender: false,
                ..
            } => {
                if let Some(pending) = self.pending_requests.values().find(|pending| {
                    pending.link_id == *link_id
                        && pending.kind != PropagationRequestKind::Acknowledge
                }) {
                    output
                        .events
                        .push(PropagationTransportEvent::RequestProgress {
                            link_id: *link_id,
                            kind: pending.kind,
                            progress: *progress,
                            transfer_size: *transfer_size,
                        });
                }
            }
            NodeEvent::ResourceCompleted {
                link_id,
                resource_hash,
                is_sender: true,
                segment_index,
                total_segments,
                ..
            } => {
                if segment_index == total_segments
                    && self.pending_uploads.get(link_id).is_some_and(|pending| {
                        matches!(pending.transfer, PendingUploadTransfer::Resource)
                    })
                {
                    if let Some(pending) = self.pending_uploads.remove(link_id) {
                        output
                            .events
                            .push(PropagationTransportEvent::UploadCompleted {
                                link_id: *link_id,
                                message_id: pending.message_id,
                            });
                    }
                }
                if let Some(pending) = self.pending_requests.values_mut().find(|pending| {
                    pending.link_id == *link_id
                        && pending.request_resource_hash == Some(*resource_hash)
                }) {
                    // Upload succeeded. Core now owns the response deadline and
                    // the Resource hash must no longer match a later transfer.
                    pending.request_resource_hash = None;
                }
            }
            NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error,
                is_sender: true,
                ..
            } => {
                if self.pending_uploads.get(link_id).is_some_and(|pending| {
                    matches!(pending.transfer, PendingUploadTransfer::Resource)
                }) {
                    if let Some(pending) = self.pending_uploads.remove(link_id) {
                        output.events.push(PropagationTransportEvent::UploadFailed {
                            link_id: *link_id,
                            message_id: pending.message_id,
                            reason: PropagationUploadFailure::Resource(*error),
                        });
                    }
                }
                let request_id = self
                    .pending_requests
                    .iter()
                    .find_map(|(request_id, pending)| {
                        (pending.link_id == *link_id
                            && pending.request_resource_hash == Some(*resource_hash))
                        .then_some(*request_id)
                    });
                if let Some(request_id) = request_id {
                    if let Some(pending) = self.pending_requests.remove(&request_id) {
                        // The client API has one request-failure signal. The
                        // ResourceFailed NodeEvent carries the precise reason;
                        // this event invokes the same state-machine path as a
                        // failed Python RequestReceipt.
                        output
                            .events
                            .push(PropagationTransportEvent::RequestTimedOut {
                                link_id: *link_id,
                                kind: pending.kind,
                            });
                    }
                }
            }
            NodeEvent::LinkClosed {
                link_id, reason, ..
            } => {
                if let Some(destination) = self.link_destinations.remove(link_id) {
                    self.wanted_links.remove(&destination);
                    self.pending_links.retain(|_, pending| pending != link_id);
                    self.active_links.retain(|_, active| active != link_id);
                    self.pending_requests
                        .retain(|_, pending| pending.link_id != *link_id);
                    if let Some(pending) = self.pending_uploads.remove(link_id) {
                        output.events.push(PropagationTransportEvent::UploadFailed {
                            link_id: *link_id,
                            message_id: pending.message_id,
                            reason: PropagationUploadFailure::LinkClosed(*reason),
                        });
                    }
                    output.events.push(PropagationTransportEvent::LinkClosed {
                        link_id: *link_id,
                        destination,
                        reason: *reason,
                    });
                }
            }
            _ => {}
        }
        Ok(output)
    }
}

pub(crate) const fn upload_representation(data_len: usize) -> PropagationUploadRepresentation {
    if data_len <= LINK_PACKET_MAX_CONTENT {
        PropagationUploadRepresentation::Packet
    } else {
        PropagationUploadRepresentation::Resource
    }
}

fn validate_destination(destination: &Destination) -> Result<(), PropagationTransportError> {
    if destination.direction() != Direction::In
        || destination.dest_type() != DestinationType::Single
        || destination.name_hash()
            != &Destination::compute_name_hash(APP_NAME, &[PROPAGATION_ASPECT])
        || destination.identity().is_none()
    {
        return Err(PropagationTransportError::InvalidDestination);
    }
    Ok(())
}

#[cfg(test)]
#[path = "propagation_client_tests.rs"]
mod tests;
