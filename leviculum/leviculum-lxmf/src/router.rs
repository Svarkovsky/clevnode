//! Long-lived, sans-I/O LXMF router.

#[cfg(feature = "pow")]
use alloc::collections::VecDeque;
use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    vec,
    vec::Vec,
};
use leviculum_core::{
    crypto::full_hash, resource::ResourceError, Clock, Destination, DestinationHash, LinkId,
    NodeCore, NodeEvent, Storage, TickOutput,
};
use rand_core::CryptoRngCore;

use crate::{
    announce::DeliveryAnnounce,
    constants::{FIELD_TICKET, LXMF_OVERHEAD, STAMP_COST_EXPIRY, STAMP_SIZE},
    message::{DeliveryMethod, Field, Message, MessageError, Verification},
    msgpack,
    node::{
        DeliveryFailure, DeliveryRepresentation, DirectLinkState, InboundRejection, LxmfNode,
        LxmfNodeError, LxmfNodeEvent, LxmfResourceSendParams, PreparedLxmfSend, APP_NAME,
        DELIVERY_ASPECT,
    },
    propagation::PropagationError,
    propagation_client::{
        PreparedUpload, PropagationTransport, PropagationTransportError, UploadSendParams,
    },
    storage::{LxmfStorage, StorageError},
    ticket::{Ticket, TicketStore},
};

mod outbound;
mod paper_runtime;
mod propagation_runtime;
#[cfg(feature = "pow")]
mod stamp_runtime;
use outbound::BuildEpochs;
pub use outbound::OutboundEntry;
use propagation_runtime::PropagationRuntime;
pub use propagation_runtime::{
    PropagationClientConfig, PropagationClientState, PropagationSyncResult, PropagationSyncStatus,
};

pub const MAX_DELIVERY_ATTEMPTS: u8 = 5;
pub const PROCESSING_INTERVAL_MS: u64 = 4_000;
pub const DELIVERY_RETRY_WAIT_MS: u64 = 10_000;
pub const PATH_REQUEST_WAIT_MS: u64 = 7_000;
pub const MAX_PATHLESS_TRIES: u8 = 1;
pub const MESSAGE_EXPIRY_SECS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

const ROUTER_STATE_KEY: &[u8] = b"lxmf/router-state";
const SNAPSHOT_VERSION: u64 = 4;
const SNAPSHOT_FIELDS: usize = 9;

type StampCostEntry = (f64, Option<u8>, bool);
type StampCostMap = BTreeMap<[u8; 16], StampCostEntry>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageState {
    Generating = 0x00,
    Outbound = 0x01,
    Sending = 0x02,
    Sent = 0x04,
    Delivered = 0x08,
    /// A propagation node accepted the message and holds it for the
    /// recipient, who has not collected it yet.
    ///
    /// Terminal here: nothing on the wire tells an origin that a mailbox was
    /// emptied, so the router never leaves this state on its own. It exists
    /// because `Sent` cannot carry the difference — a direct message that
    /// reached `Sent` was handed to the recipient's own destination, and a
    /// client that shows both the same way tells the user a message arrived
    /// when it is sitting in someone else's mailbox.
    ///
    /// Local bookkeeping only. Python has no such state (LXMessage.py:15-23)
    /// and derives the same distinction from the delivery method; the value
    /// is deliberately outside the range Python assigns, is never encoded
    /// into a wire field, and reaches no peer.
    AwaitingCollection = 0x10,
    Rejected = 0xfd,
    Cancelled = 0xfe,
    Failed = 0xff,
}

#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub max_outbound: usize,
    pub max_delivered_ids: usize,
    pub max_processed_ids: usize,
    pub max_stamp_costs: usize,
    pub max_tickets: usize,
    pub max_policy_entries: usize,
    pub max_snapshot_bytes: usize,
    pub enforce_stamps: bool,
    pub inbound_stamp_cost: Option<u8>,
    /// Hand outbound Resource builds to the caller instead of building them
    /// inside [`LxmfRouter::tick`].
    ///
    /// A tick that builds its own Resources holds the caller's borrow for the
    /// whole build — 126.6 ms for eight due 256 KiB messages, measured in
    /// `docs/src/concepts/core-lock-budget.md`. With this set, `tick` captures
    /// the link parameters instead and emits
    /// [`RouterEvent::ResourceBuildPending`]; the caller drains
    /// [`LxmfRouter::take_resource_builds`], builds off its lock, and returns
    /// each result to [`LxmfRouter::commit_resource_build`].
    ///
    /// Off by default: a caller that ignores the drained work would leave those
    /// messages queued forever, so the split is opt-in for hosts that implement
    /// both halves.
    pub defer_resource_builds: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            max_outbound: 256,
            max_delivered_ids: 4_096,
            max_processed_ids: 4_096,
            max_stamp_costs: 4_096,
            max_tickets: 4_096,
            max_policy_entries: 4_096,
            max_snapshot_bytes: 32 * 1024 * 1024,
            enforce_stamps: false,
            inbound_stamp_cost: None,
            defer_resource_builds: false,
        }
    }
}

/// Detached work coordinates for one outbound Resource, mirroring the stamp
/// requests above: it owns everything the build needs, so building never
/// borrows [`LxmfRouter`] or `NodeCore`.
///
/// No `#[must_use]`: the attribute is not inherited through the `Vec`
/// [`LxmfRouter::take_resource_builds`] hands back, so it never fired. A
/// dropped build is not silent either way — its message stays queued and the
/// next tick hands it out again.
pub struct PendingResourceBuild {
    message_id: [u8; 32],
    /// The router's restore generation at capture time. A build made against a
    /// queue that has since been replaced is refused at commit (C9).
    restore_generation: u64,
    /// The entry's build epoch at capture time. A build whose entry has since
    /// changed `message`, `state` or `propagation` is refused at commit (C4).
    build_epoch: u64,
    kind: PendingBuildKind,
}

enum PendingBuildKind {
    /// A direct delivery to the recipient.
    Delivery {
        params: LxmfResourceSendParams,
        message: Box<Message>,
        auto_compress: bool,
    },
    /// An upload of one message to a propagation node's mailbox.
    Upload {
        params: UploadSendParams,
        data: Vec<u8>,
    },
}

impl PendingResourceBuild {
    pub fn message_id(&self) -> [u8; 32] {
        self.message_id
    }

    /// Build the transfer. This is the part that scales with the message, and
    /// the reason the router handed it out.
    pub fn build(self, rng: &mut impl CryptoRngCore) -> Result<BuiltResource, RouterError> {
        let message_id = self.message_id;
        let restore_generation = self.restore_generation;
        let build_epoch = self.build_epoch;
        let kind = match self.kind {
            PendingBuildKind::Delivery {
                params,
                message,
                auto_compress,
            } => BuiltKind::Delivery(Box::new(LxmfNode::prepare_resource_send(
                &params,
                &message,
                auto_compress,
                rng,
            )?)),
            PendingBuildKind::Upload { params, data } => {
                BuiltKind::Upload(params.build(&data, rng)?)
            }
        };
        Ok(BuiltResource {
            message_id,
            restore_generation,
            build_epoch,
            kind,
        })
    }
}

/// A built transfer on its way back to
/// [`LxmfRouter::commit_resource_build`].
#[must_use = "a dropped build leaves its message queued in the router"]
pub struct BuiltResource {
    message_id: [u8; 32],
    restore_generation: u64,
    build_epoch: u64,
    kind: BuiltKind,
}

impl BuiltResource {
    pub fn message_id(&self) -> [u8; 32] {
        self.message_id
    }
}

enum BuiltKind {
    Delivery(Box<PreparedLxmfSend>),
    Upload(PreparedUpload),
}

fn record_successful_submission_attempt(
    entry: &mut OutboundEntry,
    representation: &Result<DeliveryRepresentation, LxmfNodeError>,
) {
    // Python charges direct delivery when it starts path discovery or a new
    // Link. Submitting a Packet or Resource over that Link remains part of the
    // same attempt, and reusing an already-active Link does not consume one.
    // Opportunistic delivery has no Link setup phase, so submission itself is
    // the attempt boundary.
    if matches!(
        representation,
        Ok(DeliveryRepresentation::OpportunisticPacket)
    ) {
        entry.attempts = entry.attempts.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboundPropagation {
    /// Timebase encoded into the upload envelope and reused for retries.
    pub timebase: f64,
    /// `destination_hash || destination.encrypt(packed[16..])`, without the
    /// outer propagation stamp.
    pub unstamped_lxmf: Vec<u8>,
    /// SHA-256 of `unstamped_lxmf`.
    pub transient_id: [u8; 32],
    /// Full cost advertised by the selected propagation node.
    pub target_cost: Option<u8>,
    /// The 32-byte propagation-node PoW stamp. Tickets never replace this.
    pub stamp: Option<[u8; STAMP_SIZE]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropagationStampRequest {
    pub message_id: [u8; 32],
    pub transient_id: [u8; 32],
    pub target_cost: u8,
}

/// Detached work coordinates for a recipient delivery stamp.
///
/// The request owns everything a stamp worker needs, so calculating the stamp
/// never has to borrow [`LxmfRouter`] or `NodeCore`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryStampRequest {
    pub message_id: [u8; 32],
    pub target_cost: u8,
}

/// Detached work coordinates for validating one inbound delivery stamp.
///
/// With the `pow` feature, the corresponding message remains queued in the
/// router until `LxmfRouter::set_inbound_stamp_result()` applies the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundStampRequest {
    pub message_id: [u8; 32],
    pub stamp: [u8; STAMP_SIZE],
    pub target_cost: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RouterEvent {
    MessageQueued([u8; 32]),
    MessageState {
        message_id: [u8; 32],
        state: MessageState,
    },
    MessageReceived(Box<Message>),
    InboundRejected {
        method: DeliveryMethod,
        reason: InboundRejection,
    },
    DirectLinkEstablished {
        destination: Option<DestinationHash>,
        link_id: LinkId,
        is_initiator: bool,
    },
    Duplicate([u8; 32]),
    /// A peer announced its `lxmf.delivery` destination, with the announce the
    /// router decoded for its own outbound stamp-cost cache.
    ///
    /// The router already filters announces by the delivery name hash and runs
    /// [`DeliveryAnnounce::decode`] on the app data. Emitting the result is
    /// what keeps every client from re-implementing both, and it is the only
    /// path on which a peer's display name reaches an application.
    PeerAnnounced {
        destination: DestinationHash,
        announce: DeliveryAnnounce,
    },
    InvalidSignature([u8; 32]),
    InvalidStamp([u8; 32]),
    /// A Resource build is waiting in [`LxmfRouter::take_resource_builds`].
    /// Only emitted with [`RouterConfig::defer_resource_builds`].
    ResourceBuildPending([u8; 32]),
    StampPending(DeliveryStampRequest),
    InboundStampPending(InboundStampRequest),
    PropagationStampPending(PropagationStampRequest),
    PropagationSyncState(PropagationSyncStatus),
    PropagationSyncComplete(PropagationSyncResult),
    PersistenceRequested,
}

#[derive(Debug, Default)]
#[must_use]
pub struct RouterOutput {
    pub core: TickOutput,
    pub events: Vec<RouterEvent>,
}

impl RouterOutput {
    fn merge(&mut self, mut other: Self) {
        self.core.merge(other.core);
        self.events.append(&mut other.events);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RouterError {
    QueueFull,
    Duplicate,
    NotFound,
    IdentityMismatch,
    UnsupportedMethod,
    PropagationNodeUnavailable,
    PropagationStampUnavailable,
    StaleStampRequest,
    /// A returned Resource build no longer matches the queue entry it was
    /// captured from, so installing it would put superseded bytes on the wire.
    ///
    /// Distinct from the errors those paths already produce
    /// (`Node(Resource(TransferInProgress))`, `PropagationNodeUnavailable`,
    /// `PropagationStampUnavailable`), so a test can assert staleness itself
    /// rather than any refusal. Returned by
    /// [`LxmfRouter::commit_resource_build`] when the build fails any of its
    /// three checks: restore generation, queue membership, build epoch
    /// (Codeberg #196, C3/C4/C9).
    StaleBuild,
    /// The node's timebase is below the plausibility floor, so it has no wall
    /// clock and has learned none: it can only produce uptime seconds.
    ///
    /// Returned instead of writing a wire field a peer evaluates against its
    /// own clock and silently discards (Codeberg #182). See
    /// [`LxmfRouter::issue_ticket_field`].
    NoWallClock,
    Node(LxmfNodeError),
    Message(MessageError),
    Propagation(PropagationError),
    PropagationTransport(PropagationTransportError),
    Paper(crate::paper::PaperError),
    #[cfg(feature = "pow")]
    Stamp(crate::stamp::StampError),
    Storage(StorageError),
    CorruptSnapshot,
}
impl core::fmt::Display for RouterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::QueueFull => write!(f, "outbound queue is full"),
            Self::Duplicate => write!(f, "message is already queued"),
            Self::NotFound => write!(f, "no such message"),
            Self::IdentityMismatch => write!(f, "state belongs to another identity"),
            Self::UnsupportedMethod => write!(f, "delivery method is not supported here"),
            Self::PropagationNodeUnavailable => write!(f, "no propagation node selected"),
            Self::PropagationStampUnavailable => write!(f, "propagation stamp is missing"),
            Self::StaleStampRequest => write!(f, "stamp result no longer matches the message"),
            Self::StaleBuild => write!(f, "resource build no longer matches the queue entry"),
            Self::NoWallClock => write!(f, "node has no wall clock"),
            Self::Node(error) => write!(f, "node: {error}"),
            Self::Message(error) => write!(f, "message: {error}"),
            Self::Propagation(error) => write!(f, "propagation: {error}"),
            Self::PropagationTransport(error) => write!(f, "propagation transport: {error}"),
            Self::Paper(error) => write!(f, "paper: {error}"),
            #[cfg(feature = "pow")]
            Self::Stamp(error) => write!(f, "stamp: {error}"),
            Self::Storage(error) => write!(f, "storage: {error}"),
            Self::CorruptSnapshot => write!(f, "stored router state is corrupt"),
        }
    }
}

impl core::error::Error for RouterError {}

impl From<LxmfNodeError> for RouterError {
    fn from(v: LxmfNodeError) -> Self {
        Self::Node(v)
    }
}
impl From<MessageError> for RouterError {
    fn from(v: MessageError) -> Self {
        Self::Message(v)
    }
}
impl From<PropagationError> for RouterError {
    fn from(v: PropagationError) -> Self {
        Self::Propagation(v)
    }
}
impl From<PropagationTransportError> for RouterError {
    fn from(v: PropagationTransportError) -> Self {
        Self::PropagationTransport(v)
    }
}
impl From<crate::paper::PaperError> for RouterError {
    fn from(v: crate::paper::PaperError) -> Self {
        Self::Paper(v)
    }
}
#[cfg(feature = "pow")]
impl From<crate::stamp::StampError> for RouterError {
    fn from(v: crate::stamp::StampError) -> Self {
        Self::Stamp(v)
    }
}
impl From<StorageError> for RouterError {
    fn from(v: StorageError) -> Self {
        Self::Storage(v)
    }
}

pub struct LxmfRouter {
    node: LxmfNode,
    config: RouterConfig,
    identity_hash: [u8; 16],
    outbound: BTreeMap<[u8; 32], OutboundEntry>,
    pending_builds: Vec<PendingResourceBuild>,
    /// The message IDs whose Resource build the router is still holding.
    ///
    /// This is what bounds `pending_builds` once the deferral stops marking
    /// the entry `Sending`: without it a due message would append a full
    /// `Message` clone every retry interval. A message listed here is not
    /// captured again; its `next_attempt_ms` is pushed as usual.
    builds_outstanding: BTreeSet<[u8; 32]>,
    /// Bumped by every [`restore`](Self::restore). A build carries the value it
    /// was captured at, so one made against a queue that has since been
    /// replaced is refused even though the restored queue holds its ID.
    /// Deliberately not persisted: a restored counter would validate exactly
    /// the builds it exists to refuse.
    restore_generation: u64,
    /// The one source every [`OutboundEntry`] build epoch is minted from.
    /// Router-level and monotonic, so an epoch never repeats across entry
    /// lifetimes — a per-entry counter would restart at cancel-plus-re-enqueue
    /// of identical content and validate a pre-cancel build. Not persisted for
    /// the same reason as the restore generation.
    build_epochs: BuildEpochs,
    outbound_stamp_costs: StampCostMap,
    delivered_ids: BTreeMap<[u8; 32], f64>,
    processed_ids: BTreeMap<[u8; 32], f64>,
    tickets: TicketStore,
    ignored: BTreeSet<[u8; 16]>,
    /// Runtime-only markers for Python LXMF's pre-emptive first path request
    /// for opportunistic messages. They deliberately reset after restoration;
    /// requesting a fresh path after a restart is safe and desirable.
    preemptive_path_requests: BTreeSet<[u8; 32]>,
    next_job_ms: u64,
    /// Tracks snapshot mutations until the next returned router output can ask
    /// the integration to atomically replace its checkpoint.
    persistence_dirty: bool,
    /// Volatile verification work. Durable messages are committed only after
    /// verification and therefore do not need an in-progress snapshot form.
    #[cfg(feature = "pow")]
    pending_inbound_stamps: VecDeque<(Message, f64)>,
    propagation: Option<PropagationRuntime>,
}

struct RestoredRouterState {
    outbound: BTreeMap<[u8; 32], OutboundEntry>,
    outbound_stamp_costs: StampCostMap,
    delivered_ids: BTreeMap<[u8; 32], f64>,
    processed_ids: BTreeMap<[u8; 32], f64>,
    tickets: TicketStore,
    ignored: BTreeSet<[u8; 16]>,
}

/// The unix-seconds value every LXMF wire field with cross-lifetime semantics
/// is written from (Codeberg #182).
///
/// LXMF has three such fields — the message timestamp, the ticket expiry and
/// the propagation upload timestamp — and `docs/src/concepts/time-and-clocks.md`
/// ("One value, one producer") binds them to `Transport::emission_secs`, which
/// [`NodeCore::emission_secs`] exposes. Every public router entry point that
/// needs wall time resolves it here from the `NodeCore` it is already handed,
/// so no caller can supply a monotonic or invented value; the parameter this
/// replaces was the #155 failure mode one crate up.
///
/// The value carries sub-second precision, as the reference's `time.time()`
/// does (Codeberg #217). It has to: [`Message::create`] hashes the timestamp
/// into the message ID, so at whole-second granularity two identical messages
/// created inside one second are one ID and the second is refused as a
/// [`RouterError::Duplicate`] — a message a Python node would have sent.
/// [`NodeCore::emission_secs_f64`] is the same producer and the same
/// source-priority chain as [`NodeCore::emission_secs`], one decimal point
/// further right.
///
/// One consequence worth knowing: the router's own caches (`clean`, the
/// stamp-cost and delivered/processed ID windows) are aged on this value, so a
/// clockless node whose timebase jumps from uptime seconds to real unix time
/// expires them all in one pass, exactly as a Python node does across a large
/// NTP step. The effect is a lost dedup window, not a wire-visible one.
fn emission_secs<R, C, S>(node: &NodeCore<R, C, S>) -> f64
where
    R: CryptoRngCore,
    C: Clock,
    S: Storage,
{
    node.emission_secs_f64()
}

fn unpack_local<R, C, S>(
    node: &NodeCore<R, C, S>,
    packed: &[u8],
    method: DeliveryMethod,
) -> Result<Message, RouterError>
where
    R: CryptoRngCore,
    C: Clock,
    S: Storage,
{
    let source_hash: [u8; 16] = packed
        .get(16..32)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(MessageError::TooShort)?;
    let source = node.storage().get_identity(&source_hash);
    Ok(Message::unpack(packed, None, source, method)?)
}

impl LxmfRouter {
    pub fn new(node: LxmfNode, identity_hash: [u8; 16], config: RouterConfig) -> Self {
        Self {
            node,
            config,
            identity_hash,
            outbound: BTreeMap::new(),
            pending_builds: Vec::new(),
            builds_outstanding: BTreeSet::new(),
            restore_generation: 0,
            build_epochs: BuildEpochs::default(),
            outbound_stamp_costs: BTreeMap::new(),
            delivered_ids: BTreeMap::new(),
            processed_ids: BTreeMap::new(),
            tickets: TicketStore::default(),
            ignored: BTreeSet::new(),
            preemptive_path_requests: BTreeSet::new(),
            next_job_ms: 0,
            persistence_dirty: false,
            #[cfg(feature = "pow")]
            pending_inbound_stamps: VecDeque::new(),
            propagation: None,
        }
    }

    /// Attach the propagation-mailbox client transport.
    ///
    /// Downloaded transient IDs are covered by the router checkpoint. Link and
    /// in-flight request state remains intentionally volatile.
    pub fn enable_propagation_client(
        &mut self,
        transport: PropagationTransport,
        config: PropagationClientConfig,
    ) -> Result<(), RouterError> {
        if transport.identity_hash() != self.identity_hash {
            return Err(RouterError::IdentityMismatch);
        }
        self.propagation = Some(PropagationRuntime::new(transport, config));
        Ok(())
    }

    pub fn disable_propagation_client(&mut self) -> Option<PropagationTransport> {
        self.propagation
            .take()
            .map(PropagationRuntime::into_transport)
    }

    pub fn node(&self) -> &LxmfNode {
        &self.node
    }
    pub fn node_mut(&mut self) -> &mut LxmfNode {
        &mut self.node
    }
    /// Return whether the delivery adapter owns an established Link.
    pub fn owns_link(&self, link_id: &LinkId) -> bool {
        self.node.owns_link(link_id)
            || self
                .propagation
                .as_ref()
                .is_some_and(|runtime| runtime.owns_link(link_id))
    }
    pub fn tickets(&self) -> &TicketStore {
        &self.tickets
    }
    pub fn tickets_mut(&mut self) -> &mut TicketStore {
        // A mutable borrow can change any of the snapshotted ticket maps. The
        // next RouterOutput will conservatively request a checkpoint.
        self.persistence_dirty = true;
        &mut self.tickets
    }

    /// Issue the signed `FIELD_TICKET` value that grants the destination a
    /// ticket-stamped reply. The returned field must be inserted before
    /// [`Message::create`], since fields are covered by the message ID and
    /// signature.
    ///
    /// The expiry is `emission_secs + TICKET_EXPIRY`, from the one producer
    /// (the module-private `emission_secs` helper). It is the only LXMF field
    /// a peer *discards* on its own clock: `if time.time() < expires`
    /// (`reference/LXMF/LXMF/LXMRouter.py:1854`), silently, with no reply and
    /// no log naming us.
    ///
    /// Because of that silence this call REFUSES with [`RouterError::NoWallClock`]
    /// when the node's timebase is below the plausibility floor
    /// (`NodeCore::has_plausible_wall_clock`, i.e. uptime seconds on a
    /// clockless node that has learned no announce timebase yet). Issuing
    /// anyway would produce a ticket every Python peer drops while two
    /// leviculum nodes accept each other's happily — self-consistent between
    /// our writer and our reader, wrong to a peer, which is Codeberg #155
    /// exactly. A named error is a diagnosis; a discarded ticket is a mystery.
    /// This is a refusal to *issue*, never a filter on what we accept: a
    /// peer's ticket is remembered and used regardless of our own clock.
    pub fn issue_ticket_field<R, C, S, G>(
        &mut self,
        node: &NodeCore<R, C, S>,
        destination: [u8; 16],
        rng: &mut G,
    ) -> Result<(Option<Field>, RouterOutput), RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
        G: CryptoRngCore,
    {
        if !node.has_plausible_wall_clock() {
            return Err(RouterError::NoWallClock);
        }
        let now_unix = emission_secs(node);
        let before = self.tickets.entry_count();
        let mut probe = self.tickets.clone();
        let ticket = probe.issue(destination, now_unix, rng);
        let delivery_record_headroom =
            usize::from(ticket.is_some() && !probe.last_deliveries().contains_key(&destination));
        if probe.entry_count().saturating_add(delivery_record_headroom) > self.config.max_tickets {
            return Err(RouterError::QueueFull);
        }
        self.tickets = probe;
        if self.tickets.entry_count() != before {
            self.persistence_dirty = true;
        }
        let field = ticket.map(|ticket| (FIELD_TICKET, ticket.field_value()));
        let output = self.finish_output(RouterOutput::default());
        Ok((field, output))
    }
    pub fn outbound(&self) -> &BTreeMap<[u8; 32], OutboundEntry> {
        &self.outbound
    }
    /// Returns whether this transient or clear message ID was already delivered.
    ///
    /// This mirrors Python LXMF's `has_message()` cache lookup and is also used
    /// when constructing propagation-node wants and haves lists.
    pub fn has_message(&self, message_id: &[u8; 32]) -> bool {
        self.delivered_ids.contains_key(message_id)
    }
    pub fn ignore(&mut self, source: [u8; 16]) {
        if (self.ignored.contains(&source) || self.ignored.len() < self.config.max_policy_entries)
            && self.ignored.insert(source)
        {
            self.persistence_dirty = true;
        }
    }
    pub fn unignore(&mut self, source: &[u8; 16]) {
        if self.ignored.remove(source) {
            self.persistence_dirty = true;
        }
    }
    /// Replace the complete inbound ignore policy in one bounded operation.
    ///
    /// Browser and native frontends commonly persist their block list outside
    /// LXMF as user-facing application state. Replacing the set lets those
    /// frontends reconcile the router checkpoint exactly after startup or an
    /// identity switch, including destinations that were unblocked while the
    /// router was not running.
    pub fn replace_ignored(
        &mut self,
        ignored: BTreeSet<[u8; 16]>,
    ) -> Result<RouterOutput, RouterError> {
        if ignored.len() > self.config.max_policy_entries {
            return Err(RouterError::QueueFull);
        }
        if self.ignored != ignored {
            self.ignored = ignored;
            self.persistence_dirty = true;
        }
        Ok(self.finish_output(RouterOutput::default()))
    }
    /// Build and sign an outbound message from this router's own delivery
    /// identity, stamping the timestamp from the one producer.
    ///
    /// [`Message::create`] stays available for offline composition (paper
    /// messages, vectors, decoded-and-rebuilt messages) and necessarily takes
    /// the timestamp explicitly — it has no node and no clock. This is the
    /// path for a message we are about to send, and it is the only one that
    /// cannot be handed a monotonic value by mistake (Codeberg #182).
    ///
    /// Unlike [`Self::issue_ticket_field`] this does NOT refuse an implausible
    /// clock. The reference writes `self.timestamp = time.time()` unvalidated
    /// (`reference/LXMF/LXMF/LXMessage.py:357`) and no peer discards a message
    /// on it — it is read back at :797 and displayed. Withholding the message
    /// would be a far worse failure than a mis-sorted one, and matches the
    /// "We do not validate our own clock" rule in
    /// `docs/src/concepts/time-and-clocks.md`.
    pub fn create_message<R, C, S>(
        &self,
        node: &NodeCore<R, C, S>,
        destination_hash: [u8; 16],
        title: Vec<u8>,
        content: Vec<u8>,
        fields: Vec<Field>,
        method: DeliveryMethod,
    ) -> Result<Message, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let source_hash = self.node.delivery_destination_hash();
        let source = node
            .destination(&source_hash)
            .and_then(|destination| destination.identity())
            .ok_or(RouterError::NotFound)?;
        Ok(Message::create(
            destination_hash,
            source_hash.into_bytes(),
            source,
            emission_secs(node),
            title,
            content,
            fields,
            method,
        )?)
    }

    pub fn enqueue<R, C, S>(
        &mut self,
        node: &NodeCore<R, C, S>,
        mut message: Message,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let now_ms = node.now_ms();
        let now_unix = emission_secs(node);
        if message.method == DeliveryMethod::Paper {
            return Err(RouterError::UnsupportedMethod);
        }
        if message.method == DeliveryMethod::Propagated
            && self
                .propagation
                .as_ref()
                .and_then(PropagationRuntime::outbound_node)
                .is_none()
        {
            return Err(RouterError::PropagationNodeUnavailable);
        }
        if self.outbound.contains_key(&message.message_id) {
            return Err(RouterError::Duplicate);
        }
        if self.outbound.len() >= self.config.max_outbound {
            return Err(RouterError::QueueFull);
        }
        if let Some(ticket) = self.tickets.outbound(&message.destination_hash, now_unix) {
            message.set_stamp(
                crate::stamp::ticket_stamp(&ticket.secret, &message.message_id).to_vec(),
            )?;
        }
        // Refuse a delivery no Python peer would accept before any send work
        // happens. The reference has no send-side check and lets the receiver
        // refuse the advertisement (`delivery_resource_advertised`,
        // `reference/LXMF/LXMF/LXMRouter.py:1977`); refusing here spares the
        // pack/compress/encrypt/advertise round trip that ends the same way.
        // Propagated messages are exempt: their wire form is the propagation
        // envelope, which the delivery limit does not measure.
        if message.method != DeliveryMethod::Propagated {
            if let Some(limit) = self.node.config().max_outgoing_delivery_size {
                let size = message.pack().len() as u64;
                if size > limit {
                    return Err(RouterError::Node(LxmfNodeError::DeliveryLimitExceeded {
                        size,
                        limit,
                    }));
                }
            }
        }
        let id = message.message_id;
        self.outbound.insert(
            id,
            OutboundEntry::new(
                message,
                MessageState::Outbound,
                0,
                now_ms,
                0.01,
                None,
                &mut self.build_epochs,
            ),
        );
        self.persistence_dirty = true;
        let mut output = RouterOutput {
            core: TickOutput::default(),
            events: vec![RouterEvent::MessageQueued(id)],
        };
        // The entry this call created is due now. A host driven by the returned
        // `next_deadline_ms` would otherwise sleep past it.
        self.apply_deadline(&mut output.core);
        Ok(self.finish_output(output))
    }

    /// Take an entry out of the outbound queue, dropping any outstanding-build
    /// marker with it.
    ///
    /// A build for an entry that no longer exists is refused at commit; leaving
    /// the marker behind would only block the ID if the same message is queued
    /// again.
    fn remove_outbound(&mut self, id: &[u8; 32]) -> Option<OutboundEntry> {
        self.builds_outstanding.remove(id);
        self.outbound.remove(id)
    }

    pub fn cancel<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        id: &[u8; 32],
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let entry = self.remove_outbound(id).ok_or(RouterError::NotFound)?;
        self.preemptive_path_requests.remove(id);
        self.persistence_dirty = true;
        let mut output = RouterOutput {
            core: TickOutput::default(),
            events: vec![RouterEvent::MessageState {
                message_id: *id,
                state: MessageState::Cancelled,
            }],
        };
        if entry.message().method == DeliveryMethod::Propagated {
            if let Some(mut propagation) = self.propagation.take() {
                output
                    .core
                    .merge(propagation.cancel_outbound_message(node, id));
                self.propagation = Some(propagation);
            }
        } else {
            output.core.merge(self.node.cancel_outbound(node, id));
        }
        self.apply_deadline(&mut output.core);
        Ok(self.finish_output(output))
    }

    /// Attach a generated PoW or ticket stamp to a queued outbound message.
    ///
    /// The message ID does not cover the stamp, so this preserves both the ID
    /// and signature. Invalid stamp lengths leave the queued entry unchanged.
    /// The entry and router scheduler are made immediately due so an event loop
    /// can retry delivery without waiting for the previous stamp-pending delay.
    pub fn set_outbound_stamp(
        &mut self,
        id: &[u8; 32],
        stamp: Vec<u8>,
        now_ms: u64,
    ) -> Result<RouterOutput, RouterError> {
        let entry = self.outbound.get_mut(id).ok_or(RouterError::NotFound)?;
        entry.set_message_stamp(stamp, &mut self.build_epochs)?;
        if entry.message().method == DeliveryMethod::Propagated {
            // Recipient delivery stamps are inside the encrypted transient
            // bytes. Any late change therefore invalidates the transient ID
            // and its independent propagation-node stamp.
            entry.set_propagation(None, &mut self.build_epochs);
        }
        entry.next_attempt_ms = now_ms;
        self.next_job_ms = self.next_job_ms.min(now_ms);
        self.persistence_dirty = true;

        let mut output = RouterOutput {
            core: TickOutput::default(),
            events: Vec::new(),
        };
        self.apply_deadline(&mut output.core);
        Ok(self.finish_output(output))
    }

    pub(super) fn push_upload_build(
        &mut self,
        message_id: [u8; 32],
        params: UploadSendParams,
        data: Vec<u8>,
    ) {
        let Some(entry) = self.outbound.get(&message_id) else {
            return;
        };
        self.pending_builds.push(PendingResourceBuild {
            message_id,
            restore_generation: self.restore_generation,
            build_epoch: entry.build_epoch(),
            kind: PendingBuildKind::Upload { params, data },
        });
        self.builds_outstanding.insert(message_id);
    }

    pub(super) fn has_outstanding_build(&self, message_id: &[u8; 32]) -> bool {
        self.builds_outstanding.contains(message_id)
    }

    /// Take the Resource builds [`tick`](Self::tick) handed out, to run off the
    /// caller's lock. Each result goes back through
    /// [`commit_resource_build`](Self::commit_resource_build).
    ///
    /// Draining also releases the outstanding-build markers: from here the
    /// router holds nothing for those messages, and a caller that never returns
    /// what it took must get the work offered again rather than have its
    /// messages sit forever behind a marker only it could clear.
    pub fn take_resource_builds(&mut self) -> Vec<PendingResourceBuild> {
        self.builds_outstanding.clear();
        core::mem::take(&mut self.pending_builds)
    }

    /// Install a built transfer and emit its advertisement.
    ///
    /// This is where a deferred send becomes a submission: the entry goes to
    /// `MessageState::Sending` and takes its attempt here, at the point the
    /// core accepts the transfer, exactly as the composed path does inside
    /// [`tick`](Self::tick). Nothing before this point may claim success.
    ///
    /// A build whose message has left the queue, or that was captured before a
    /// [`restore`](Self::restore), is refused with [`RouterError::StaleBuild`]
    /// and dropped; the entry, if there is one, is left untouched.
    ///
    /// Reports a link that re-keyed while the build ran as
    /// `RouterError::Node(LxmfNodeError::Resource(ResourceError::LinkStateChanged))`
    /// — an upload reports the same through `RouterError::PropagationTransport`.
    /// That build is spent either way; the message is retried by a later tick.
    pub fn commit_resource_build<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        built: BuiltResource,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let now_unix = emission_secs(node);
        let now_ms = node.now_ms();
        let message_id = built.message_id;
        if built.restore_generation != self.restore_generation {
            // The queue this build was made against no longer exists. The
            // marker, if any, belongs to a build captured after the restore.
            return Err(RouterError::StaleBuild);
        }
        let Some(entry) = self.outbound.get(&message_id) else {
            self.builds_outstanding.remove(&message_id);
            return Err(RouterError::StaleBuild);
        };
        if built.build_epoch != entry.build_epoch() {
            // The entry changed `message`, `state` or `propagation` since the
            // capture, so the built bytes or their target are superseded.
            self.builds_outstanding.remove(&message_id);
            return Err(RouterError::StaleBuild);
        }
        self.builds_outstanding.remove(&message_id);
        let mut output = match built.kind {
            BuiltKind::Delivery(prepared) => {
                let committed = self.node.commit_resource_send(node, *prepared)?;
                let mut output = RouterOutput {
                    core: committed.core,
                    events: Vec::new(),
                };
                if let Some(entry) = self.outbound.get_mut(&message_id) {
                    let representation = LxmfNode::representation(entry.message());
                    record_successful_submission_attempt(entry, &representation);
                    entry.set_state(MessageState::Sending, &mut self.build_epochs);
                    self.persistence_dirty = true;
                }
                for ev in committed.events {
                    self.handle_node_event(ev, now_ms, now_unix, &mut output.events);
                }
                output
            }
            BuiltKind::Upload(prepared) => {
                // The upload path takes its `Sending` mark from the transport's
                // `UploadSubmitted` event, which `commit_upload` runs through
                // the same handler a composed submission does.
                let mut propagation = self
                    .propagation
                    .take()
                    .ok_or(RouterError::PropagationNodeUnavailable)?;
                let result = propagation.commit_upload(self, node, prepared, now_unix);
                self.propagation = Some(propagation);
                result?
            }
        };
        self.apply_deadline(&mut output.core);
        Ok(self.finish_output(output))
    }

    /// Attach the result of a detached recipient-stamp request after checking
    /// that its message and advertised cost are still current.
    pub fn set_outbound_stamp_result<R, C, S>(
        &mut self,
        node: &NodeCore<R, C, S>,
        request: &DeliveryStampRequest,
        stamp: Vec<u8>,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let now_unix = emission_secs(node);
        let current = self
            .outbound
            .get(&request.message_id)
            .filter(|entry| entry.message().stamp.is_none())
            .and_then(|entry| {
                self.outbound_stamp_cost_at(&entry.message().destination_hash, now_unix)
            });
        if current != Some(request.target_cost) {
            return Err(RouterError::StaleStampRequest);
        }
        self.set_outbound_stamp(&request.message_id, stamp, node.now_ms())
    }

    /// Return the detached work request needed by an external propagation
    /// stamp executor (for example a WASM worker or Rayon pool).
    pub fn outbound_propagation_stamp_request(
        &self,
        id: &[u8; 32],
    ) -> Option<PropagationStampRequest> {
        let propagation = self.outbound.get(id)?.propagation()?;
        (propagation.stamp.is_none()).then_some(PropagationStampRequest {
            message_id: *id,
            transient_id: propagation.transient_id,
            target_cost: propagation.target_cost?,
        })
    }

    /// Return detached recipient-stamp work for a queued outbound message.
    ///
    /// The request contains no router or NodeCore borrow and can safely run on
    /// a cooperative executor, a WASM worker, Rayon, or dedicated hardware
    /// while receive processing continues.
    pub fn outbound_stamp_request<R, C, S>(
        &self,
        node: &NodeCore<R, C, S>,
        id: &[u8; 32],
    ) -> Option<DeliveryStampRequest>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let entry = self.outbound.get(id)?;
        let target_cost =
            self.outbound_stamp_cost_at(&entry.message().destination_hash, emission_secs(node))?;
        (entry.message().stamp.is_none() && target_cost > 0).then_some(DeliveryStampRequest {
            message_id: *id,
            target_cost,
        })
    }

    /// Attach the separate 32-byte propagation-node stamp to a queued message.
    pub fn set_outbound_propagation_stamp(
        &mut self,
        id: &[u8; 32],
        stamp: [u8; STAMP_SIZE],
        now_ms: u64,
    ) -> Result<RouterOutput, RouterError> {
        let entry = self.outbound.get_mut(id).ok_or(RouterError::NotFound)?;
        if entry.message().method != DeliveryMethod::Propagated {
            return Err(RouterError::UnsupportedMethod);
        }
        if entry
            .propagation()
            .ok_or(RouterError::PropagationStampUnavailable)?
            .target_cost
            .is_none()
        {
            return Err(RouterError::PropagationStampUnavailable);
        }
        entry.set_propagation_stamp(stamp, &mut self.build_epochs);
        entry.next_attempt_ms = now_ms;
        self.next_job_ms = self.next_job_ms.min(now_ms);
        self.persistence_dirty = true;

        let mut output = RouterOutput::default();
        self.apply_deadline(&mut output.core);
        Ok(self.finish_output(output))
    }

    /// Attach a detached propagation-stamp result only if the prepared
    /// ciphertext, transient ID, node cost and empty-stamp state still match.
    pub fn set_outbound_propagation_stamp_result(
        &mut self,
        request: &PropagationStampRequest,
        stamp: [u8; STAMP_SIZE],
        now_ms: u64,
    ) -> Result<RouterOutput, RouterError> {
        let current = self
            .outbound
            .get(&request.message_id)
            .and_then(OutboundEntry::propagation);
        if !current.is_some_and(|prepared| {
            prepared.transient_id == request.transient_id
                && prepared.target_cost == Some(request.target_cost)
                && prepared.stamp.is_none()
        }) {
            return Err(RouterError::StaleStampRequest);
        }
        self.set_outbound_propagation_stamp(&request.message_id, stamp, now_ms)
    }

    /// The unexpired stamp cost a peer announced for `destination`, restricted
    /// to the window the reference is willing to announce (Codeberg #181).
    ///
    /// The reference applies no bound on the read side: `received_announce`
    /// (Handlers.py:17-18) stores whatever `stamp_cost_from_app_data` returned
    /// and `get_stamp` feeds it to `generate_stamp` (LXMessage.py:320), whose
    /// search loop (LXStamper.py:199) runs until `stamp_valid` holds. At cost
    /// 255 the target is `1 << 1` and the loop never terminates, so a single
    /// hostile or buggy announce wedges the sender's outbound queue.
    ///
    /// Filtering here is a deviation under the rule in `CLAUDE.md`. It is
    /// wire-invisible (we send nothing different), semantically invisible (the
    /// reference's own emitter refuses to announce 0 or 255, so no conforming
    /// peer can be on the receiving end of the change), and it removes an
    /// unbounded loop reachable from the network — Priority 1. The bound is
    /// exactly the reference's emit window and not a lower, "sane" ceiling:
    /// refusing a legal cost of, say, 40 would be observable, because that peer
    /// would then reject every message we send it as unstamped.
    ///
    /// This does not make stamp generation bounded. Any cost above roughly 40
    /// bits is already unreachable in practice; only the strictly
    /// non-terminating case is removed. Bounding the work for legal costs is a
    /// scheduling question (cancellation, deadlines), not a compatibility one.
    pub fn outbound_stamp_cost<R, C, S>(
        &self,
        node: &NodeCore<R, C, S>,
        destination: &[u8; 16],
    ) -> Option<u8>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        self.outbound_stamp_cost_at(destination, emission_secs(node))
    }

    /// [`Self::outbound_stamp_cost`] at an already-resolved timebase, for the
    /// internal paths that took one wall-clock reading for the whole operation.
    fn outbound_stamp_cost_at(&self, destination: &[u8; 16], now_unix: f64) -> Option<u8> {
        self.outbound_stamp_costs
            .get(destination)
            .filter(|(seen, _, _)| now_unix - *seen <= STAMP_COST_EXPIRY as f64)
            .and_then(|(_, cost, _)| *cost)
            .filter(|cost| *cost > 0 && *cost < 255)
    }

    pub fn handle_event<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        event: &NodeEvent,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let now_unix = emission_secs(node);
        let node_output = self.node.handle_event(node, event)?;
        let mut output = RouterOutput {
            core: node_output.core,
            events: Vec::new(),
        };
        for event in node_output.events {
            self.handle_node_event(event, node.now_ms(), now_unix, &mut output.events);
        }
        if let NodeEvent::LinkClosed {
            destination_hash,
            is_initiator: true,
            ..
        } = event
        {
            let needs_direct_link = self.outbound.values().any(|entry| {
                entry.message().destination_hash == destination_hash.into_bytes()
                    && LxmfNode::representation(entry.message())
                        != Ok(DeliveryRepresentation::OpportunisticPacket)
            });
            if needs_direct_link {
                // Python requests the path again when a direct delivery link
                // closes while a message still needs it. Do not expire a
                // cached path here; NodeCore handles failed-handshake expiry.
                output.core.merge(node.request_path(destination_hash));
            }
        }
        if let NodeEvent::AnnounceReceived { announce, .. } = event {
            // Only `lxmf.delivery` app data is a `DeliveryAnnounce`. Python
            // reaches the same restriction by registering its announce handler
            // on that aspect (LXMRouter.py:270-276); here the whole NodeEvent
            // stream arrives, so the filter is explicit. Without it a
            // propagation-node announce would decode into a peer with a
            // nonsense display name.
            if announce.name_hash() == &Destination::compute_name_hash(APP_NAME, &[DELIVERY_ASPECT])
            {
                let data = announce.app_data();
                if let Ok(decoded) = DeliveryAnnounce::decode(data) {
                    if !data.is_empty() {
                        self.insert_bounded_stamp_cost(
                            announce.destination_hash().into_bytes(),
                            (now_unix, decoded.stamp_cost, decoded.compression_supported),
                        );
                    }
                    output.events.push(RouterEvent::PeerAnnounced {
                        destination: *announce.destination_hash(),
                        announce: decoded,
                    });
                }
            }
        }
        if let Some(mut propagation) = self.propagation.take() {
            let propagation_output = propagation.handle_event(self, node, event, now_unix);
            self.propagation = Some(propagation);
            output.merge(propagation_output?);
        }
        self.apply_deadline(&mut output.core);
        Ok(self.finish_output(output))
    }

    /// Make queued direct deliveries immediately eligible when their Link
    /// activates. Python LXMF installs `process_outbound` as the Link's
    /// establishment callback, so retaining the earlier retry deadline here
    /// would add up to `DELIVERY_RETRY_WAIT` of avoidable latency.
    fn wake_direct_outbound(&mut self, destination: DestinationHash, now_ms: u64) {
        let mut changed = false;
        for entry in self.outbound.values_mut() {
            if entry.state() == MessageState::Outbound
                && entry.message().destination_hash == destination.into_bytes()
                && LxmfNode::representation(entry.message())
                    != Ok(DeliveryRepresentation::OpportunisticPacket)
                && entry.next_attempt_ms > now_ms
            {
                entry.next_attempt_ms = now_ms;
                changed = true;
            }
        }
        if changed {
            if self.next_job_ms > now_ms {
                self.next_job_ms = now_ms;
            }
            self.persistence_dirty = true;
        }
    }

    fn handle_node_event(
        &mut self,
        event: LxmfNodeEvent,
        now_ms: u64,
        now_unix: f64,
        out: &mut Vec<RouterEvent>,
    ) {
        match event {
            LxmfNodeEvent::MessageReceived(message) => {
                self.handle_inbound_message(message, now_unix, out)
            }
            LxmfNodeEvent::Delivered { message_id } => {
                if let Some(mut entry) = self.remove_outbound(&message_id) {
                    self.preemptive_path_requests.remove(&message_id);
                    self.persistence_dirty = true;
                    entry.set_state(MessageState::Delivered, &mut self.build_epochs);
                    if entry.message().fields.iter().any(|(key, raw)| {
                        *key == FIELD_TICKET
                            && Ticket::from_field_value(raw).is_ok_and(|ticket| {
                                self.tickets
                                    .contains_inbound(&entry.message().destination_hash, &ticket)
                            })
                    }) {
                        self.tickets
                            .mark_delivered(entry.message().destination_hash, now_unix);
                    }
                    out.push(RouterEvent::MessageState {
                        message_id,
                        state: MessageState::Delivered,
                    });
                }
            }
            LxmfNodeEvent::DeliveryFailed {
                message_id,
                reason: DeliveryFailure::Resource(ResourceError::Cancelled),
            } => {
                if let Some(mut entry) = self.remove_outbound(&message_id) {
                    self.preemptive_path_requests.remove(&message_id);
                    self.persistence_dirty = true;
                    entry.set_state(MessageState::Rejected, &mut self.build_epochs);
                    out.push(RouterEvent::MessageState {
                        message_id,
                        state: MessageState::Rejected,
                    });
                }
            }
            LxmfNodeEvent::DeliveryFailed { message_id, .. } => {
                let mut changed = false;
                if let Some(entry) = self.outbound.get_mut(&message_id) {
                    let next_attempt_ms =
                        entry.next_attempt_ms.saturating_add(DELIVERY_RETRY_WAIT_MS);
                    changed = entry.set_state(MessageState::Outbound, &mut self.build_epochs)
                        || entry.next_attempt_ms != next_attempt_ms;
                    entry.next_attempt_ms = next_attempt_ms;
                }
                if changed {
                    self.persistence_dirty = true;
                }
            }
            LxmfNodeEvent::DirectLinkEstablished {
                destination,
                link_id,
                is_initiator,
            } => {
                if let Some(destination) = destination {
                    self.wake_direct_outbound(destination, now_ms);
                }
                out.push(RouterEvent::DirectLinkEstablished {
                    destination,
                    link_id,
                    is_initiator,
                });
            }
            LxmfNodeEvent::Submitted {
                message_id,
                method: DeliveryMethod::Opportunistic,
                ..
            } => {
                // Python LXMessage.send() marks an opportunistic packet SENT
                // immediately after it has been submitted to Reticulum. A
                // later proof promotes it to DELIVERED, while a missing proof
                // leaves it eligible for the normal router retries.
                if let Some(entry) = self.outbound.get_mut(&message_id) {
                    if entry.set_state(MessageState::Sent, &mut self.build_epochs) {
                        self.persistence_dirty = true;
                    }
                }
            }
            LxmfNodeEvent::Progress {
                message_id,
                progress,
            } => {
                let mut changed = false;
                if let Some(entry) = self.outbound.get_mut(&message_id) {
                    changed = entry.progress.to_bits() != progress.to_bits();
                    entry.progress = progress;
                }
                if changed {
                    self.persistence_dirty = true;
                }
            }
            LxmfNodeEvent::InboundRejected { method, reason } => {
                out.push(RouterEvent::InboundRejected { method, reason });
            }
            _ => {}
        }
    }

    /// Apply the common inbound policy before a message reaches the durable
    /// de-duplication and delivery path. Propagation client downloads use this
    /// same gateway as messages received directly by [`LxmfNode`].
    fn handle_inbound_message(
        &mut self,
        message: Message,
        now_unix: f64,
        out: &mut Vec<RouterEvent>,
    ) {
        // Python accepts a reply ticket from any signature-valid message
        // before duplicate and stamp enforcement. The ticket itself is signed
        // as part of the field map, so remembering it here is safe and lets a
        // peer bootstrap ticketed replies even when this delivery is rejected.
        self.remember_verified_ticket(&message, now_unix);
        if self.config.enforce_stamps && self.config.inbound_stamp_cost.unwrap_or(0) > 0 {
            if message.verification != Verification::Valid {
                self.accept_inbound(message, now_unix, out);
                return;
            }
            if message.stamp.as_deref().is_some_and(|stamp| {
                self.tickets.validates_inbound_stamp(
                    &message.source_hash,
                    &message.message_id,
                    stamp,
                    now_unix,
                )
            }) {
                self.accept_inbound(message, now_unix, out);
                return;
            }
            #[cfg(not(feature = "pow"))]
            {
                out.push(RouterEvent::InvalidStamp(message.message_id));
            }
            #[cfg(feature = "pow")]
            if message
                .stamp
                .as_ref()
                .is_none_or(|stamp| stamp.len() != STAMP_SIZE)
                || self.pending_inbound_stamps.len() >= self.config.max_outbound
            {
                out.push(RouterEvent::InvalidStamp(message.message_id));
            } else {
                let message_id = message.message_id;
                let stamp: [u8; STAMP_SIZE] = message
                    .stamp
                    .as_deref()
                    .and_then(|stamp| stamp.try_into().ok())
                    .expect("stamp length was checked above");
                self.pending_inbound_stamps.push_back((message, now_unix));
                out.push(RouterEvent::InboundStampPending(InboundStampRequest {
                    message_id,
                    stamp,
                    target_cost: self.config.inbound_stamp_cost.unwrap_or(0),
                }));
            }
        } else {
            self.accept_inbound(message, now_unix, out);
        }
    }

    fn remember_verified_ticket(&mut self, message: &Message, now_unix: f64) {
        if message.verification != Verification::Valid {
            return;
        }
        let Some((_, raw)) = message.fields.iter().find(|(key, _)| *key == FIELD_TICKET) else {
            return;
        };
        let Ok(ticket) = Ticket::from_field_value(raw) else {
            return;
        };
        let replacing = self
            .tickets
            .outbound_entries()
            .contains_key(&message.source_hash);
        if !replacing && self.tickets.entry_count() >= self.config.max_tickets {
            return;
        }
        let changed = self.tickets.outbound_entries().get(&message.source_hash) != Some(&ticket);
        if self.tickets.remember(message.source_hash, ticket, now_unix) && changed {
            self.persistence_dirty = true;
        }
    }

    fn accept_inbound(&mut self, message: Message, now_unix: f64, out: &mut Vec<RouterEvent>) {
        if self.ignored.contains(&message.source_hash) {
            return;
        }
        // Match Python LXMF: a message from an identity that has not announced
        // yet is still delivered with SOURCE_UNKNOWN semantics. The caller can
        // present it as unverified, while a signature that was checked and is
        // cryptographically invalid remains rejected.
        if message.verification == Verification::Invalid {
            out.push(RouterEvent::InvalidSignature(message.message_id));
            return;
        }
        if self.delivered_ids.contains_key(&message.message_id) {
            out.push(RouterEvent::Duplicate(message.message_id));
            return;
        }
        self.insert_bounded_id(message.message_id, now_unix, true);
        out.push(RouterEvent::MessageReceived(Box::new(message)));
    }

    fn insert_bounded_id(&mut self, id: [u8; 32], time: f64, delivered: bool) {
        let (map, cap) = if delivered {
            (&mut self.delivered_ids, self.config.max_delivered_ids)
        } else {
            (&mut self.processed_ids, self.config.max_processed_ids)
        };
        if cap == 0 {
            return;
        }
        let mut changed = false;
        if !map.contains_key(&id) && map.len() >= cap {
            if let Some(oldest) = map
                .iter()
                .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(core::cmp::Ordering::Equal))
                .map(|(id, _)| *id)
            {
                map.remove(&oldest);
                changed = true;
            }
        }
        changed |= map
            .insert(id, time)
            .is_none_or(|previous| previous.to_bits() != time.to_bits());
        if changed {
            self.persistence_dirty = true;
        }
    }

    fn insert_bounded_stamp_cost(&mut self, destination: [u8; 16], entry: StampCostEntry) {
        let cap = self.config.max_stamp_costs;
        if cap == 0 {
            return;
        }
        let mut changed = false;
        if !self.outbound_stamp_costs.contains_key(&destination)
            && self.outbound_stamp_costs.len() >= cap
        {
            if let Some(oldest) = self
                .outbound_stamp_costs
                .iter()
                .min_by(|a, b| {
                    a.1 .0
                        .partial_cmp(&b.1 .0)
                        .unwrap_or(core::cmp::Ordering::Equal)
                })
                .map(|(destination, _)| *destination)
            {
                self.outbound_stamp_costs.remove(&oldest);
                changed = true;
            }
        }
        changed |= self
            .outbound_stamp_costs
            .insert(destination, entry)
            .is_none_or(|previous| previous != entry);
        if changed {
            self.persistence_dirty = true;
        }
    }

    pub fn tick<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
    ) -> Result<RouterOutput, RouterError>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let now_unix = emission_secs(node);
        let now_ms = node.now_ms();
        let mut output = RouterOutput::default();
        if let Some(mut propagation) = self.propagation.take() {
            let client_output = propagation.tick(self, node, now_unix);
            self.propagation = Some(propagation);
            output.merge(client_output?);
        }
        if now_ms < self.next_job_ms {
            self.apply_deadline(&mut output.core);
            return Ok(self.finish_output(output));
        }
        // This scheduler cursor is derived, and is captured opportunistically
        // with the next semantic checkpoint change. Advancing it alone must
        // not force an idle host to write storage every processing interval.
        self.next_job_ms = now_ms.saturating_add(PROCESSING_INTERVAL_MS);
        let due: Vec<[u8; 32]> = self
            .outbound
            .iter()
            .filter_map(|(id, e)| {
                (e.message().method != DeliveryMethod::Propagated && e.next_attempt_ms <= now_ms)
                    .then_some(*id)
            })
            .collect();
        for id in due {
            let Some(mut entry) = self.outbound.remove(&id) else {
                continue;
            };
            self.persistence_dirty = true;
            let representation = LxmfNode::representation(entry.message());
            let uses_direct_transport = matches!(
                representation,
                Ok(DeliveryRepresentation::DirectPacket | DeliveryRepresentation::DirectResource)
            );
            // A direct packet or Resource that has already been handed to
            // NodeCore owns its current attempt until NodeCore reports either
            // delivery or failure. Retrying an entry that is still Sending
            // races the active transfer, consumes the retry budget, and can
            // mark a large Resource failed while bytes are still in flight.
            // Python LXMRouter likewise waits while a direct message remains
            // in the SENDING state.
            if uses_direct_transport && entry.state() == MessageState::Sending {
                entry.next_attempt_ms = now_ms.saturating_add(PROCESSING_INTERVAL_MS);
                self.outbound.insert(id, entry);
                continue;
            }

            if entry.attempts >= MAX_DELIVERY_ATTEMPTS {
                self.preemptive_path_requests.remove(&id);
                self.builds_outstanding.remove(&id);
                entry.set_state(MessageState::Failed, &mut self.build_epochs);
                output.events.push(RouterEvent::MessageState {
                    message_id: id,
                    state: MessageState::Failed,
                });
                continue;
            }
            if let Some(target_cost) = entry
                .message()
                .stamp
                .is_none()
                .then(|| self.outbound_stamp_cost_at(&entry.message().destination_hash, now_unix))
                .flatten()
                .filter(|cost| *cost > 0)
            {
                entry.next_attempt_ms = now_ms.saturating_add(PROCESSING_INTERVAL_MS);
                output
                    .events
                    .push(RouterEvent::StampPending(DeliveryStampRequest {
                        message_id: id,
                        target_cost,
                    }));
                self.outbound.insert(id, entry);
                continue;
            }

            if representation == Ok(DeliveryRepresentation::OpportunisticPacket) {
                let destination = DestinationHash::new(entry.message().destination_hash);
                let has_path = node.has_path(&destination);

                // Python LXMRouter.handle_outbound() requests an unknown path
                // once before the first opportunistic delivery attempt. That
                // pre-emptive request does not consume the delivery budget.
                if !has_path && entry.attempts == 0 && !self.preemptive_path_requests.contains(&id)
                {
                    output.core.merge(node.request_path(&destination));
                    self.preemptive_path_requests.insert(id);
                    entry.next_attempt_ms = now_ms.saturating_add(PATH_REQUEST_WAIT_MS);
                    self.outbound.insert(id, entry);
                    continue;
                }

                // After one pathless send attempt, Python requests the path
                // again and counts that as the next delivery attempt.
                if !has_path && entry.attempts >= MAX_PATHLESS_TRIES {
                    output.core.merge(node.request_path(&destination));
                    entry.attempts = entry.attempts.saturating_add(1);
                    entry.next_attempt_ms = now_ms.saturating_add(PATH_REQUEST_WAIT_MS);
                    self.outbound.insert(id, entry);
                    continue;
                }

                // If two opportunistic attempts used a known path without a
                // proof, Python treats the cached route as suspect, drops it,
                // and performs path discovery before trying again.
                if has_path && entry.attempts == MAX_PATHLESS_TRIES.saturating_add(1) {
                    node.remove_path(destination.as_bytes());
                    output.core.merge(node.request_path(&destination));
                    entry.attempts = entry.attempts.saturating_add(1);
                    entry.next_attempt_ms = now_ms.saturating_add(PATH_REQUEST_WAIT_MS);
                    self.outbound.insert(id, entry);
                    continue;
                }
            }

            // A deferred Resource has not been sent. Handing the build out is
            // not a submission and must not be answered like one: the entry
            // stays `Outbound` and comes due again after the retry wait, so a
            // build the caller never returns is retried rather than stranded.
            // `MessageState::Sending` and the attempt belong to
            // `commit_resource_build`, where the core accepts the transfer.
            //
            // Phase 1 stays inside the tick and its `Err` still routes into the
            // send match below, so link establishment and the
            // `DirectLinkUnavailable` handling are shared with a composed send.
            let submitted = if self.config.defer_resource_builds
                && representation == Ok(DeliveryRepresentation::DirectResource)
            {
                if self.builds_outstanding.contains(&id) {
                    entry.next_attempt_ms = now_ms.saturating_add(DELIVERY_RETRY_WAIT_MS);
                    self.outbound.insert(id, entry);
                    continue;
                }
                match self.node.resource_send_params(node, entry.message()) {
                    Ok(params) => {
                        // The entry's writes come first, the epoch capture
                        // second, so the build carries the epoch this tick
                        // leaves behind. The state write is a no-op here and
                        // must not advance it (a bump per re-emission would
                        // refuse every build older than one retry interval).
                        entry.set_state(MessageState::Outbound, &mut self.build_epochs);
                        entry.next_attempt_ms = now_ms.saturating_add(DELIVERY_RETRY_WAIT_MS);
                        self.pending_builds.push(PendingResourceBuild {
                            message_id: id,
                            restore_generation: self.restore_generation,
                            build_epoch: entry.build_epoch(),
                            kind: PendingBuildKind::Delivery {
                                params,
                                message: Box::new(entry.message().clone()),
                                auto_compress: self.node.config().auto_compress_resources,
                            },
                        });
                        self.builds_outstanding.insert(id);
                        output.events.push(RouterEvent::ResourceBuildPending(id));
                        self.outbound.insert(id, entry);
                        continue;
                    }
                    Err(error) => Err(error),
                }
            } else {
                self.node.send(node, entry.message())
            };

            match submitted {
                Ok(sent) => {
                    record_successful_submission_attempt(&mut entry, &representation);
                    entry.set_state(MessageState::Sending, &mut self.build_epochs);
                    entry.next_attempt_ms = now_ms.saturating_add(DELIVERY_RETRY_WAIT_MS);
                    output.core.merge(sent.core);
                    self.outbound.insert(id, entry);
                    for ev in sent.events {
                        self.handle_node_event(ev, now_ms, now_unix, &mut output.events);
                    }
                }
                Err(LxmfNodeError::DirectLinkUnavailable) => {
                    let destination = DestinationHash::new(entry.message().destination_hash);
                    match self.node.ensure_direct_link(node, destination) {
                        Ok((state, link)) => {
                            output.core.merge(link.core);
                            match state {
                                DirectLinkState::Started(_) => {
                                    entry.attempts = entry.attempts.saturating_add(1);
                                    entry.next_attempt_ms =
                                        now_ms.saturating_add(DELIVERY_RETRY_WAIT_MS);
                                }
                                DirectLinkState::PathRequested => {
                                    entry.attempts = entry.attempts.saturating_add(1);
                                    entry.next_attempt_ms =
                                        now_ms.saturating_add(PATH_REQUEST_WAIT_MS);
                                }
                                DirectLinkState::Connecting(_) | DirectLinkState::Ready(_) => {
                                    // Python only waits while an existing link is
                                    // pending; observing it does not consume a
                                    // new delivery attempt.
                                    entry.next_attempt_ms =
                                        now_ms.saturating_add(PROCESSING_INTERVAL_MS);
                                }
                            }
                        }
                        Err(_) => {
                            entry.attempts = entry.attempts.saturating_add(1);
                            entry.next_attempt_ms = now_ms.saturating_add(DELIVERY_RETRY_WAIT_MS);
                        }
                    }
                    self.outbound.insert(id, entry);
                }
                Err(_) => {
                    entry.attempts = entry.attempts.saturating_add(1);
                    entry.next_attempt_ms = now_ms.saturating_add(DELIVERY_RETRY_WAIT_MS);
                    self.outbound.insert(id, entry);
                }
            }
        }
        self.clean(now_unix);
        self.apply_deadline(&mut output.core);
        Ok(self.finish_output(output))
    }

    fn clean(&mut self, now_unix: f64) {
        let stamp_costs_before = self.outbound_stamp_costs.len();
        let delivered_before = self.delivered_ids.len();
        let processed_before = self.processed_ids.len();
        self.outbound_stamp_costs
            .retain(|_, (seen, _, _)| now_unix - *seen <= STAMP_COST_EXPIRY as f64);
        self.delivered_ids
            .retain(|_, seen| now_unix - *seen <= MESSAGE_EXPIRY_SECS * 6.0);
        self.processed_ids
            .retain(|_, seen| now_unix - *seen <= MESSAGE_EXPIRY_SECS * 6.0);
        let tickets_changed = self.tickets.clean(now_unix);
        if stamp_costs_before != self.outbound_stamp_costs.len()
            || delivered_before != self.delivered_ids.len()
            || processed_before != self.processed_ids.len()
            || tickets_changed
        {
            self.persistence_dirty = true;
        }
    }

    /// Finish one public operation, coalescing all checkpoint invalidations in
    /// it (and any earlier mutation-only policy call) into exactly one event.
    fn finish_output(&mut self, mut output: RouterOutput) -> RouterOutput {
        let mut saw_request = false;
        output.events.retain(|event| {
            if matches!(event, RouterEvent::PersistenceRequested) {
                if saw_request {
                    false
                } else {
                    saw_request = true;
                    true
                }
            } else {
                true
            }
        });
        if self.persistence_dirty && !saw_request {
            output.events.push(RouterEvent::PersistenceRequested);
        }
        output
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.outbound
            .values()
            .map(|entry| entry.next_attempt_ms)
            .chain(core::iter::once(self.next_job_ms))
            .chain(
                self.propagation
                    .as_ref()
                    .and_then(PropagationRuntime::next_deadline),
            )
            .min()
    }
    fn apply_deadline(&self, core: &mut TickOutput) {
        core.next_deadline_ms = match (core.next_deadline_ms, self.next_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Persist the bounded, identity-bound client checkpoint as one value.
    /// Storage implementations should replace a single key atomically.
    pub fn persist(&mut self, storage: &mut impl LxmfStorage) -> Result<(), RouterError> {
        let snapshot = self.snapshot()?;
        storage.store(ROUTER_STATE_KEY, &snapshot)?;
        storage.flush()?;
        self.persistence_dirty = false;
        Ok(())
    }

    /// Restore a checkpoint transactionally. No live router field is changed
    /// unless the complete snapshot passes identity, structural and configured
    /// capacity validation.
    pub fn restore(&mut self, storage: &impl LxmfStorage) -> Result<(), RouterError> {
        let Some(snapshot) = storage.load(ROUTER_STATE_KEY)? else {
            return Ok(());
        };
        if snapshot.len() > self.config.max_snapshot_bytes {
            return Err(RouterError::CorruptSnapshot);
        }
        let restored = self.decode_snapshot(&snapshot)?;
        self.outbound = restored.outbound;
        // Every build in flight belongs to the queue this call replaces. The
        // generation is what refuses them — the restored snapshot carries the
        // same message IDs, so membership cannot tell them apart. Dropping the
        // queued ones and their markers is housekeeping on top of that.
        self.restore_generation = self.restore_generation.wrapping_add(1);
        self.pending_builds.clear();
        self.builds_outstanding.clear();
        self.preemptive_path_requests.clear();
        // Queue deadlines and in-flight states are expressed in the host's
        // process-local monotonic clock. That epoch is not stable across a
        // restart, and live Packet/Resource correlation is deliberately not
        // persisted. Preserve durable retry and prepared-upload data, but make
        // every restored entry immediately eligible for a fresh attempt.
        for entry in self.outbound.values_mut() {
            entry.set_state(MessageState::Outbound, &mut self.build_epochs);
            entry.next_attempt_ms = 0;
            entry.progress = 0.01;
        }
        self.outbound_stamp_costs = restored.outbound_stamp_costs;
        self.delivered_ids = restored.delivered_ids;
        self.processed_ids = restored.processed_ids;
        self.tickets = restored.tickets;
        self.ignored = restored.ignored;
        self.next_job_ms = 0;
        self.persistence_dirty = false;
        Ok(())
    }

    fn snapshot(&self) -> Result<Vec<u8>, RouterError> {
        self.validate_persistence_limits()?;
        let mut out = Vec::new();
        msgpack::array(&mut out, SNAPSHOT_FIELDS);
        msgpack::uint(&mut out, SNAPSHOT_VERSION);
        msgpack::bin(&mut out, &self.identity_hash);
        encode_outbound(&mut out, &self.outbound, self.config.max_snapshot_bytes)?;
        encode_id_times(&mut out, &self.delivered_ids);
        encode_id_times(&mut out, &self.processed_ids);
        encode_stamp_costs(&mut out, &self.outbound_stamp_costs);
        encode_tickets(&mut out, &self.tickets);
        encode_fixed_set(&mut out, &self.ignored);
        msgpack::uint(&mut out, self.next_job_ms);
        if out.len() > self.config.max_snapshot_bytes {
            return Err(RouterError::QueueFull);
        }
        Ok(out)
    }

    fn validate_persistence_limits(&self) -> Result<(), RouterError> {
        bounded_count(self.outbound.len(), self.config.max_outbound)?;
        bounded_count(self.delivered_ids.len(), self.config.max_delivered_ids)?;
        bounded_count(self.processed_ids.len(), self.config.max_processed_ids)?;
        bounded_count(self.outbound_stamp_costs.len(), self.config.max_stamp_costs)?;
        bounded_count(self.ignored.len(), self.config.max_policy_entries)?;
        validate_ticket_count(&self.tickets, self.config.max_tickets)?;

        if self.outbound.iter().any(|(message_id, entry)| {
            *message_id != entry.message().message_id
                || !entry.message().timestamp.is_finite()
                || !entry.progress.is_finite()
                || !(0.0..=1.0).contains(&entry.progress)
                || entry.message().method == DeliveryMethod::Paper
                || (entry.message().method != DeliveryMethod::Propagated
                    && entry.propagation().is_some())
                || entry.propagation().is_some_and(|propagation| {
                    !propagation.timebase.is_finite()
                        || propagation.unstamped_lxmf.len() <= LXMF_OVERHEAD
                        || propagation.unstamped_lxmf[..16] != entry.message().destination_hash
                        || full_hash(&propagation.unstamped_lxmf) != propagation.transient_id
                        || (propagation.stamp.is_some() && propagation.target_cost.is_none())
                })
        }) || self.delivered_ids.values().any(|time| !time.is_finite())
            || self.processed_ids.values().any(|time| !time.is_finite())
            || self
                .outbound_stamp_costs
                .values()
                .any(|(seen, _, _)| !seen.is_finite())
            || self
                .tickets
                .inbound()
                .values()
                .flatten()
                .any(|ticket| !ticket.expires_unix.is_finite())
            || self
                .tickets
                .outbound_entries()
                .values()
                .any(|ticket| !ticket.expires_unix.is_finite())
            || self
                .tickets
                .last_deliveries()
                .values()
                .any(|time| !time.is_finite())
        {
            return Err(RouterError::CorruptSnapshot);
        }
        Ok(())
    }

    fn decode_snapshot(&mut self, data: &[u8]) -> Result<RestoredRouterState, RouterError> {
        let mut p = 0;
        if msgpack::array_len(data, &mut p)? != SNAPSHOT_FIELDS {
            return Err(RouterError::CorruptSnapshot);
        }
        let snapshot_version = msgpack::read_uint(data, &mut p)?;
        if !(3..=SNAPSHOT_VERSION).contains(&snapshot_version) {
            return Err(RouterError::CorruptSnapshot);
        }
        let identity_hash: [u8; 16] = read_fixed(data, &mut p)?;
        if identity_hash != self.identity_hash {
            return Err(RouterError::CorruptSnapshot);
        }

        // Restored entries get fresh epochs from the live counter. The epoch
        // is deliberately not persisted: a restored value would validate a
        // build captured before the restore.
        let outbound = decode_outbound(
            data,
            &mut p,
            self.config.max_outbound,
            snapshot_version,
            &mut self.build_epochs,
        )?;
        let delivered_ids = decode_id_times(data, &mut p, self.config.max_delivered_ids)?;
        let processed_ids = decode_id_times(data, &mut p, self.config.max_processed_ids)?;
        let outbound_stamp_costs = decode_stamp_costs(data, &mut p, self.config.max_stamp_costs)?;
        let tickets = decode_tickets(data, &mut p, self.config.max_tickets)?;
        let ignored = decode_fixed_set(data, &mut p, self.config.max_policy_entries)?;
        // Consume the legacy persisted cursor for snapshot compatibility. It
        // belongs to the previous process's monotonic-clock epoch and must not
        // be restored.
        let _persisted_next_job_ms = msgpack::read_uint(data, &mut p)?;
        if p != data.len() {
            return Err(RouterError::CorruptSnapshot);
        }

        Ok(RestoredRouterState {
            outbound,
            outbound_stamp_costs,
            delivered_ids,
            processed_ids,
            tickets,
            ignored,
        })
    }
}

fn bounded_count(count: usize, limit: usize) -> Result<(), RouterError> {
    if count > limit || u32::try_from(count).is_err() {
        Err(RouterError::QueueFull)
    } else {
        Ok(())
    }
}

fn decode_count(data: &[u8], p: &mut usize, limit: usize) -> Result<usize, RouterError> {
    let count = msgpack::array_len(data, p)?;
    if count > limit {
        Err(RouterError::CorruptSnapshot)
    } else {
        Ok(count)
    }
}

fn read_fixed<const N: usize>(data: &[u8], p: &mut usize) -> Result<[u8; N], RouterError> {
    msgpack::read_bin(data, p)?
        .try_into()
        .map_err(|_| RouterError::CorruptSnapshot)
}

fn read_finite(data: &[u8], p: &mut usize) -> Result<f64, RouterError> {
    let value = msgpack::read_number_f64(data, p)?;
    value
        .is_finite()
        .then_some(value)
        .ok_or(RouterError::CorruptSnapshot)
}

fn encode_outbound(
    out: &mut Vec<u8>,
    entries: &BTreeMap<[u8; 32], OutboundEntry>,
    max_snapshot_bytes: usize,
) -> Result<(), RouterError> {
    msgpack::array(out, entries.len());
    for entry in entries.values() {
        let packed = entry.message().pack();
        if u32::try_from(packed.len()).is_err()
            || packed.len() > max_snapshot_bytes.saturating_sub(out.len())
        {
            return Err(RouterError::QueueFull);
        }
        msgpack::array(out, 8);
        msgpack::bin(out, &packed);
        msgpack::uint(out, entry.message().method as u64);
        msgpack::uint(out, entry.state() as u64);
        msgpack::uint(out, verification_value(entry.message().verification));
        msgpack::uint(out, entry.attempts as u64);
        msgpack::uint(out, entry.next_attempt_ms);
        msgpack::f64(out, entry.progress as f64);
        if let Some(propagation) = entry.propagation() {
            if u32::try_from(propagation.unstamped_lxmf.len()).is_err()
                || propagation.unstamped_lxmf.len() > max_snapshot_bytes.saturating_sub(out.len())
            {
                return Err(RouterError::QueueFull);
            }
            msgpack::array(out, 5);
            msgpack::f64(out, propagation.timebase);
            msgpack::bin(out, &propagation.unstamped_lxmf);
            msgpack::bin(out, &propagation.transient_id);
            encode_optional_u64(out, propagation.target_cost.map(u64::from));
            if let Some(stamp) = propagation.stamp {
                msgpack::bin(out, &stamp);
            } else {
                msgpack::nil(out);
            }
        } else {
            msgpack::nil(out);
        }
    }
    Ok(())
}

fn decode_outbound(
    data: &[u8],
    p: &mut usize,
    limit: usize,
    snapshot_version: u64,
    epochs: &mut BuildEpochs,
) -> Result<BTreeMap<[u8; 32], OutboundEntry>, RouterError> {
    let count = decode_count(data, p, limit)?;
    let mut entries = BTreeMap::new();
    for _ in 0..count {
        let expected_fields = if snapshot_version >= 4 { 8 } else { 7 };
        if msgpack::array_len(data, p)? != expected_fields {
            return Err(RouterError::CorruptSnapshot);
        }
        let packed = msgpack::read_bin(data, p)?;
        let method = method_from(msgpack::read_uint(data, p)?)?;
        let state = state_from(msgpack::read_uint(data, p)?)?;
        let verification = verification_from(msgpack::read_uint(data, p)?)?;
        let attempts =
            u8::try_from(msgpack::read_uint(data, p)?).map_err(|_| RouterError::CorruptSnapshot)?;
        let next_attempt_ms = msgpack::read_uint(data, p)?;
        let progress = read_finite(data, p)?;
        if !(0.0..=1.0).contains(&progress) {
            return Err(RouterError::CorruptSnapshot);
        }
        let propagation = if snapshot_version < 4 {
            None
        } else if msgpack::peek_kind(data, *p)? == msgpack::Kind::Nil {
            msgpack::read_nil(data, p)?;
            None
        } else {
            if msgpack::array_len(data, p)? != 5 {
                return Err(RouterError::CorruptSnapshot);
            }
            let timebase = read_finite(data, p)?;
            let unstamped_lxmf = msgpack::read_bin(data, p)?.to_vec();
            let transient_id = read_fixed(data, p)?;
            let target_cost = read_optional_u64(data, p)?
                .map(|value| u8::try_from(value).map_err(|_| RouterError::CorruptSnapshot))
                .transpose()?;
            let stamp = if msgpack::peek_kind(data, *p)? == msgpack::Kind::Nil {
                msgpack::read_nil(data, p)?;
                None
            } else {
                Some(read_fixed(data, p)?)
            };
            Some(OutboundPropagation {
                timebase,
                unstamped_lxmf,
                transient_id,
                target_cost,
                stamp,
            })
        };

        // Snapshots always carry the fully packed form, even for an
        // opportunistic message. Decode that form first, then restore the
        // selected transport method and local verification state.
        let mut message = Message::unpack(packed, None, None, DeliveryMethod::Direct)
            .map_err(|_| RouterError::CorruptSnapshot)?;
        message.method = method;
        message.verification = verification;
        if message.method == DeliveryMethod::Paper
            || (message.method != DeliveryMethod::Propagated && propagation.is_some())
            || propagation.as_ref().is_some_and(|prepared| {
                !prepared.timebase.is_finite()
                    || prepared.unstamped_lxmf.len() <= LXMF_OVERHEAD
                    || prepared.unstamped_lxmf[..16] != message.destination_hash
                    || full_hash(&prepared.unstamped_lxmf) != prepared.transient_id
                    || (prepared.stamp.is_some() && prepared.target_cost.is_none())
            })
        {
            return Err(RouterError::CorruptSnapshot);
        }
        let id = message.message_id;
        if entries
            .insert(
                id,
                OutboundEntry::new(
                    message,
                    state,
                    attempts,
                    next_attempt_ms,
                    progress as f32,
                    propagation,
                    epochs,
                ),
            )
            .is_some()
        {
            return Err(RouterError::CorruptSnapshot);
        }
    }
    Ok(entries)
}

fn encode_id_times(out: &mut Vec<u8>, map: &BTreeMap<[u8; 32], f64>) {
    msgpack::array(out, map.len());
    for (id, time) in map {
        msgpack::array(out, 2);
        msgpack::bin(out, id);
        msgpack::f64(out, *time);
    }
}

fn decode_id_times(
    data: &[u8],
    p: &mut usize,
    limit: usize,
) -> Result<BTreeMap<[u8; 32], f64>, RouterError> {
    let count = decode_count(data, p, limit)?;
    let mut map = BTreeMap::new();
    for _ in 0..count {
        if msgpack::array_len(data, p)? != 2 {
            return Err(RouterError::CorruptSnapshot);
        }
        let id = read_fixed(data, p)?;
        let time = read_finite(data, p)?;
        if map.insert(id, time).is_some() {
            return Err(RouterError::CorruptSnapshot);
        }
    }
    Ok(map)
}

fn encode_stamp_costs(out: &mut Vec<u8>, costs: &StampCostMap) {
    msgpack::array(out, costs.len());
    for (destination, (seen, cost, compression)) in costs {
        msgpack::array(out, 4);
        msgpack::bin(out, destination);
        msgpack::f64(out, *seen);
        encode_optional_u64(out, cost.map(u64::from));
        msgpack::bool(out, *compression);
    }
}

fn decode_stamp_costs(
    data: &[u8],
    p: &mut usize,
    limit: usize,
) -> Result<StampCostMap, RouterError> {
    let count = decode_count(data, p, limit)?;
    let mut costs = BTreeMap::new();
    for _ in 0..count {
        if msgpack::array_len(data, p)? != 4 {
            return Err(RouterError::CorruptSnapshot);
        }
        let destination = read_fixed(data, p)?;
        let seen = read_finite(data, p)?;
        let cost = read_optional_u64(data, p)?
            .map(|value| u8::try_from(value).map_err(|_| RouterError::CorruptSnapshot))
            .transpose()?;
        let compression = msgpack::read_bool(data, p)?;
        if costs
            .insert(destination, (seen, cost, compression))
            .is_some()
        {
            return Err(RouterError::CorruptSnapshot);
        }
    }
    Ok(costs)
}

fn validate_ticket_count(store: &TicketStore, limit: usize) -> Result<(), RouterError> {
    let inbound = store
        .inbound()
        .values()
        .try_fold(0usize, |total, entries| total.checked_add(entries.len()))
        .ok_or(RouterError::QueueFull)?;
    let total = inbound
        .checked_add(store.outbound_entries().len())
        .and_then(|value| value.checked_add(store.last_deliveries().len()))
        .ok_or(RouterError::QueueFull)?;
    bounded_count(total, limit)
}

fn encode_tickets(out: &mut Vec<u8>, store: &TicketStore) {
    msgpack::array(out, 3);
    let inbound: usize = store.inbound().values().map(Vec::len).sum();
    msgpack::array(out, inbound);
    for (destination, entries) in store.inbound() {
        for ticket in entries {
            msgpack::array(out, 3);
            msgpack::bin(out, destination);
            msgpack::f64(out, ticket.expires_unix);
            msgpack::bin(out, &ticket.secret);
        }
    }
    msgpack::array(out, store.outbound_entries().len());
    for (destination, ticket) in store.outbound_entries() {
        msgpack::array(out, 3);
        msgpack::bin(out, destination);
        msgpack::f64(out, ticket.expires_unix);
        msgpack::bin(out, &ticket.secret);
    }
    msgpack::array(out, store.last_deliveries().len());
    for (destination, time) in store.last_deliveries() {
        msgpack::array(out, 2);
        msgpack::bin(out, destination);
        msgpack::f64(out, *time);
    }
}

fn decode_tickets(data: &[u8], p: &mut usize, limit: usize) -> Result<TicketStore, RouterError> {
    if msgpack::array_len(data, p)? != 3 {
        return Err(RouterError::CorruptSnapshot);
    }
    let mut remaining = limit;
    let mut store = TicketStore::default();

    let inbound_count = decode_count(data, p, remaining)?;
    remaining = remaining.saturating_sub(inbound_count);
    for _ in 0..inbound_count {
        if msgpack::array_len(data, p)? != 3 {
            return Err(RouterError::CorruptSnapshot);
        }
        let destination = read_fixed(data, p)?;
        let expires_unix = read_finite(data, p)?;
        let secret = read_fixed(data, p)?;
        store.restore_inbound(
            destination,
            Ticket {
                expires_unix,
                secret,
            },
        );
    }

    let outbound_count = decode_count(data, p, remaining)?;
    remaining = remaining.saturating_sub(outbound_count);
    for _ in 0..outbound_count {
        if msgpack::array_len(data, p)? != 3 {
            return Err(RouterError::CorruptSnapshot);
        }
        let destination = read_fixed(data, p)?;
        let expires_unix = read_finite(data, p)?;
        let secret = read_fixed(data, p)?;
        if store.outbound_entries().contains_key(&destination) {
            return Err(RouterError::CorruptSnapshot);
        }
        store.restore_outbound(
            destination,
            Ticket {
                expires_unix,
                secret,
            },
        );
    }

    let delivery_count = decode_count(data, p, remaining)?;
    for _ in 0..delivery_count {
        if msgpack::array_len(data, p)? != 2 {
            return Err(RouterError::CorruptSnapshot);
        }
        let destination = read_fixed(data, p)?;
        let time = read_finite(data, p)?;
        if store.last_deliveries().contains_key(&destination) {
            return Err(RouterError::CorruptSnapshot);
        }
        store.restore_last_delivery(destination, time);
    }
    Ok(store)
}

fn encode_fixed_set<const N: usize>(out: &mut Vec<u8>, values: &BTreeSet<[u8; N]>) {
    msgpack::array(out, values.len());
    for value in values {
        msgpack::bin(out, value);
    }
}

fn decode_fixed_set<const N: usize>(
    data: &[u8],
    p: &mut usize,
    limit: usize,
) -> Result<BTreeSet<[u8; N]>, RouterError> {
    let count = decode_count(data, p, limit)?;
    let mut values = BTreeSet::new();
    for _ in 0..count {
        if !values.insert(read_fixed(data, p)?) {
            return Err(RouterError::CorruptSnapshot);
        }
    }
    Ok(values)
}
fn encode_optional_u64(out: &mut Vec<u8>, value: Option<u64>) {
    if let Some(value) = value {
        msgpack::uint(out, value);
    } else {
        msgpack::nil(out);
    }
}

fn read_optional_u64(data: &[u8], p: &mut usize) -> Result<Option<u64>, RouterError> {
    if data.get(*p) == Some(&0xc0) {
        msgpack::read_nil(data, p)?;
        Ok(None)
    } else {
        Ok(Some(msgpack::read_uint(data, p)?))
    }
}

fn method_from(value: u64) -> Result<DeliveryMethod, RouterError> {
    match value {
        1 => Ok(DeliveryMethod::Opportunistic),
        2 => Ok(DeliveryMethod::Direct),
        3 => Ok(DeliveryMethod::Propagated),
        _ => Err(RouterError::CorruptSnapshot),
    }
}
fn state_from(value: u64) -> Result<MessageState, RouterError> {
    match value {
        0 => Ok(MessageState::Generating),
        1 => Ok(MessageState::Outbound),
        2 => Ok(MessageState::Sending),
        4 => Ok(MessageState::Sent),
        8 => Ok(MessageState::Delivered),
        0x10 => Ok(MessageState::AwaitingCollection),
        0xfd => Ok(MessageState::Rejected),
        0xfe => Ok(MessageState::Cancelled),
        0xff => Ok(MessageState::Failed),
        _ => Err(RouterError::CorruptSnapshot),
    }
}

fn verification_value(value: Verification) -> u64 {
    match value {
        Verification::Unverified => 0,
        Verification::Valid => 1,
        Verification::Invalid => 2,
    }
}

fn verification_from(value: u64) -> Result<Verification, RouterError> {
    match value {
        0 => Ok(Verification::Unverified),
        1 => Ok(Verification::Valid),
        2 => Ok(Verification::Invalid),
        _ => Err(RouterError::CorruptSnapshot),
    }
}

impl From<msgpack::Error> for RouterError {
    fn from(_: msgpack::Error) -> Self {
        Self::CorruptSnapshot
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use core::cell::Cell;
    use leviculum_core::{Action, Identity, InterfaceId, MemoryStorage, NodeCoreBuilder};
    use rand_core::OsRng;

    use crate::{
        announce,
        node::{DeliveryFailure, LxmfNodeConfig},
        storage::MemoryLxmfStorage,
    };

    /// Wall time for the whole test module. The router resolves every
    /// wall-clock field through `Transport::emission_secs` (Codeberg #182), so
    /// the injection point for LXMF time is the platform clock, not a
    /// parameter on the call.
    const TEST_WALL_UNIX: u64 = 1_700_000_000;

    struct TestClock(Cell<u64>);

    impl Clock for TestClock {
        fn now_ms(&self) -> u64 {
            self.0.get()
        }
        fn wall_unix_secs(&self) -> Option<u64> {
            Some(TEST_WALL_UNIX)
        }
    }

    type TestNode = NodeCore<OsRng, TestClock, MemoryStorage>;

    fn identity_from(seed: u8) -> Identity {
        let mut private = [0u8; 64];
        for (index, byte) in private.iter_mut().enumerate() {
            *byte = seed.wrapping_add(index as u8);
        }
        Identity::from_private_key_bytes(&private).expect("deterministic identity")
    }

    fn router_and_node(config: RouterConfig) -> (LxmfRouter, TestNode) {
        let mut core = NodeCoreBuilder::new().build(
            OsRng,
            TestClock(Cell::new(1_000)),
            MemoryStorage::with_defaults(),
        );
        let identity = identity_from(1);
        let identity_hash = *identity.hash();
        let destination = LxmfNode::delivery_destination(identity).expect("delivery destination");
        let node = LxmfNode::register(&mut core, destination, LxmfNodeConfig::default())
            .expect("register delivery destination");
        (LxmfRouter::new(node, identity_hash, config), core)
    }

    fn router(config: RouterConfig) -> LxmfRouter {
        router_and_node(config).0
    }

    fn enable_propagation_client(router: &mut LxmfRouter, node: &mut TestNode) {
        let identity = {
            let destination = node
                .destination(&router.node.delivery_destination_hash())
                .expect("registered delivery destination");
            let private = destination
                .identity()
                .expect("delivery destination has identity")
                .private_key_bytes()
                .expect("delivery identity has private keys");
            Identity::from_private_key_bytes(&private).expect("copy delivery identity")
        };
        let destination = PropagationTransport::destination(identity).expect("propagation client");
        let transport = PropagationTransport::register(node, destination)
            .expect("register propagation client destination");
        router
            .enable_propagation_client(transport, PropagationClientConfig::default())
            .expect("matching delivery identity");
    }

    fn checkpoint(router: &mut LxmfRouter) {
        router
            .persist(&mut MemoryLxmfStorage::new(128 * 1024))
            .expect("checkpoint router state");
    }

    fn persistence_request_count(output: &RouterOutput) -> usize {
        output
            .events
            .iter()
            .filter(|event| matches!(event, RouterEvent::PersistenceRequested))
            .count()
    }

    fn message(seed: u8) -> Message {
        let source = identity_from(seed);
        Message::create(
            [seed.wrapping_add(1); 16],
            [seed.wrapping_add(2); 16],
            &source,
            1_700_000_000.25,
            b"persisted title".to_vec(),
            b"persisted content".to_vec(),
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .expect("message")
    }

    fn oversized_opportunistic_message(seed: u8) -> Message {
        let source = identity_from(seed);
        let message = Message::create(
            [seed.wrapping_add(1); 16],
            [seed.wrapping_add(2); 16],
            &source,
            1_700_000_000.25,
            b"oversized opportunistic".to_vec(),
            vec![seed; 1_024],
            Vec::new(),
            DeliveryMethod::Opportunistic,
        )
        .expect("message");
        assert_eq!(
            LxmfNode::representation(&message),
            Ok(DeliveryRepresentation::DirectResource)
        );
        message
    }

    fn announced_peer(router: &mut LxmfRouter, node: &mut TestNode, seed: u8) -> DestinationHash {
        let identity = identity_from(seed);
        let mut destination =
            LxmfNode::delivery_destination(identity).expect("remote delivery destination");
        let destination_hash = *destination.hash();
        let app_data = announce::delivery(Some(b"remote"), None);
        let packet = destination
            .announce(
                Some(&app_data),
                &mut OsRng,
                node.now_ms(),
                node.now_ms() / 1000,
            )
            .expect("remote delivery announce");
        let mut packed = alloc::vec![0; packet.packed_size()];
        let length = packet.pack(&mut packed).expect("pack remote announce");
        let announce_event = node
            .handle_packet(InterfaceId(0), &packed[..length])
            .events
            .into_iter()
            .find(|event| matches!(event, NodeEvent::AnnounceReceived { .. }))
            .expect("remote announce event");
        let _ = router
            .handle_event(node, &announce_event)
            .expect("remember remote announce");
        destination_hash
    }

    /// The wording of an error belongs to the library, once, and not to each
    /// client that has to show one.
    #[test]
    fn error_display_names_the_wrapped_error() {
        assert_eq!(
            alloc::format!("{}", RouterError::QueueFull),
            "outbound queue is full"
        );
        assert_eq!(
            alloc::format!("{}", RouterError::Storage(StorageError::Full)),
            "storage: storage is full"
        );
        assert_eq!(
            alloc::format!(
                "{}",
                RouterError::Node(LxmfNodeError::Message(MessageError::TooShort))
            ),
            "node: message: message is shorter than the LXMF header"
        );
        // And a std host can put one in a `Box<dyn Error>` without wrapping it
        // first.
        let error: &dyn core::error::Error = &RouterError::QueueFull;
        assert_eq!(alloc::format!("{error}"), "outbound queue is full");
    }

    #[test]
    fn unknown_source_message_is_delivered_as_unverified() {
        let mut router = router(RouterConfig::default());
        let mut incoming = message(2);
        incoming.destination_hash = router.node.delivery_destination_hash().into_bytes();
        incoming.verification = Verification::Unverified;
        let message_id = incoming.message_id;
        let mut events = Vec::new();

        router.handle_inbound_message(incoming, 1_700_000_001.0, &mut events);

        assert!(events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageReceived(message)
                if message.message_id == message_id
                    && message.verification == Verification::Unverified
        )));
    }

    #[test]
    fn cryptographically_invalid_message_is_rejected() {
        let mut router = router(RouterConfig::default());
        let mut incoming = message(3);
        incoming.destination_hash = router.node.delivery_destination_hash().into_bytes();
        incoming.verification = Verification::Invalid;
        let message_id = incoming.message_id;
        let mut events = Vec::new();

        router.handle_inbound_message(incoming, 1_700_000_001.0, &mut events);

        assert!(events.iter().any(|event| matches!(
            event,
            RouterEvent::InvalidSignature(id) if *id == message_id
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, RouterEvent::MessageReceived(_))));
    }

    fn populated_router(config: RouterConfig) -> LxmfRouter {
        let (mut router, node) = router_and_node(config);
        let queued = message(40);
        let queued_id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        let outbound = router.outbound.get_mut(&queued_id).unwrap();
        outbound.set_state(MessageState::Sending, &mut router.build_epochs);
        outbound.attempts = 3;
        outbound.next_attempt_ms = 98_765;
        outbound.progress = 0.625;

        router
            .outbound_stamp_costs
            .insert([3; 16], (1_700_000_001.0, Some(9), true));
        router.delivered_ids.insert([4; 32], 1_700_000_002.0);
        router.processed_ids.insert([5; 32], 1_700_000_003.0);

        let issued = router
            .tickets
            .issue([6; 16], 1_700_000_000.0, &mut OsRng)
            .expect("ticket");
        router.tickets.mark_delivered([6; 16], 1_700_000_004.0);
        assert!(router.tickets.remember(
            [7; 16],
            Ticket {
                expires_unix: issued.expires_unix + 1.0,
                secret: [8; 16],
            },
            1_700_000_000.0,
        ));

        router.ignored.insert([22; 16]);
        router.next_job_ms = 543_210;
        router
    }

    #[test]
    fn complete_checkpoint_round_trips_within_storage_bound() {
        let config = RouterConfig::default();
        let mut original = populated_router(config.clone());
        let mut storage = MemoryLxmfStorage::new(128 * 1024);

        original.persist(&mut storage).expect("persist");
        assert!(storage.bytes() <= 128 * 1024);

        let mut restored = router(config);
        restored.restore(&storage).expect("restore");
        for entry in original.outbound.values_mut() {
            entry.set_state(MessageState::Outbound, &mut original.build_epochs);
            entry.next_attempt_ms = 0;
            entry.progress = 0.01;
        }
        original.next_job_ms = 0;
        assert_eq!(restored.snapshot().unwrap(), original.snapshot().unwrap());
        assert_eq!(restored.next_job_ms, 0);
        assert!(restored.ignored.contains(&[22; 16]));
    }

    #[test]
    fn replacing_ignored_policy_is_bounded_and_requests_one_checkpoint() {
        let config = RouterConfig {
            max_policy_entries: 2,
            ..RouterConfig::default()
        };
        let mut router = router(config);
        let ignored = BTreeSet::from([[1; 16], [2; 16]]);

        let output = router
            .replace_ignored(ignored.clone())
            .expect("bounded ignore policy");
        assert_eq!(router.ignored, ignored);
        assert_eq!(persistence_request_count(&output), 1);

        checkpoint(&mut router);
        let unchanged = router
            .replace_ignored(ignored)
            .expect("unchanged ignore policy");
        assert_eq!(persistence_request_count(&unchanged), 0);

        let oversized = BTreeSet::from([[3; 16], [4; 16], [5; 16]]);
        assert!(matches!(
            router.replace_ignored(oversized),
            Err(RouterError::QueueFull)
        ));
        assert_eq!(router.ignored, BTreeSet::from([[1; 16], [2; 16]]));
    }

    #[test]
    fn replaced_ignore_policy_discards_inbound_before_delivery_state() {
        let mut router = router(RouterConfig::default());
        let mut incoming = message(23);
        incoming.destination_hash = router.node.delivery_destination_hash().into_bytes();
        let source_hash = incoming.source_hash;
        let message_id = incoming.message_id;
        let _ = router
            .replace_ignored(BTreeSet::from([source_hash]))
            .expect("ignore source");
        let mut events = Vec::new();

        router.handle_inbound_message(incoming, 1_700_000_001.0, &mut events);

        assert!(!router.has_message(&message_id));
        assert!(!events
            .iter()
            .any(|event| matches!(event, RouterEvent::MessageReceived(_))));
    }

    #[test]
    fn identity_mismatch_is_rejected_without_partial_restore() {
        let config = RouterConfig::default();
        let mut original = populated_router(config.clone());
        let mut storage = MemoryLxmfStorage::new(128 * 1024);
        original.persist(&mut storage).unwrap();

        let mut target = router(config);
        target.identity_hash = [0xff; 16];
        target.ignored.insert([0xee; 16]);
        let before = target.snapshot().unwrap();
        assert_eq!(target.restore(&storage), Err(RouterError::CorruptSnapshot));
        assert_eq!(target.snapshot().unwrap(), before);
    }

    #[test]
    fn truncated_snapshot_is_rejected_without_partial_restore() {
        let config = RouterConfig::default();
        let mut original = populated_router(config.clone());
        let mut storage = MemoryLxmfStorage::new(128 * 1024);
        original.persist(&mut storage).unwrap();
        let mut state = storage.load(ROUTER_STATE_KEY).unwrap().unwrap();
        state.truncate(state.len() / 2);
        storage.store(ROUTER_STATE_KEY, &state).unwrap();

        let mut target = router(config);
        target.ignored.insert([0xdd; 16]);
        let before = target.snapshot().unwrap();
        assert!(target.restore(&storage).is_err());
        assert_eq!(target.snapshot().unwrap(), before);
    }

    #[test]
    fn restore_rejects_counts_above_configured_capacity() {
        let source_config = RouterConfig::default();
        let mut original = populated_router(source_config);
        let mut storage = MemoryLxmfStorage::new(128 * 1024);
        original.persist(&mut storage).unwrap();

        let bounded_config = RouterConfig {
            max_outbound: 0,
            ..RouterConfig::default()
        };
        let mut target = router(bounded_config);
        assert_eq!(target.restore(&storage), Err(RouterError::CorruptSnapshot));
        assert!(target.outbound.is_empty());
        assert!(target.ignored.is_empty());
    }

    #[test]
    fn stamp_cost_cache_evicts_the_oldest_announce() {
        let mut router = router(RouterConfig {
            max_stamp_costs: 2,
            ..RouterConfig::default()
        });
        router.insert_bounded_stamp_cost([1; 16], (10.0, Some(1), true));
        router.insert_bounded_stamp_cost([2; 16], (20.0, Some(2), true));
        router.insert_bounded_stamp_cost([3; 16], (30.0, Some(3), true));

        assert!(!router.outbound_stamp_costs.contains_key(&[1; 16]));
        assert!(router.outbound_stamp_costs.contains_key(&[2; 16]));
        assert!(router.outbound_stamp_costs.contains_key(&[3; 16]));
    }

    #[test]
    fn delivery_announce_stamp_cost_requests_one_checkpoint() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let identity = identity_from(70);
        let mut destination =
            LxmfNode::delivery_destination(identity).expect("remote delivery destination");
        let app_data = announce::delivery(Some(b"remote"), Some(9));
        let packet = destination
            .announce(Some(&app_data), &mut OsRng, 1_000, 1_000 / 1000)
            .expect("delivery announce");
        let destination_hash = *destination.hash();
        let mut packed = alloc::vec![0; packet.packed_size()];
        let length = packet.pack(&mut packed).expect("pack announce");
        let announce_event = node
            .handle_packet(InterfaceId(0), &packed[..length])
            .events
            .into_iter()
            .find(|event| matches!(event, NodeEvent::AnnounceReceived { .. }))
            .expect("announce event");

        let output = router
            .handle_event(&mut node, &announce_event)
            .expect("handle delivery announce");

        assert_eq!(
            router.outbound_stamp_cost(&node, destination_hash.as_bytes()),
            Some(9)
        );
        assert_eq!(persistence_request_count(&output), 1);
    }

    /// The decoded announce reaches the application. Without this the display
    /// name is read for the stamp cost and then dropped, and every client
    /// re-filters the announce stream and re-runs `DeliveryAnnounce::decode`.
    #[test]
    fn delivery_announce_reports_the_peer_and_its_display_name() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let identity = identity_from(71);
        let mut destination =
            LxmfNode::delivery_destination(identity).expect("remote delivery destination");
        let app_data = announce::delivery(Some(b"Anna"), Some(9));
        let packet = destination
            .announce(Some(&app_data), &mut OsRng, 1_000, 1_000 / 1000)
            .expect("delivery announce");
        let destination_hash = *destination.hash();
        let mut packed = alloc::vec![0; packet.packed_size()];
        let length = packet.pack(&mut packed).expect("pack announce");
        let announce_event = node
            .handle_packet(InterfaceId(0), &packed[..length])
            .events
            .into_iter()
            .find(|event| matches!(event, NodeEvent::AnnounceReceived { .. }))
            .expect("announce event");

        let output = router
            .handle_event(&mut node, &announce_event)
            .expect("handle delivery announce");

        let reported = output
            .events
            .iter()
            .find_map(|event| match event {
                RouterEvent::PeerAnnounced {
                    destination,
                    announce,
                } => Some((*destination, announce.clone())),
                _ => None,
            })
            .expect("the announce is reported to the application");
        assert_eq!(reported.0, destination_hash);
        assert_eq!(reported.1.display_name.as_deref(), Some(&b"Anna"[..]));
        assert_eq!(reported.1.stamp_cost, Some(9));
    }

    /// Only `lxmf.delivery` app data is a `DeliveryAnnounce`. A propagation
    /// node's announce decodes into a peer with a nonsense display name unless
    /// the aspect is checked, and its destination is not a peer at all.
    #[test]
    fn non_delivery_announce_is_not_reported_as_a_peer() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let identity = identity_from(72);
        let mut destination =
            PropagationTransport::destination(identity).expect("propagation destination");
        // Not msgpack: `DeliveryAnnounce::decode` would take the whole blob as
        // a display name.
        let app_data = alloc::vec![0x01, 0x02, 0x03];
        let packet = destination
            .announce(Some(&app_data), &mut OsRng, 1_000, 1_000 / 1000)
            .expect("propagation announce");
        let destination_hash = *destination.hash();
        let mut packed = alloc::vec![0; packet.packed_size()];
        let length = packet.pack(&mut packed).expect("pack announce");
        let announce_event = node
            .handle_packet(InterfaceId(0), &packed[..length])
            .events
            .into_iter()
            .find(|event| matches!(event, NodeEvent::AnnounceReceived { .. }))
            .expect("announce event");

        let output = router
            .handle_event(&mut node, &announce_event)
            .expect("handle propagation announce");

        assert!(
            !output
                .events
                .iter()
                .any(|event| matches!(event, RouterEvent::PeerAnnounced { .. })),
            "a propagation announce is not a peer announce"
        );
        assert!(
            !router
                .outbound_stamp_costs
                .contains_key(destination_hash.as_bytes()),
            "and it carries no delivery stamp cost"
        );
    }

    /// A peer announcing a cost the reference itself would never announce does
    /// not get us mining forever (Codeberg #181, read side).
    ///
    /// The app_data here is hand-built, because our own encoder can no longer
    /// produce it: only a non-conforming or hostile peer emits `0xcc 0xff` in
    /// the stamp-cost slot. Python's `received_announce` (Handlers.py:17-18)
    /// stores that 255 unbounded and `get_stamp` hands it to `generate_stamp`
    /// (LXMessage.py:320), whose loop (LXStamper.py:199) cannot terminate at that
    /// cost. We drop it instead; see `outbound_stamp_cost` for the deviation
    /// argument.
    ///
    /// What this test cannot catch: it proves no stamp is requested, not that
    /// stamping is bounded. A legal cost of 200 would still be mined until the
    /// heat death of the universe — that is a scheduling concern, not a
    /// compatibility one, and deliberately not addressed here.
    #[test]
    fn hostile_announced_stamp_cost_is_not_mined() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let identity = identity_from(73);
        let mut destination =
            LxmfNode::delivery_destination(identity).expect("remote delivery destination");
        // fixarray(3) || bin8 "remote" || uint8 255 || fixarray(1) || 0
        let app_data = alloc::vec![
            0x93, 0xc4, 0x06, b'r', b'e', b'm', b'o', b't', b'e', 0xcc, 0xff, 0x91, 0x00
        ];
        let packet = destination
            .announce(Some(&app_data), &mut OsRng, 1_000, 1_000 / 1000)
            .expect("delivery announce");
        let destination_hash = *destination.hash();
        let mut packed = alloc::vec![0; packet.packed_size()];
        let length = packet.pack(&mut packed).expect("pack announce");
        let announce_event = node
            .handle_packet(InterfaceId(0), &packed[..length])
            .events
            .into_iter()
            .find(|event| matches!(event, NodeEvent::AnnounceReceived { .. }))
            .expect("announce event");
        let _ = router
            .handle_event(&mut node, &announce_event)
            .expect("handle delivery announce");

        // The peer's 255 was decoded and cached, and is then withheld.
        assert_eq!(
            router
                .outbound_stamp_costs
                .get(destination_hash.as_bytes())
                .and_then(|(_, cost, _)| *cost),
            Some(255),
            "the announced value is stored verbatim; only the accessor filters"
        );
        assert_eq!(
            router.outbound_stamp_cost(&node, destination_hash.as_bytes()),
            None
        );

        // End to end: a message queued for that peer never enters stamp work.
        let source = identity_from(74);
        let queued = Message::create(
            destination_hash.into_bytes(),
            [74; 16],
            &source,
            1_700_000_000.25,
            b"title".to_vec(),
            b"content".to_vec(),
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .expect("message");
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        assert_eq!(router.outbound_stamp_request(&node, &id), None);
        let output = router
            .tick(&mut node)
            .expect("tick with a hostile cost cached");
        assert!(
            !output
                .events
                .iter()
                .any(|event| matches!(event, RouterEvent::StampPending(_))),
            "no stamp work may be requested for an unannounceable cost"
        );

        // The pin bites: the same announce one bit lower is mined normally.
        assert_eq!(
            router.outbound_stamp_costs.insert(
                destination_hash.into_bytes(),
                (1_700_000_000.0, Some(254), true)
            ),
            Some((1_700_000_000.0, Some(255), true))
        );
        assert_eq!(
            router.outbound_stamp_cost(&node, destination_hash.as_bytes()),
            Some(254)
        );
    }

    #[test]
    fn unknown_peer_retry_uses_delivery_retry_wait() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let queued = message(71);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        checkpoint(&mut router);

        let output = router.tick(&mut node).expect("tick");
        let entry = router.outbound.get(&id).expect("retry remains queued");
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.next_attempt_ms, 1_000 + DELIVERY_RETRY_WAIT_MS);
        assert_eq!(persistence_request_count(&output), 1);
    }

    #[test]
    fn direct_delivery_uses_shared_attempt_limit() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let queued = message(72);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        let entry = router.outbound.get_mut(&id).expect("queued direct message");
        entry.attempts = MAX_DELIVERY_ATTEMPTS;
        entry.next_attempt_ms = 1_000;

        let output = router.tick(&mut node).expect("tick");

        assert!(!router.outbound.contains_key(&id));
        assert!(output.events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageState { message_id, state: MessageState::Failed }
                if *message_id == id
        )));
    }

    #[test]
    fn active_direct_delivery_does_not_consume_another_attempt() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let queued = message(83);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        let entry = router.outbound.get_mut(&id).expect("queued direct message");
        entry.attempts = MAX_DELIVERY_ATTEMPTS;
        entry.set_state(MessageState::Sending, &mut router.build_epochs);
        entry.next_attempt_ms = 1_000;

        let output = router.tick(&mut node).expect("tick");
        let entry = router
            .outbound
            .get(&id)
            .expect("active direct delivery remains tracked");

        assert_eq!(entry.attempts, MAX_DELIVERY_ATTEMPTS);
        assert_eq!(entry.state(), MessageState::Sending);
        assert_eq!(
            entry.next_attempt_ms,
            1_000_u64.saturating_add(PROCESSING_INTERVAL_MS)
        );
        assert!(!output.events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageState {
                message_id,
                state: MessageState::Failed
            } if *message_id == id
        )));
    }

    #[test]
    fn direct_payload_submission_stays_within_the_link_attempt() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let queued = message(85);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        let entry = router.outbound.get_mut(&id).expect("queued direct message");
        entry.attempts = 1;

        record_successful_submission_attempt(entry, &Ok(DeliveryRepresentation::DirectPacket));
        assert_eq!(entry.attempts, 1);

        record_successful_submission_attempt(entry, &Ok(DeliveryRepresentation::DirectResource));
        assert_eq!(entry.attempts, 1);

        record_successful_submission_attempt(
            entry,
            &Ok(DeliveryRepresentation::OpportunisticPacket),
        );
        assert_eq!(entry.attempts, 2);
    }

    #[test]
    fn oversized_opportunistic_fallback_uses_shared_attempt_limit() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let queued = oversized_opportunistic_message(84);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        let entry = router
            .outbound
            .get_mut(&id)
            .expect("queued direct fallback");
        entry.attempts = MAX_DELIVERY_ATTEMPTS;
        entry.next_attempt_ms = 1_000;

        let output = router.tick(&mut node).expect("tick");

        assert!(!router.outbound.contains_key(&id));
        assert!(output.events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageState {
                message_id,
                state: MessageState::Failed
            } if *message_id == id
        )));
    }

    #[test]
    fn active_oversized_opportunistic_fallback_is_not_resubmitted() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let queued = oversized_opportunistic_message(85);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        let entry = router
            .outbound
            .get_mut(&id)
            .expect("queued direct fallback");
        entry.attempts = MAX_DELIVERY_ATTEMPTS;
        entry.set_state(MessageState::Sending, &mut router.build_epochs);
        entry.next_attempt_ms = 1_000;

        let output = router.tick(&mut node).expect("tick");
        let entry = router
            .outbound
            .get(&id)
            .expect("active direct fallback remains tracked");

        assert_eq!(entry.attempts, MAX_DELIVERY_ATTEMPTS);
        assert_eq!(entry.state(), MessageState::Sending);
        assert_eq!(
            entry.next_attempt_ms,
            1_000_u64.saturating_add(PROCESSING_INTERVAL_MS)
        );
        assert!(!output.events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageState {
                message_id,
                state: MessageState::Failed
            } if *message_id == id
        )));
    }

    #[test]
    fn cancelling_an_outbound_message_removes_it_and_emits_cancelled() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let queued = message(82);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();

        let output = router
            .cancel(&mut node, &id)
            .expect("cancel queued message");

        assert!(!router.outbound.contains_key(&id));
        assert!(output.events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageState { message_id, state: MessageState::Cancelled }
                if *message_id == id
        )));
    }

    #[test]
    fn opportunistic_unknown_path_is_requested_before_first_attempt() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let mut queued = message(73);
        queued.method = DeliveryMethod::Opportunistic;
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();

        let output = router.tick(&mut node).expect("tick");
        let entry = router.outbound.get(&id).expect("message remains queued");
        assert_eq!(entry.attempts, 0);
        assert!(router.preemptive_path_requests.contains(&id));
        assert_eq!(entry.next_attempt_ms, 1_000 + PATH_REQUEST_WAIT_MS);
        assert!(output
            .core
            .actions
            .iter()
            .any(|action| matches!(action, Action::Broadcast { .. })));
    }

    #[test]
    fn opportunistic_submission_matches_python_sent_state() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let mut queued = message(79);
        queued.method = DeliveryMethod::Opportunistic;
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        let mut events = Vec::new();

        router.handle_node_event(
            LxmfNodeEvent::Submitted {
                message_id: id,
                method: DeliveryMethod::Opportunistic,
                representation: DeliveryRepresentation::OpportunisticPacket,
                submission: crate::node::SubmissionId::Packet([0; 16]),
            },
            1_000,
            1_700_000_001.0,
            &mut events,
        );

        assert_eq!(router.outbound[&id].state(), MessageState::Sent);
        assert!(events.is_empty());
    }

    #[test]
    fn observing_pending_direct_link_does_not_consume_an_attempt() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let destination = announced_peer(&mut router, &mut node, 74);
        let mut queued = message(75);
        queued.destination_hash = destination.into_bytes();
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();

        let _ = router.tick(&mut node).expect("start link");
        let entry = router.outbound.get(&id).expect("message remains queued");
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.next_attempt_ms, 1_000 + DELIVERY_RETRY_WAIT_MS);

        router.next_job_ms = 1_000;
        router.outbound.get_mut(&id).unwrap().next_attempt_ms = 1_000;
        let _ = router.tick(&mut node).expect("observe pending link");
        let entry = router.outbound.get(&id).expect("message remains queued");
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.next_attempt_ms, 1_000 + PROCESSING_INTERVAL_MS);
    }

    #[test]
    fn direct_link_establishment_wakes_waiting_message_without_an_attempt() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let destination = DestinationHash::new([0x76; 16]);
        let mut queued = message(76);
        queued.destination_hash = destination.into_bytes();
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        let entry = router.outbound.get_mut(&id).unwrap();
        entry.attempts = 1;
        entry.next_attempt_ms = 1_000 + DELIVERY_RETRY_WAIT_MS;
        router.next_job_ms = 1_000 + PROCESSING_INTERVAL_MS;
        checkpoint(&mut router);

        let mut events = Vec::new();
        router.handle_node_event(
            LxmfNodeEvent::DirectLinkEstablished {
                destination: Some(destination),
                link_id: LinkId::new([0x77; 16]),
                is_initiator: true,
            },
            1_250,
            1_700_000_001.0,
            &mut events,
        );

        let entry = router.outbound.get(&id).unwrap();
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.next_attempt_ms, 1_250);
        assert_eq!(router.next_job_ms, 1_250);
        assert!(router.persistence_dirty);
    }

    #[test]
    fn direct_unknown_path_counts_one_request_attempt() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let destination = announced_peer(&mut router, &mut node, 76);
        assert!(node.remove_path(destination.as_bytes()));
        let mut queued = message(77);
        queued.destination_hash = destination.into_bytes();
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();

        let output = router.tick(&mut node).expect("tick");
        let entry = router.outbound.get(&id).expect("message remains queued");
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.next_attempt_ms, 1_000 + PATH_REQUEST_WAIT_MS);
        assert!(output
            .core
            .actions
            .iter()
            .any(|action| matches!(action, Action::Broadcast { .. })));
    }

    #[test]
    fn closed_direct_link_requests_path_only_for_a_waiting_message() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        let destination = DestinationHash::new([0x78; 16]);
        let mut queued = message(78);
        queued.destination_hash = destination.into_bytes();
        let _ = router.enqueue(&node, queued).unwrap();
        let event = NodeEvent::LinkClosed {
            link_id: LinkId::new([0x79; 16]),
            reason: leviculum_core::LinkCloseReason::PeerClosed,
            is_initiator: true,
            destination_hash: destination,
        };

        let output = router
            .handle_event(&mut node, &event)
            .expect("handle direct link close");
        assert!(output
            .core
            .actions
            .iter()
            .any(|action| matches!(action, Action::Broadcast { .. })));

        router.outbound.clear();
        let output = router
            .handle_event(&mut node, &event)
            .expect("handle idle link close");
        assert!(!output
            .core
            .actions
            .iter()
            .any(|action| matches!(action, Action::Broadcast { .. })));
    }

    #[test]
    fn idle_scheduler_tick_does_not_request_a_checkpoint() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());

        let output = router.tick(&mut node).expect("tick");

        assert_eq!(router.next_job_ms, 1_000 + PROCESSING_INTERVAL_MS);
        assert_eq!(persistence_request_count(&output), 0);
    }

    #[test]
    fn delivery_failure_and_progress_each_request_a_checkpoint() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let queued = message(72);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        checkpoint(&mut router);
        let entry = router.outbound.get_mut(&id).unwrap();
        entry.set_state(MessageState::Sending, &mut router.build_epochs);

        let mut failure_events = Vec::new();
        router.handle_node_event(
            LxmfNodeEvent::DeliveryFailed {
                message_id: id,
                reason: DeliveryFailure::DirectPacketTimeout,
            },
            node.now_ms(),
            1_700_000_001.0,
            &mut failure_events,
        );
        let failure = router.finish_output(RouterOutput {
            core: TickOutput::default(),
            events: failure_events,
        });
        let entry = router.outbound.get(&id).unwrap();
        assert_eq!(entry.state(), MessageState::Outbound);
        assert_eq!(
            entry.next_attempt_ms,
            node.now_ms() + DELIVERY_RETRY_WAIT_MS
        );
        assert_eq!(persistence_request_count(&failure), 1);
        checkpoint(&mut router);

        let mut progress_events = Vec::new();
        router.handle_node_event(
            LxmfNodeEvent::Progress {
                message_id: id,
                progress: 0.75,
            },
            5_000,
            1_700_000_002.0,
            &mut progress_events,
        );
        let progress = router.finish_output(RouterOutput {
            core: TickOutput::default(),
            events: progress_events,
        });
        assert_eq!(router.outbound.get(&id).unwrap().progress, 0.75);
        assert_eq!(persistence_request_count(&progress), 1);
    }

    #[test]
    fn receiver_cancelled_resource_is_terminally_rejected() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let queued = message(86);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();
        checkpoint(&mut router);
        let entry = router.outbound.get_mut(&id).unwrap();
        entry.set_state(MessageState::Sending, &mut router.build_epochs);

        let mut events = Vec::new();
        router.handle_node_event(
            LxmfNodeEvent::DeliveryFailed {
                message_id: id,
                reason: DeliveryFailure::Resource(ResourceError::Cancelled),
            },
            5_000,
            1_700_000_001.0,
            &mut events,
        );

        assert!(!router.outbound.contains_key(&id));
        assert!(router.persistence_dirty);
        assert_eq!(
            events,
            vec![RouterEvent::MessageState {
                message_id: id,
                state: MessageState::Rejected,
            }]
        );
    }

    #[test]
    fn cleanup_removals_request_one_checkpoint() {
        let mut router = router(RouterConfig::default());
        router.insert_bounded_stamp_cost([1; 16], (0.0, Some(4), true));
        router.insert_bounded_id([2; 32], 0.0, true);
        router.insert_bounded_id([3; 32], 0.0, false);
        assert!(router.tickets_mut().remember(
            [4; 16],
            Ticket {
                expires_unix: 10.0,
                secret: [5; 16],
            },
            0.0,
        ));
        checkpoint(&mut router);

        let now_unix = MESSAGE_EXPIRY_SECS * 7.0 + STAMP_COST_EXPIRY as f64 + 100.0;
        router.clean(now_unix);
        let output = router.finish_output(RouterOutput::default());

        assert!(router.outbound_stamp_costs.is_empty());
        assert!(router.delivered_ids.is_empty());
        assert!(router.processed_ids.is_empty());
        assert!(router.tickets.outbound(&[4; 16], now_unix).is_none());
        assert_eq!(persistence_request_count(&output), 1);
    }

    #[test]
    fn failed_checkpoint_remains_dirty_until_a_successful_retry() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let _ = router.enqueue(&node, message(73)).unwrap();
        let mut full = MemoryLxmfStorage::new(0);
        assert_eq!(
            router.persist(&mut full),
            Err(RouterError::Storage(StorageError::Full))
        );
        assert_eq!(
            persistence_request_count(&router.finish_output(RouterOutput::default())),
            1
        );

        checkpoint(&mut router);
        assert_eq!(
            persistence_request_count(&router.finish_output(RouterOutput::default())),
            0
        );
    }

    #[test]
    fn policy_mutations_are_flushed_by_the_next_router_output() {
        let mut router = router(RouterConfig::default());
        let source = [0x44; 16];
        router.ignore(source);
        assert_eq!(
            persistence_request_count(&router.finish_output(RouterOutput::default())),
            1
        );
        checkpoint(&mut router);

        router.unignore(&source);
        assert_eq!(
            persistence_request_count(&router.finish_output(RouterOutput::default())),
            1
        );
    }

    #[test]
    fn propagated_requires_a_selected_client_node_and_paper_uses_its_own_api() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let mut propagated = message(41);
        propagated.method = DeliveryMethod::Propagated;
        assert!(matches!(
            router.enqueue(&node, propagated),
            Err(RouterError::PropagationNodeUnavailable)
        ));

        let mut paper = message(42);
        paper.method = DeliveryMethod::Paper;
        assert!(matches!(
            router.enqueue(&node, paper),
            Err(RouterError::UnsupportedMethod)
        ));
    }

    #[test]
    fn queued_outbound_accepts_both_stamp_lengths_and_becomes_due() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        router.next_job_ms = 90_000;

        for (seed, length, now_ms) in [(43, 16, 1_200), (44, 32, 1_100)] {
            let queued = message(seed);
            let id = queued.message_id;
            let _ = router.enqueue(&node, queued).unwrap();

            let output = router
                .set_outbound_stamp(&id, vec![seed; length], now_ms)
                .expect("attach queued stamp");
            let entry = router.outbound.get(&id).expect("queued entry");
            assert_eq!(
                entry.message().stamp.as_deref(),
                Some(&vec![seed; length][..])
            );
            assert_eq!(entry.next_attempt_ms, now_ms);
            assert_eq!(output.events, vec![RouterEvent::PersistenceRequested]);
            assert_eq!(output.core.next_deadline_ms, Some(now_ms));
        }
        assert_eq!(router.next_job_ms, 1_100);
    }

    #[test]
    fn invalid_queued_stamp_is_rejected_without_mutating_entry() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let queued = message(45);
        let id = queued.message_id;
        let _ = router.enqueue(&node, queued).unwrap();

        assert!(matches!(
            router.set_outbound_stamp(&id, vec![0xaa; 31], 1_000),
            Err(RouterError::Message(MessageError::InvalidFormat))
        ));
        let entry = router.outbound.get(&id).expect("queued entry");
        assert!(entry.message().stamp.is_none());
        assert_eq!(entry.next_attempt_ms, node.now_ms());
    }

    #[test]
    fn detached_delivery_stamp_result_rejects_a_changed_cost() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let queued = message(46);
        let message_id = queued.message_id;
        let destination = queued.destination_hash;
        router.insert_bounded_stamp_cost(destination, (1_700_000_000.0, Some(8), false));
        let _ = router.enqueue(&node, queued).expect("queue message");
        let stale = router
            .outbound_stamp_request(&node, &message_id)
            .expect("detached request");

        router.insert_bounded_stamp_cost(destination, (1_700_000_002.0, Some(9), false));
        assert!(matches!(
            router.set_outbound_stamp_result(&node, &stale, vec![0x46; STAMP_SIZE]),
            Err(RouterError::StaleStampRequest)
        ));
        assert!(router.outbound[&message_id].message().stamp.is_none());

        let current = router
            .outbound_stamp_request(&node, &message_id)
            .expect("updated request");
        let _ = router
            .set_outbound_stamp_result(&node, &current, vec![0x47; STAMP_SIZE])
            .expect("attach current result");
        assert_eq!(
            router.outbound[&message_id].message().stamp.as_deref(),
            Some(&[0x47; STAMP_SIZE][..])
        );
    }

    #[test]
    fn detached_propagation_stamp_result_rejects_changed_material() {
        let mut router = router(RouterConfig::default());
        let mut outbound = message(47);
        outbound.method = DeliveryMethod::Propagated;
        let message_id = outbound.message_id;
        let mut unstamped_lxmf = outbound.destination_hash.to_vec();
        unstamped_lxmf.extend_from_slice(&[0x91; 96]);
        let transient_id = full_hash(&unstamped_lxmf);
        let entry = OutboundEntry::new(
            outbound,
            MessageState::Outbound,
            0,
            0,
            0.01,
            Some(OutboundPropagation {
                timebase: 1_700_000_000.0,
                unstamped_lxmf,
                transient_id,
                target_cost: Some(8),
                stamp: None,
            }),
            &mut router.build_epochs,
        );
        router.outbound.insert(message_id, entry);
        let stale = router
            .outbound_propagation_stamp_request(&message_id)
            .expect("detached request");
        let entry = router.outbound.get_mut(&message_id).unwrap();
        entry.retarget_propagation(Some(9), &mut router.build_epochs);

        assert!(matches!(
            router.set_outbound_propagation_stamp_result(&stale, [0x92; STAMP_SIZE], 1_000),
            Err(RouterError::StaleStampRequest)
        ));
        let current = router
            .outbound_propagation_stamp_request(&message_id)
            .expect("updated request");
        let _ = router
            .set_outbound_propagation_stamp_result(&current, [0x93; STAMP_SIZE], 1_001)
            .expect("attach current result");
        assert_eq!(
            router.outbound[&message_id]
                .propagation()
                .and_then(|prepared| prepared.stamp),
            Some([0x93; STAMP_SIZE])
        );
    }

    #[cfg(feature = "pow")]
    #[test]
    fn detached_inbound_validation_retains_message_and_rejects_stale_cost() {
        let mut router = router(RouterConfig {
            enforce_stamps: true,
            inbound_stamp_cost: Some(8),
            ..RouterConfig::default()
        });
        let remote = identity_from(48);
        let mut incoming = Message::create(
            router.node.delivery_destination_hash().into_bytes(),
            [0x48; 16],
            &remote,
            1_700_000_000.0,
            b"detached validation".to_vec(),
            Vec::new(),
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .expect("incoming message");
        incoming
            .set_stamp(vec![0x94; STAMP_SIZE])
            .expect("attach candidate stamp");
        let message_id = incoming.message_id;
        let mut events = Vec::new();
        router.handle_inbound_message(incoming, 1_700_000_001.0, &mut events);
        let request = events
            .iter()
            .find_map(|event| match event {
                RouterEvent::InboundStampPending(request) => Some(*request),
                _ => None,
            })
            .expect("detached validation request");

        router.config.inbound_stamp_cost = Some(9);
        assert!(matches!(
            router.set_inbound_stamp_result(&request, true),
            Err(RouterError::StaleStampRequest)
        ));
        assert_eq!(router.pending_inbound_stamps.len(), 1);

        router.config.inbound_stamp_cost = Some(8);
        let output = router
            .set_inbound_stamp_result(&request, true)
            .expect("apply current validation");
        assert!(output.events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageReceived(message) if message.message_id == message_id
        )));
        assert!(router.pending_inbound_stamps.is_empty());
    }

    #[test]
    fn issued_ticket_stamp_is_accepted_synchronously_even_without_pow() {
        let (mut router, node) = router_and_node(RouterConfig {
            enforce_stamps: true,
            inbound_stamp_cost: Some(20),
            ..RouterConfig::default()
        });
        let remote_hash = [0x61; 16];
        let (field, output) = router
            .issue_ticket_field(&node, remote_hash, &mut OsRng)
            .expect("issue reply ticket");
        assert_eq!(persistence_request_count(&output), 1);
        let field = field.expect("ticket is due");
        assert_eq!(field.0, FIELD_TICKET);
        let ticket = Ticket::from_field_value(&field.1).expect("ticket field value");

        let remote = identity_from(90);
        let mut incoming = Message::create(
            router.node.delivery_destination_hash().into_bytes(),
            remote_hash,
            &remote,
            1_700_000_001.0,
            b"ticket reply".to_vec(),
            b"no recipient PoW".to_vec(),
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .expect("signed incoming message");
        incoming
            .set_stamp(crate::stamp::ticket_stamp(&ticket.secret, &incoming.message_id).to_vec())
            .expect("attach ticket-derived stamp");
        let message_id = incoming.message_id;
        let mut events = Vec::new();

        router.handle_inbound_message(incoming, 1_700_000_001.0, &mut events);

        assert!(events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageReceived(message) if message.message_id == message_id
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            RouterEvent::StampPending(_) | RouterEvent::InvalidStamp(_)
        )));
    }

    #[test]
    fn remembered_ticket_stamps_propagated_outbound_before_encryption() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        enable_propagation_client(&mut router, &mut node);
        let _ = router
            .set_outbound_propagation_node(&mut node, Some(DestinationHash::new([0x71; 16])))
            .expect("select propagation node");

        let mut outbound = message(91);
        outbound.method = DeliveryMethod::Propagated;
        let ticket = Ticket {
            expires_unix: 1_700_100_000.0,
            secret: [0x72; 16],
        };
        assert!(router.tickets.remember(
            outbound.destination_hash,
            ticket.clone(),
            1_700_000_000.0,
        ));
        let message_id = outbound.message_id;

        let _ = router
            .enqueue(&node, outbound)
            .expect("queue ticket-stamped propagation message");

        let entry = router.outbound.get(&message_id).expect("queued entry");
        assert_eq!(
            entry.message().stamp.as_deref(),
            Some(&crate::stamp::ticket_stamp(&ticket.secret, &message_id)[..])
        );
        assert_eq!(entry.message().stamp.as_ref().map(Vec::len), Some(16));
        assert!(
            entry.propagation().is_none(),
            "outer stamp remains separate"
        );
    }

    #[test]
    fn restore_preserves_prepared_propagation_but_rebases_volatile_queue_state() {
        let config = RouterConfig::default();
        let mut original = router(config.clone());
        original.next_job_ms = 99_000;
        let mut outbound = message(92);
        outbound.method = DeliveryMethod::Propagated;
        let message_id = outbound.message_id;
        let mut unstamped_lxmf = outbound.destination_hash.to_vec();
        unstamped_lxmf.extend_from_slice(&[0x81; 128]);
        let propagation = OutboundPropagation {
            timebase: 1_700_000_005.5,
            transient_id: full_hash(&unstamped_lxmf),
            unstamped_lxmf,
            target_cost: Some(11),
            stamp: Some([0x82; STAMP_SIZE]),
        };
        let entry = OutboundEntry::new(
            outbound,
            MessageState::Sending,
            2,
            12_000,
            0.4,
            Some(propagation.clone()),
            &mut original.build_epochs,
        );
        original.outbound.insert(message_id, entry);
        let mut storage = MemoryLxmfStorage::new(128 * 1024);

        original.persist(&mut storage).expect("persist v4 snapshot");
        let mut restored = router(config);
        restored.restore(&storage).expect("restore v4 snapshot");

        assert_eq!(restored.next_job_ms, 0);
        let restored_entry = restored.outbound.get(&message_id).expect("restored entry");
        assert_eq!(restored_entry.state(), MessageState::Outbound);
        assert_eq!(restored_entry.attempts, 2);
        assert_eq!(restored_entry.next_attempt_ms, 0);
        assert_eq!(restored_entry.progress, 0.01);
        assert_eq!(restored_entry.propagation(), Some(&propagation));
        assert_eq!(
            restored_entry.message().pack(),
            original.outbound[&message_id].message().pack()
        );
    }

    #[test]
    fn version_three_snapshot_restores_without_propagation_preparation() {
        let mut source = router(RouterConfig::default());
        let outbound = message(93);
        let message_id = outbound.message_id;
        let mut snapshot = Vec::new();
        msgpack::array(&mut snapshot, SNAPSHOT_FIELDS);
        msgpack::uint(&mut snapshot, 3);
        msgpack::bin(&mut snapshot, &source.identity_hash);
        msgpack::array(&mut snapshot, 1);
        msgpack::array(&mut snapshot, 7);
        msgpack::bin(&mut snapshot, &outbound.pack());
        msgpack::uint(&mut snapshot, DeliveryMethod::Direct as u64);
        msgpack::uint(&mut snapshot, MessageState::Sending as u64);
        msgpack::uint(&mut snapshot, verification_value(outbound.verification));
        msgpack::uint(&mut snapshot, 3);
        msgpack::uint(&mut snapshot, 80_000);
        msgpack::f64(&mut snapshot, 0.75);
        encode_id_times(&mut snapshot, &BTreeMap::new());
        encode_id_times(&mut snapshot, &BTreeMap::new());
        encode_stamp_costs(&mut snapshot, &BTreeMap::new());
        encode_tickets(&mut snapshot, &TicketStore::default());
        msgpack::array(&mut snapshot, 0);
        msgpack::uint(&mut snapshot, 90_000);
        let mut storage = MemoryLxmfStorage::new(128 * 1024);
        storage
            .store(ROUTER_STATE_KEY, &snapshot)
            .expect("store v3 snapshot");

        source.restore(&storage).expect("restore v3 snapshot");

        let restored = source.outbound.get(&message_id).expect("restored message");
        assert!(restored.propagation().is_none());
        assert_eq!(restored.message().pack(), outbound.pack());
        assert_eq!(restored.state(), MessageState::Outbound);
        assert_eq!(restored.attempts, 3);
        assert_eq!(restored.next_attempt_ms, 0);
        assert_eq!(restored.progress, 0.01);
        assert_eq!(source.next_job_ms, 0);
    }

    #[test]
    fn switching_propagation_nodes_keeps_ciphertext_but_clears_outer_stamp() {
        let (mut router, mut node) = router_and_node(RouterConfig::default());
        enable_propagation_client(&mut router, &mut node);
        let _ = router
            .set_outbound_propagation_node(&mut node, Some(DestinationHash::new([0x91; 16])))
            .expect("select first propagation node");
        let mut outbound = message(94);
        outbound.method = DeliveryMethod::Propagated;
        let message_id = outbound.message_id;
        let _ = router
            .enqueue(&node, outbound)
            .expect("queue propagated message");
        let mut unstamped_lxmf = router.outbound[&message_id]
            .message()
            .destination_hash
            .to_vec();
        unstamped_lxmf.extend_from_slice(&[0xa1; 128]);
        let prepared = OutboundPropagation {
            timebase: 1_700_000_000.0,
            transient_id: full_hash(&unstamped_lxmf),
            unstamped_lxmf,
            target_cost: Some(8),
            stamp: Some([0xa2; STAMP_SIZE]),
        };
        let entry = router.outbound.get_mut(&message_id).unwrap();
        entry.set_propagation(Some(prepared.clone()), &mut router.build_epochs);

        let output = router
            .set_outbound_propagation_node(&mut node, Some(DestinationHash::new([0x92; 16])))
            .expect("switch propagation node");

        assert_eq!(persistence_request_count(&output), 1);
        let entry = router.outbound.get(&message_id).expect("queued entry");
        let after = entry.propagation().expect("ciphertext retained");
        assert_eq!(after.timebase, prepared.timebase);
        assert_eq!(after.unstamped_lxmf, prepared.unstamped_lxmf);
        assert_eq!(after.transient_id, prepared.transient_id);
        assert_eq!(after.target_cost, None);
        assert_eq!(after.stamp, None);
        assert_eq!(entry.state(), MessageState::Outbound);
        assert_eq!(entry.next_attempt_ms, node.now_ms());
    }

    /// A message a Python peer would refuse at the Resource advertisement is
    /// refused at enqueue with the distinguishable error, and nothing is
    /// queued: no `MessageQueued`, no outbound entry, no core actions.
    #[test]
    fn enqueue_refuses_message_over_delivery_limit() {
        let (mut router, node) = router_and_node(RouterConfig::default());
        let source = identity_from(7);
        let over_limit = Message::create(
            [0x21; 16],
            [0x22; 16],
            &source,
            1_700_000_000.25,
            b"too big".to_vec(),
            alloc::vec![0x5a; 1_000_001],
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .expect("message");
        let size = over_limit.pack().len() as u64;
        assert!(size > 1_000_000);

        let refused = router.enqueue(&node, over_limit);
        match refused {
            Err(RouterError::Node(LxmfNodeError::DeliveryLimitExceeded {
                size: reported,
                limit,
            })) => {
                assert_eq!(reported, size);
                assert_eq!(limit, 1_000_000);
            }
            other => panic!("expected DeliveryLimitExceeded, got {other:?}"),
        }
        assert!(router.outbound.is_empty(), "nothing may be queued");
    }
}
