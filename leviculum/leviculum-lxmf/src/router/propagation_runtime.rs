//! Cooperative propagation client for mailbox synchronisation and uploads.
//!
//! This module deliberately does not implement propagation-node hosting,
//! message transit storage, `/offer`, or peer synchronisation. It drives the
//! Python-compatible `/get` mailbox flow and originator Packet/Resource uploads.

use alloc::{collections::BTreeSet, vec::Vec};

use leviculum_core::{
    crypto::full_hash, Clock, DestinationHash, LinkId, NodeCore, ReceivedAnnounce, ResourceError,
    SendError, Storage, TickOutput,
};
use rand_core::CryptoRngCore;

use super::{
    unpack_local, DeliveryMethod, LxmfNodeError, LxmfRouter, OutboundPropagation,
    PropagationStampRequest, RouterError, RouterEvent, RouterOutput,
};
use crate::{
    propagation::{
        MessageGetRequest, MessageGetResponse, MessageListResponse, PeerError, PropagatedMessage,
        PropagationUpload, TransferLimit, TransientId,
    },
    propagation_client::{
        upload_representation, KnownPropagationNode, PropagationRequestKind, PropagationTransport,
        PropagationTransportError, PropagationTransportEvent, PropagationUploadFailure,
        PropagationUploadRepresentation,
    },
};

/// Python `LXMRouter.PR_PATH_TIMEOUT`, expressed for the sans-I/O clock.
pub const PROPAGATION_PATH_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropagationClientConfig {
    /// Do not tell the node to purge messages already present locally.
    pub retain_synced_on_node: bool,
    /// Maximum `/get` response payload requested from the remote node, in
    /// kilobytes. Python sends its `delivery_per_transfer_limit` here
    /// (`reference/LXMF/LXMF/LXMRouter.py:1592`), default
    /// [`DELIVERY_LIMIT_KB`](crate::constants::DELIVERY_LIMIT_KB).
    pub delivery_transfer_limit_kb: u64,
    /// Maximum time to wait for a path before reporting [`PropagationClientState::NoPath`].
    pub path_timeout_ms: u64,
}

impl Default for PropagationClientConfig {
    fn default() -> Self {
        Self {
            retain_synced_on_node: false,
            delivery_transfer_limit_kb: crate::constants::DELIVERY_LIMIT_KB,
            path_timeout_ms: PROPAGATION_PATH_TIMEOUT_MS,
        }
    }
}

/// Values are wire-compatible with Python `LXMRouter.PR_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PropagationClientState {
    Idle = 0x00,
    PathRequested = 0x01,
    LinkEstablishing = 0x02,
    LinkEstablished = 0x03,
    RequestSent = 0x04,
    Receiving = 0x05,
    ResponseReceived = 0x06,
    Complete = 0x07,
    NoPath = 0xf0,
    LinkFailed = 0xf1,
    TransferFailed = 0xf2,
    NoIdentity = 0xf3,
    NoAccess = 0xf4,
    Failed = 0xfe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropagationSyncResult {
    /// Number of encrypted messages returned by the node.
    pub received: usize,
    /// Number whose transient IDs had already been processed locally.
    pub duplicates: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PropagationSyncStatus {
    pub state: PropagationClientState,
    pub progress: f32,
    pub transfer_size: Option<u64>,
}

impl PropagationSyncStatus {
    const fn idle() -> Self {
        Self {
            state: PropagationClientState::Idle,
            progress: 0.0,
            transfer_size: None,
        }
    }
}

#[derive(Debug)]
struct MailboxSync {
    state: PropagationClientState,
    max_messages: Option<usize>,
    progress: f32,
    transfer_size: Option<u64>,
    received: usize,
    duplicates: usize,
    path_deadline_ms: Option<u64>,
}

impl Default for MailboxSync {
    fn default() -> Self {
        Self {
            state: PropagationClientState::Idle,
            max_messages: None,
            progress: 0.0,
            transfer_size: None,
            received: 0,
            duplicates: 0,
            path_deadline_ms: None,
        }
    }
}

pub(super) struct PropagationRuntime {
    transport: PropagationTransport,
    config: PropagationClientConfig,
    outbound_node: Option<DestinationHash>,
    client: MailboxSync,
}

impl PropagationRuntime {
    pub(super) fn new(transport: PropagationTransport, config: PropagationClientConfig) -> Self {
        Self {
            transport,
            config,
            outbound_node: None,
            client: MailboxSync::default(),
        }
    }

    pub(super) fn into_transport(self) -> PropagationTransport {
        self.transport
    }

    pub(super) const fn outbound_node(&self) -> Option<DestinationHash> {
        self.outbound_node
    }

    pub(super) fn owns_link(&self, link_id: &LinkId) -> bool {
        self.transport.owns_link(link_id)
    }

    fn selected_stamp_cost(&self) -> Option<u8> {
        let destination = self.outbound_node?;
        let cost = self.transport.known_node(&destination)?.announce.stamp_cost;
        u8::try_from(cost).ok()
    }

    pub(super) fn ensure_prepared_upload<R, C, S>(
        &self,
        router: &mut LxmfRouter,
        node: &mut NodeCore<R, C, S>,
        message_id: [u8; 32],
        now_unix: f64,
    ) -> Result<Option<PropagationStampRequest>, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if !now_unix.is_finite() {
            return Err(RouterError::CorruptSnapshot);
        }
        let target_cost = self.selected_stamp_cost();
        let entry = router
            .outbound
            .get_mut(&message_id)
            .ok_or(RouterError::NotFound)?;
        if entry.message().method != DeliveryMethod::Propagated {
            return Err(RouterError::UnsupportedMethod);
        }

        if entry.propagation().is_none() {
            let packed = entry.message().pack();
            let encrypted = node
                .encrypt_for_destination(
                    &DestinationHash::new(entry.message().destination_hash),
                    &packed[16..],
                )
                .map_err(LxmfNodeError::Send)?;
            let mut unstamped_lxmf = Vec::with_capacity(16 + encrypted.len());
            unstamped_lxmf.extend_from_slice(&entry.message().destination_hash);
            unstamped_lxmf.extend_from_slice(&encrypted);
            let transient_id = full_hash(&unstamped_lxmf);
            entry.set_propagation(
                Some(OutboundPropagation {
                    timebase: now_unix,
                    unstamped_lxmf,
                    transient_id,
                    target_cost,
                    stamp: None,
                }),
                &mut router.build_epochs,
            );
            router.persistence_dirty = true;
        } else if entry.retarget_propagation(target_cost, &mut router.build_epochs) {
            router.persistence_dirty = true;
        }

        let prepared = entry
            .propagation()
            .ok_or(RouterError::PropagationStampUnavailable)?;
        Ok(match (prepared.stamp, prepared.target_cost) {
            (None, Some(target_cost)) => Some(PropagationStampRequest {
                message_id,
                transient_id: prepared.transient_id,
                target_cost,
            }),
            _ => None,
        })
    }

    pub(super) fn handle_event<R, C, S>(
        &mut self,
        router: &mut LxmfRouter,
        node: &mut NodeCore<R, C, S>,
        event: &leviculum_core::NodeEvent,
        now_unix: f64,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let transport = self.transport.handle_event(node, event)?;
        let mut output = RouterOutput {
            core: transport.core,
            events: Vec::new(),
        };
        if let leviculum_core::NodeEvent::PathFound {
            destination_hash, ..
        } = event
        {
            if self.outbound_node.as_ref() == Some(destination_hash)
                && self.client.state == PropagationClientState::PathRequested
            {
                self.client.path_deadline_ms = None;
                self.set_state(PropagationClientState::LinkEstablishing, &mut output);
            }
        }
        for event in transport.events {
            self.handle_transport_event(router, node, event, now_unix, &mut output)?;
        }
        Ok(output)
    }

    fn handle_transport_event<R, C, S>(
        &mut self,
        router: &mut LxmfRouter,
        node: &mut NodeCore<R, C, S>,
        event: PropagationTransportEvent,
        now_unix: f64,
        output: &mut RouterOutput,
    ) -> Result<(), RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        match event {
            PropagationTransportEvent::NodeAnnounced(known) => {
                let destination = known.destination;
                if self.outbound_node == Some(destination)
                    && self.client.state == PropagationClientState::PathRequested
                {
                    self.client.path_deadline_ms = None;
                    self.set_state(PropagationClientState::LinkEstablishing, output);
                    if let Some(link_id) = self.transport.active_link(&destination) {
                        self.begin_list_request(node, link_id, output)?;
                    }
                }
            }
            PropagationTransportEvent::LinkEstablished {
                destination,
                link_id,
            } => {
                if self.outbound_node == Some(destination) {
                    if sync_is_active(self.client.state) {
                        self.begin_list_request(node, link_id, output)?;
                    }
                    // Python's established callback immediately re-enters
                    // outbound processing. The link-establishment attempt was
                    // already charged, so make queued origin uploads due
                    // without charging or waiting for PATH_REQUEST_WAIT again.
                    let now_ms = node.now_ms();
                    let mut changed = false;
                    for entry in router.outbound.values_mut().filter(|entry| {
                        entry.message().method == DeliveryMethod::Propagated
                            && entry.state() == super::MessageState::Outbound
                    }) {
                        if entry.next_attempt_ms != now_ms {
                            entry.next_attempt_ms = now_ms;
                            changed = true;
                        }
                    }
                    router.persistence_dirty |= changed;
                }
            }
            PropagationTransportEvent::ResponseReceived { kind, data, .. } => match kind {
                PropagationRequestKind::List => {
                    self.handle_list_response(router, node, &data, output)?;
                }
                PropagationRequestKind::Get => {
                    self.handle_get_response(router, node, &data, now_unix, output)?;
                }
                PropagationRequestKind::Acknowledge => {
                    // Python does not wait for the purge response.
                }
            },
            PropagationTransportEvent::RequestTimedOut { kind, .. } => {
                if matches!(
                    kind,
                    PropagationRequestKind::List | PropagationRequestKind::Get
                ) {
                    self.set_state(PropagationClientState::TransferFailed, output);
                    self.cancel_outbound_link(node, output);
                } else if kind == PropagationRequestKind::Acknowledge {
                    self.cancel_outbound_link(node, output);
                }
            }
            PropagationTransportEvent::RequestProgress {
                kind,
                progress,
                transfer_size,
                ..
            } => {
                if kind == PropagationRequestKind::Get {
                    let changed = self.client.progress.to_bits() != progress.to_bits()
                        || self.client.transfer_size != Some(transfer_size);
                    self.client.progress = progress;
                    self.client.transfer_size = Some(transfer_size);
                    if self.client.state == PropagationClientState::Receiving {
                        if changed {
                            self.emit_sync_status(output);
                        }
                    } else {
                        self.set_state(PropagationClientState::Receiving, output);
                    }
                }
            }
            PropagationTransportEvent::UploadSubmitted { message_id, .. } => {
                if let Some(entry) = router.outbound.get_mut(&message_id) {
                    // Establishing the propagation link already charged this logical
                    // attempt. Submitting on that link is part of the same attempt.
                    entry.set_state(super::MessageState::Sending, &mut router.build_epochs);
                    entry.next_attempt_ms =
                        node.now_ms().saturating_add(super::DELIVERY_RETRY_WAIT_MS);
                    entry.progress = 0.01;
                    router.persistence_dirty = true;
                }
            }
            PropagationTransportEvent::UploadProgress {
                message_id,
                progress,
                ..
            } => {
                if let Some(entry) = router.outbound.get_mut(&message_id) {
                    let progress = 0.1 + progress.clamp(0.0, 1.0) * 0.9;
                    if entry.progress.to_bits() != progress.to_bits() {
                        entry.progress = progress;
                        router.persistence_dirty = true;
                    }
                }
            }
            PropagationTransportEvent::UploadCompleted { message_id, .. } => {
                if router.remove_outbound(&message_id).is_some() {
                    router.persistence_dirty = true;
                    // The node has it, the recipient does not. Reporting
                    // `Sent` here would make this indistinguishable from a
                    // message handed to the recipient's own destination, and
                    // no later event corrects it: a collection from the
                    // mailbox is not signalled to the origin. The queue entry
                    // still goes — this leg of delivery is over and nothing
                    // is retried — so the state travels on the event.
                    output.events.push(RouterEvent::MessageState {
                        message_id,
                        state: super::MessageState::AwaitingCollection,
                    });
                }
            }
            PropagationTransportEvent::UploadFailed {
                link_id,
                message_id,
                reason,
            } => {
                // Python tears down a propagation link after a packet receipt
                // timeout or failed resource transfer. The subsequent retry must
                // establish a fresh link. A LinkClosed failure is already the
                // teardown notification and must not be closed a second time.
                if matches!(
                    reason,
                    PropagationUploadFailure::PacketTimeout | PropagationUploadFailure::Resource(_)
                ) {
                    output.core.merge(self.transport.close(node, link_id));
                }
                if let Some(entry) = router.outbound.get_mut(&message_id) {
                    entry.set_state(super::MessageState::Outbound, &mut router.build_epochs);
                    entry.next_attempt_ms =
                        node.now_ms().saturating_add(super::DELIVERY_RETRY_WAIT_MS);
                    entry.progress = 0.01;
                    router.persistence_dirty = true;
                }
            }
            PropagationTransportEvent::UploadRejected {
                link_id,
                message_id,
                ..
            } => {
                if router.remove_outbound(&message_id).is_some() {
                    router.persistence_dirty = true;
                    output.events.push(RouterEvent::MessageState {
                        message_id,
                        state: super::MessageState::Rejected,
                    });
                }
                output.core.merge(self.transport.close(node, link_id));
            }
            PropagationTransportEvent::LinkClosed { destination, .. } => {
                if self.outbound_node == Some(destination) {
                    match self.client.state {
                        PropagationClientState::Complete => {
                            self.client.state = PropagationClientState::Idle;
                            self.client.progress = 0.0;
                            self.client.transfer_size = None;
                            self.emit_sync_status(output);
                        }
                        PropagationClientState::PathRequested
                        | PropagationClientState::LinkEstablishing => {
                            self.set_state(PropagationClientState::LinkFailed, output);
                        }
                        PropagationClientState::LinkEstablished
                        | PropagationClientState::RequestSent
                        | PropagationClientState::Receiving
                        | PropagationClientState::ResponseReceived => {
                            self.set_state(PropagationClientState::TransferFailed, output);
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn begin_list_request<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        link_id: LinkId,
        output: &mut RouterOutput,
    ) -> Result<(), RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        self.client.path_deadline_ms = None;
        self.set_state(PropagationClientState::LinkEstablished, output);
        output.core.merge(self.transport.identify(node, link_id)?);
        let request = MessageGetRequest::list().encode()?;
        output.core.merge(self.transport.submit_mailbox_request(
            node,
            link_id,
            &request,
            PropagationRequestKind::List,
            None,
        )?);
        self.set_state(PropagationClientState::RequestSent, output);
        Ok(())
    }

    fn handle_list_response<R, C, S>(
        &mut self,
        router: &LxmfRouter,
        node: &mut NodeCore<R, C, S>,
        data: &[u8],
        output: &mut RouterOutput,
    ) -> Result<(), RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let response = match MessageListResponse::decode(data) {
            Ok(response) => response,
            Err(_) => {
                self.set_state(PropagationClientState::TransferFailed, output);
                self.cancel_outbound_link(node, output);
                return Ok(());
            }
        };
        let ids = match response {
            MessageListResponse::TransientIds(ids) => ids,
            MessageListResponse::Error(error) => {
                self.handle_remote_error(error, node, output);
                return Ok(());
            }
        };
        if ids.is_empty() {
            self.complete(0, 0, output);
            return Ok(());
        }

        let mut wants = Vec::new();
        let mut haves = Vec::new();
        for id in ids {
            if router.has_message(&id) {
                if !self.config.retain_synced_on_node {
                    haves.push(id);
                }
            } else if self
                .client
                .max_messages
                .is_none_or(|maximum| wants.len() < maximum)
            {
                wants.push(id);
            }
        }

        let request = MessageGetRequest {
            wants: Some(wants),
            haves: Some(haves),
            transfer_limit_kb: Some(TransferLimit::Integer(
                self.config.delivery_transfer_limit_kb,
            )),
        }
        .encode()?;
        let Some(link_id) = self.active_link() else {
            self.set_state(PropagationClientState::LinkFailed, output);
            return Ok(());
        };
        output.core.merge(self.transport.submit_mailbox_request(
            node,
            link_id,
            &request,
            PropagationRequestKind::Get,
            None,
        )?);
        self.set_state(PropagationClientState::RequestSent, output);
        Ok(())
    }

    fn handle_get_response<R, C, S>(
        &mut self,
        router: &mut LxmfRouter,
        node: &mut NodeCore<R, C, S>,
        data: &[u8],
        now_unix: f64,
        output: &mut RouterOutput,
    ) -> Result<(), RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let response = match MessageGetResponse::decode(data) {
            Ok(response) => response,
            Err(_) => {
                self.set_state(PropagationClientState::TransferFailed, output);
                self.cancel_outbound_link(node, output);
                return Ok(());
            }
        };
        let messages = match response {
            MessageGetResponse::Messages(messages) => messages,
            MessageGetResponse::Error(error) => {
                self.handle_remote_error(error, node, output);
                return Ok(());
            }
        };

        let mut haves = Vec::with_capacity(messages.len());
        let mut duplicates = 0;
        for bytes in &messages {
            let transient_id = full_hash(bytes);
            haves.push(transient_id);
            if let Ok(true) = deliver_unstamped(router, node, bytes, now_unix, output) {
                duplicates += 1;
            }
        }

        if !haves.is_empty() {
            if let Some(link_id) = self.active_link() {
                let acknowledgement = MessageGetRequest::acknowledge(haves).encode()?;
                output.core.merge(self.transport.submit_mailbox_request(
                    node,
                    link_id,
                    &acknowledgement,
                    PropagationRequestKind::Acknowledge,
                    None,
                )?);
            }
        }
        self.complete(messages.len(), duplicates, output);
        Ok(())
    }

    fn handle_remote_error<R, C, S>(
        &mut self,
        error: PeerError,
        node: &mut NodeCore<R, C, S>,
        output: &mut RouterOutput,
    ) where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let state = match error {
            PeerError::NoIdentity => PropagationClientState::NoIdentity,
            PeerError::NoAccess => PropagationClientState::NoAccess,
            _ => PropagationClientState::TransferFailed,
        };
        self.set_state(state, output);
        self.cancel_outbound_link(node, output);
    }

    fn complete(&mut self, received: usize, duplicates: usize, output: &mut RouterOutput) {
        self.client.received = received;
        self.client.duplicates = duplicates;
        self.client.progress = 1.0;
        self.set_state(PropagationClientState::Complete, output);
        output.events.push(RouterEvent::PropagationSyncComplete(
            PropagationSyncResult {
                received,
                duplicates,
            },
        ));
    }

    fn active_link(&self) -> Option<LinkId> {
        self.outbound_node
            .as_ref()
            .and_then(|destination| self.transport.active_link(destination))
    }

    fn cancel_outbound_link<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        output: &mut RouterOutput,
    ) where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if let Some(destination) = self.outbound_node {
            output.core.merge(self.transport.cancel(node, destination));
        }
    }

    pub(super) fn cancel_outbound_message<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        message_id: &[u8; 32],
    ) -> TickOutput
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        if !self.transport.has_active_upload(message_id) {
            return TickOutput::default();
        }
        self.active_link()
            .map_or_else(TickOutput::default, |link_id| {
                self.transport.close(node, link_id)
            })
    }

    fn set_state(&mut self, state: PropagationClientState, output: &mut RouterOutput) {
        if matches!(
            state,
            PropagationClientState::NoPath
                | PropagationClientState::LinkFailed
                | PropagationClientState::TransferFailed
                | PropagationClientState::NoIdentity
                | PropagationClientState::NoAccess
                | PropagationClientState::Failed
        ) {
            self.client.progress = 0.0;
            self.client.path_deadline_ms = None;
        }
        if self.client.state != state {
            self.client.state = state;
            self.emit_sync_status(output);
        }
    }

    fn sync_status(&self) -> PropagationSyncStatus {
        PropagationSyncStatus {
            state: self.client.state,
            progress: self.client.progress,
            transfer_size: self.client.transfer_size,
        }
    }

    fn emit_sync_status(&self, output: &mut RouterOutput) {
        output
            .events
            .push(RouterEvent::PropagationSyncState(self.sync_status()));
    }

    /// Advances the path wait without blocking an executor or event loop.
    pub(super) fn tick_client<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let mut output = RouterOutput::default();
        if self.client.state != PropagationClientState::PathRequested {
            return Ok(output);
        }
        let Some(destination) = self.outbound_node else {
            self.set_state(PropagationClientState::Failed, &mut output);
            return Ok(output);
        };
        if node.has_path(&destination) {
            self.client.path_deadline_ms = None;
            self.set_state(PropagationClientState::LinkEstablishing, &mut output);
            output
                .core
                .merge(self.transport.ensure_link(node, destination)?.core);
        } else if self
            .client
            .path_deadline_ms
            .is_some_and(|deadline| node.now_ms() >= deadline)
        {
            self.client.path_deadline_ms = None;
            output.core.merge(self.transport.cancel(node, destination));
            self.set_state(PropagationClientState::NoPath, &mut output);
        } else if let Some(deadline) = self.client.path_deadline_ms {
            output.core.next_deadline_ms = Some(deadline);
        }
        Ok(output)
    }

    /// Advance upload receipt timeouts, mailbox synchronisation and one
    /// serialized originator upload without blocking the protocol loop.
    pub(super) fn tick<R, C, S>(
        &mut self,
        router: &mut LxmfRouter,
        node: &mut NodeCore<R, C, S>,
        now_unix: f64,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let mut output = RouterOutput::default();
        output.core.merge(self.refresh_orphaned_nodes(node));
        output.merge(self.tick_client(node)?);
        output.merge(self.tick_outbound(router, node, now_unix)?);
        Ok(output)
    }

    /// Request a fresh announce for propagation metadata whose corresponding
    /// Core identity or cached announce is missing, then immediately forget the
    /// orphan. Python's `Identity.known_destinations` stores the key and
    /// app-data atomically, so it cannot retain this inconsistent state.
    /// Removing it after emitting the request restores that invariant and makes
    /// recovery one-shot; a later verified propagation announce recreates the
    /// entry through `PropagationTransport::remember_announce`.
    fn refresh_orphaned_nodes<R, C, S>(&mut self, node: &mut NodeCore<R, C, S>) -> TickOutput
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let orphaned = self
            .transport
            .known_nodes()
            .filter(|known| {
                node.storage()
                    .get_identity(known.destination.as_bytes())
                    .is_none()
                    || node
                        .storage()
                        .get_announce_cache(known.destination.as_bytes())
                        .is_none()
            })
            .map(|known| known.destination)
            .collect::<Vec<_>>();
        let mut output = TickOutput::default();
        for destination in orphaned {
            output.merge(node.request_path(&destination));
            self.transport.forget_known_node(&destination);
        }
        output
    }

    fn tick_outbound<R, C, S>(
        &mut self,
        router: &mut LxmfRouter,
        node: &mut NodeCore<R, C, S>,
        now_unix: f64,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let mut output = RouterOutput::default();
        let now_ms = node.now_ms();
        let Some(mut propagation_node) = self.outbound_node else {
            return Ok(output);
        };

        // A selected preferred node can disappear after the message was
        // queued. Once Core no longer has a route, move pending work to the
        // best reachable announced alternative instead of exhausting every
        // attempt against the stale preference.
        if node.hops_to(&propagation_node).is_none() {
            if let Some(alternative) = self
                .transport
                .known_nodes()
                .filter(|known| known.destination != propagation_node && known.announce.enabled)
                .filter_map(|known| {
                    node.hops_to(&known.destination).map(|hops| {
                        (
                            hops,
                            known.announce.peering_cost,
                            known.announce.stamp_cost,
                            known.destination,
                        )
                    })
                })
                .min()
                .map(|(_, _, _, destination)| destination)
            {
                self.outbound_node = Some(alternative);
                self.client = MailboxSync::default();
                propagation_node = alternative;
                for entry in router
                    .outbound
                    .values_mut()
                    .filter(|entry| entry.message().method == DeliveryMethod::Propagated)
                {
                    // A build made against the old node's link must not
                    // commit against the new one: the retarget is a value
                    // change of `propagation` and moves the entry's epoch
                    // whenever an envelope was prepared (S3).
                    entry.retarget_propagation(None, &mut router.build_epochs);
                    entry.set_state(super::MessageState::Outbound, &mut router.build_epochs);
                    entry.next_attempt_ms = now_ms;
                }
                router.persistence_dirty = true;
            }
        }

        let active_link = self.transport.active_link(&propagation_node);
        if active_link
            .and_then(|link_id| self.transport.active_upload(&link_id))
            .is_some()
        {
            return Ok(output);
        }

        let Some(message_id) = router.outbound.iter().find_map(|(message_id, entry)| {
            (entry.message().method == DeliveryMethod::Propagated
                && entry.next_attempt_ms <= now_ms)
                .then_some(*message_id)
        }) else {
            return Ok(output);
        };

        if router
            .outbound
            .get(&message_id)
            .is_some_and(|entry| entry.attempts >= super::MAX_DELIVERY_ATTEMPTS)
        {
            router.remove_outbound(&message_id);
            router.persistence_dirty = true;
            output.events.push(RouterEvent::MessageState {
                message_id,
                state: super::MessageState::Failed,
            });
            return Ok(output);
        }

        let recipient_stamp_cost = router
            .outbound
            .get(&message_id)
            .and_then(|entry| {
                router.outbound_stamp_cost_at(&entry.message().destination_hash, now_unix)
            })
            .filter(|cost| *cost > 0);
        if let Some(target_cost) = recipient_stamp_cost {
            if router
                .outbound
                .get(&message_id)
                .is_some_and(|entry| entry.message().stamp.is_none())
            {
                if let Some(entry) = router.outbound.get_mut(&message_id) {
                    entry.next_attempt_ms = now_ms.saturating_add(super::PROCESSING_INTERVAL_MS);
                }
                router.persistence_dirty = true;
                output
                    .events
                    .push(RouterEvent::StampPending(super::DeliveryStampRequest {
                        message_id,
                        target_cost,
                    }));
                return Ok(output);
            }
        }

        let stamp_request = match self.ensure_prepared_upload(router, node, message_id, now_unix) {
            Ok(request) => request,
            Err(RouterError::Node(_)) => {
                if let Some(entry) = router.outbound.get_mut(&message_id) {
                    entry.next_attempt_ms = now_ms.saturating_add(super::PATH_REQUEST_WAIT_MS);
                    output.core.merge(
                        node.request_path(&DestinationHash::new(entry.message().destination_hash)),
                    );
                }
                router.persistence_dirty = true;
                return Ok(output);
            }
            Err(error) => return Err(error),
        };

        let target_cost = router
            .outbound
            .get(&message_id)
            .and_then(super::OutboundEntry::propagation)
            .and_then(|prepared| prepared.target_cost);
        if target_cost.is_none() {
            // A legacy snapshot can contain a path without the corresponding
            // signed announce/app-data. A path response normally replays the
            // cached announce, but Core will correctly reject that replay while
            // the old route and random blob are still installed. Expire only
            // this incomplete route before requesting it again so the response
            // can be validated as the replacement path and repopulate the
            // propagation metadata. Never weaken global replay protection.
            if self.transport.known_node(&propagation_node).is_none()
                && node.hops_to(&propagation_node).is_some()
            {
                node.remove_path(propagation_node.as_bytes());
            }
            let mut exhausted = false;
            if let Some(entry) = router.outbound.get_mut(&message_id) {
                // Python charges a propagated delivery attempt before it
                // checks for a route, then requests the selected node when
                // its cached announce and target cost are unavailable.
                entry.attempts = entry.attempts.saturating_add(1);
                exhausted = entry.attempts >= super::MAX_DELIVERY_ATTEMPTS;
                if !exhausted {
                    entry.next_attempt_ms = now_ms.saturating_add(super::PATH_REQUEST_WAIT_MS);
                }
            }
            router.persistence_dirty = true;
            if exhausted {
                router.remove_outbound(&message_id);
                output.events.push(RouterEvent::MessageState {
                    message_id,
                    state: super::MessageState::Failed,
                });
                return Ok(output);
            }
            output.core.merge(node.request_path(&propagation_node));
            return Ok(output);
        }

        if let Some(request) = stamp_request {
            if let Some(entry) = router.outbound.get_mut(&message_id) {
                entry.next_attempt_ms = now_ms.saturating_add(super::PROCESSING_INTERVAL_MS);
            }
            output
                .events
                .push(RouterEvent::PropagationStampPending(request));
            return Ok(output);
        }

        let Some(link_id) = active_link else {
            let mut exhausted = false;
            if let Some(entry) = router.outbound.get_mut(&message_id) {
                entry.attempts = entry.attempts.saturating_add(1);
                exhausted = entry.attempts >= super::MAX_DELIVERY_ATTEMPTS;
                if !exhausted {
                    entry.next_attempt_ms = now_ms.saturating_add(super::PATH_REQUEST_WAIT_MS);
                }
            }
            router.persistence_dirty = true;
            if exhausted {
                router.remove_outbound(&message_id);
                output.events.push(RouterEvent::MessageState {
                    message_id,
                    state: super::MessageState::Failed,
                });
                return Ok(output);
            }
            let transport = self.transport.ensure_link(node, propagation_node)?;
            output.core.merge(transport.core);
            return Ok(output);
        };

        let prepared = router
            .outbound
            .get(&message_id)
            .and_then(super::OutboundEntry::propagation)
            .ok_or(RouterError::PropagationStampUnavailable)?;
        let stamp = prepared
            .stamp
            .ok_or(RouterError::PropagationStampUnavailable)?;
        let upload =
            PropagationUpload::single(prepared.timebase, prepared.unstamped_lxmf.clone(), stamp)
                .encode();
        // Deferred like a direct Resource: capture the link, hand the build out,
        // and let the caller return it to `LxmfRouter::commit_resource_build`.
        if router.config.defer_resource_builds
            && upload_representation(upload.len()) == PropagationUploadRepresentation::Resource
        {
            // One outstanding build per message: the entry is pushed out to the
            // retry wait either way, so a build already handed out is not
            // re-encoded and re-queued behind itself.
            if router.has_outstanding_build(&message_id) {
                schedule_upload_retry(
                    router,
                    message_id,
                    now_ms.saturating_add(super::DELIVERY_RETRY_WAIT_MS),
                );
                return Ok(output);
            }
            match self
                .transport
                .upload_send_params(node, link_id, message_id, upload.len())
            {
                Ok(params) => {
                    router.push_upload_build(message_id, params, upload);
                    output
                        .events
                        .push(RouterEvent::ResourceBuildPending(message_id));
                    // Handing the build out is not a submission. Without this
                    // the entry stays due and every tick re-encodes the whole
                    // upload to hand out another copy of it.
                    schedule_upload_retry(
                        router,
                        message_id,
                        now_ms.saturating_add(super::DELIVERY_RETRY_WAIT_MS),
                    );
                    return Ok(output);
                }
                Err(PropagationTransportError::UploadInProgress)
                | Err(PropagationTransportError::Resource(ResourceError::TransferInProgress)) => {
                    schedule_upload_retry(
                        router,
                        message_id,
                        now_ms.saturating_add(super::PROCESSING_INTERVAL_MS),
                    );
                    return Ok(output);
                }
                Err(error) => return Err(error.into()),
            }
        }

        let mut submitted = match self
            .transport
            .submit_upload(node, link_id, message_id, &upload)
        {
            Ok(submitted) => submitted,
            Err(PropagationTransportError::UploadInProgress)
            | Err(PropagationTransportError::Resource(ResourceError::TransferInProgress))
            | Err(PropagationTransportError::Send(SendError::Busy)) => {
                schedule_upload_retry(
                    router,
                    message_id,
                    now_ms.saturating_add(super::PROCESSING_INTERVAL_MS),
                );
                return Ok(output);
            }
            Err(PropagationTransportError::Send(SendError::PacingDelay { ready_at_ms })) => {
                schedule_upload_retry(router, message_id, ready_at_ms);
                return Ok(output);
            }
            Err(PropagationTransportError::LinkUnavailable)
            | Err(PropagationTransportError::Resource(_))
            | Err(PropagationTransportError::Send(_)) => {
                output.core.merge(self.transport.close(node, link_id));
                schedule_upload_retry(
                    router,
                    message_id,
                    now_ms.saturating_add(super::DELIVERY_RETRY_WAIT_MS),
                );
                return Ok(output);
            }
            Err(error) => return Err(error.into()),
        };
        output.core.merge(submitted.core);
        for event in submitted.events.drain(..) {
            self.handle_transport_event(router, node, event, now_unix, &mut output)?;
        }
        Ok(output)
    }

    /// Install an upload built off the caller's lock and run the events it
    /// produces through the same path a direct submission takes.
    pub(super) fn commit_upload<R, C, S>(
        &mut self,
        router: &mut LxmfRouter,
        node: &mut NodeCore<R, C, S>,
        prepared: crate::propagation_client::PreparedUpload,
        now_unix: f64,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let mut output = RouterOutput::default();
        let mut submitted = self.transport.commit_upload(node, prepared)?;
        output.core.merge(submitted.core);
        for event in submitted.events.drain(..) {
            self.handle_transport_event(router, node, event, now_unix, &mut output)?;
        }
        Ok(output)
    }

    pub(super) fn next_deadline(&self) -> Option<u64> {
        if self.client.state == PropagationClientState::PathRequested {
            self.client.path_deadline_ms
        } else {
            None
        }
    }
}

fn schedule_upload_retry(router: &mut LxmfRouter, message_id: [u8; 32], next_attempt_ms: u64) {
    if let Some(entry) = router.outbound.get_mut(&message_id) {
        // Usually a no-op value-wise (the entry already is `Outbound`), and a
        // no-op must not move the epoch: this runs every retry interval while
        // a build is outstanding, and a bump per interval would refuse every
        // build a consumer holds longer than one interval.
        entry.set_state(super::MessageState::Outbound, &mut router.build_epochs);
        entry.next_attempt_ms = next_attempt_ms;
        entry.progress = 0.01;
        router.persistence_dirty = true;
    }
}

impl LxmfRouter {
    /// Select the configured propagation node even before a path is known, or
    /// otherwise choose the best enabled node announced to this router. Known
    /// routes rank ahead of nodes that require a path request.
    pub fn select_outbound_propagation_node<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        preferred: Option<DestinationHash>,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let selected = {
            let propagation = self.propagation.as_ref().ok_or(RouterError::NotFound)?;
            // Python accepts a configured propagation hash before a route is
            // known and lets outbound processing request the missing path. An
            // unknown preferred node must therefore remain selectable. Only
            // a node explicitly heard as disabled is rejected here.
            let preferred = preferred.filter(|destination| {
                propagation
                    .transport
                    .known_node(destination)
                    .is_none_or(|known| known.announce.enabled)
            });
            preferred.or_else(|| {
                propagation
                    .transport
                    .known_nodes()
                    .filter(|known| known.announce.enabled)
                    .map(|known| {
                        (
                            node.hops_to(&known.destination).is_none(),
                            node.hops_to(&known.destination).unwrap_or(u8::MAX),
                            known.announce.peering_cost,
                            known.announce.stamp_cost,
                            known.destination,
                        )
                    })
                    .min()
                    .map(|(_, _, _, _, destination)| destination)
            })
        }
        .ok_or(RouterError::NotFound)?;
        self.set_outbound_propagation_node(node, Some(selected))
    }

    pub fn set_outbound_propagation_node<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        destination: Option<DestinationHash>,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let mut output = RouterOutput::default();
        if let Some(destination) = destination.as_ref() {
            let now_ms = node.now_ms();
            node.storage_mut()
                .used_known_dest(destination.as_bytes(), now_ms);
        }
        let changed = {
            let propagation = self.propagation.as_mut().ok_or(RouterError::NotFound)?;
            let changed = propagation.outbound_node != destination;
            if changed {
                propagation.cancel_outbound_link(node, &mut output);
                propagation.outbound_node = destination;
                propagation.client = MailboxSync::default();
            }
            changed
        };
        if changed {
            let now_ms = node.now_ms();
            let mut queue_changed = false;
            for entry in self
                .outbound
                .values_mut()
                .filter(|entry| entry.message().method == DeliveryMethod::Propagated)
            {
                queue_changed |= entry.retarget_propagation(None, &mut self.build_epochs);
                queue_changed |= entry
                    .set_state(super::MessageState::Outbound, &mut self.build_epochs)
                    || entry.next_attempt_ms != now_ms;
                entry.next_attempt_ms = now_ms;
            }
            self.persistence_dirty |= queue_changed;
            output.events.push(RouterEvent::PropagationSyncState(
                PropagationSyncStatus::idle(),
            ));
        }
        Ok(self.finish_output(output))
    }

    pub fn outbound_propagation_node(&self) -> Option<DestinationHash> {
        self.propagation
            .as_ref()
            .and_then(PropagationRuntime::outbound_node)
    }

    pub fn propagation_client_state(&self) -> Option<PropagationClientState> {
        self.propagation
            .as_ref()
            .map(|runtime| runtime.client.state)
    }

    pub fn propagation_client_progress(&self) -> Option<f32> {
        self.propagation
            .as_ref()
            .map(|runtime| runtime.client.progress)
    }

    pub fn propagation_client_transfer_size(&self) -> Option<u64> {
        self.propagation
            .as_ref()
            .and_then(|runtime| runtime.client.transfer_size)
    }

    pub fn propagation_client_last_result(&self) -> Option<PropagationSyncResult> {
        self.propagation.as_ref().and_then(|runtime| {
            (runtime.client.state == PropagationClientState::Complete).then_some(
                PropagationSyncResult {
                    received: runtime.client.received,
                    duplicates: runtime.client.duplicates,
                },
            )
        })
    }

    pub fn known_propagation_node(
        &self,
        destination: &DestinationHash,
    ) -> Option<&KnownPropagationNode> {
        self.propagation
            .as_ref()
            .and_then(|runtime| runtime.transport.known_node(destination))
    }

    pub fn known_propagation_nodes(&self) -> impl Iterator<Item = &KnownPropagationNode> {
        self.propagation
            .iter()
            .flat_map(|runtime| runtime.transport.known_nodes())
    }

    pub fn restore_propagation_announce(&mut self, announce: &ReceivedAnnounce) -> bool {
        self.propagation
            .as_mut()
            .and_then(|runtime| runtime.transport.remember_announce(announce))
            .is_some()
    }

    pub fn restore_known_propagation_node(&mut self, known: KnownPropagationNode) -> bool {
        let Some(runtime) = self.propagation.as_mut() else {
            return false;
        };
        runtime.transport.restore_known_node(known);
        true
    }

    pub fn retain_known_propagation_nodes(&mut self, destinations: &BTreeSet<DestinationHash>) {
        if let Some(runtime) = self.propagation.as_mut() {
            runtime.transport.retain_known_nodes(destinations);
        }
    }

    pub fn request_messages_from_propagation_node<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        max_messages: Option<usize>,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let propagation = self.propagation.as_mut().ok_or(RouterError::NotFound)?;
        let destination = propagation.outbound_node.ok_or(RouterError::NotFound)?;
        let now_ms = node.now_ms();
        node.storage_mut()
            .used_known_dest(destination.as_bytes(), now_ms);
        propagation.client = MailboxSync {
            // Python uses zero as PR_ALL_MESSAGES.
            max_messages: max_messages.filter(|maximum| *maximum != 0),
            ..MailboxSync::default()
        };
        let mut output = RouterOutput::default();

        if let Some(link_id) = propagation.transport.active_link(&destination) {
            propagation.begin_list_request(node, link_id, &mut output)?;
        } else if node.has_path(&destination) {
            propagation.set_state(PropagationClientState::LinkEstablishing, &mut output);
            output
                .core
                .merge(propagation.transport.ensure_link(node, destination)?.core);
        } else {
            propagation.client.path_deadline_ms = Some(
                node.now_ms()
                    .saturating_add(propagation.config.path_timeout_ms),
            );
            propagation.set_state(PropagationClientState::PathRequested, &mut output);
            output
                .core
                .merge(propagation.transport.ensure_link(node, destination)?.core);
            output.core.next_deadline_ms = propagation.client.path_deadline_ms;
        }
        Ok(self.finish_output(output))
    }

    pub fn cancel_propagation_node_requests<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let propagation = self.propagation.as_mut().ok_or(RouterError::NotFound)?;
        let mut output = RouterOutput::default();
        propagation.cancel_outbound_link(node, &mut output);
        propagation.client = MailboxSync::default();
        output.events.push(RouterEvent::PropagationSyncState(
            PropagationSyncStatus::idle(),
        ));
        Ok(self.finish_output(output))
    }
}

fn sync_is_active(state: PropagationClientState) -> bool {
    matches!(
        state,
        PropagationClientState::PathRequested
            | PropagationClientState::LinkEstablishing
            | PropagationClientState::LinkEstablished
            | PropagationClientState::RequestSent
            | PropagationClientState::Receiving
            | PropagationClientState::ResponseReceived
    )
}

/// Returns `true` when the encrypted transient ID was already processed.
fn deliver_unstamped<R, C, S>(
    router: &mut LxmfRouter,
    node: &NodeCore<R, C, S>,
    bytes: &[u8],
    now_unix: f64,
    output: &mut RouterOutput,
) -> Result<bool, RouterError>
where
    R: CryptoRngCore,
    C: Clock,
    S: Storage,
{
    let transient_id: TransientId = full_hash(bytes);
    if router.processed_ids.contains_key(&transient_id) {
        output.events.push(RouterEvent::Duplicate(transient_id));
        return Ok(true);
    }
    let propagated = PropagatedMessage::from_unstamped_bytes(bytes)?;
    if propagated.destination_hash() != router.node.delivery_destination_hash().as_bytes() {
        router.insert_bounded_id(transient_id, now_unix, false);
        return Ok(false);
    }
    let destination = node
        .destination(&router.node.delivery_destination_hash())
        .ok_or(RouterError::NotFound)?;
    let packed = propagated.decrypt(destination)?;
    let message = unpack_local(node, &packed, DeliveryMethod::Propagated)?;
    // As with paper messages, do not leave behind an unreported checkpoint
    // mutation if destination decryption or clear-message parsing fails.
    router.insert_bounded_id(transient_id, now_unix, false);
    // Python records the encrypted transient ID as locally delivered after
    // successful destination decryption, separately from the clear message ID.
    router.insert_bounded_id(transient_id, now_unix, true);
    router.handle_inbound_message(message, now_unix, &mut output.events);
    Ok(false)
}

#[cfg(test)]
#[path = "propagation_runtime_tests.rs"]
mod tests;
