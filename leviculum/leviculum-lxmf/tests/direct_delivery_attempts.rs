//! Direct-delivery attempt accounting and Resource-conclusion handling,
//! pinned against the LXMF/RNS reference.
//!
//! These pins were written for Codeberg PR #179 (nilu96) and driven red on
//! master before the contribution was integrated. They use only the public
//! crate API, so they observe the same surface an application does.
//!
//! Reference citations:
//!
//! * `reference/LXMF/LXMF/LXMRouter.py:2820-2839` — in the `DIRECT` branch,
//!   `delivery_attempts += 1` happens once, in the "no link exists" arm, when
//!   the router either creates the `RNS.Link` (2831) or requests the path
//!   (2837). Submitting over an already-active link (2784-2791) does not
//!   touch the counter.
//! * `reference/LXMF/LXMF/LXMessage.py:597-606` — a Resource that concludes
//!   non-`COMPLETE` tears its link down and returns the message to `OUTBOUND`
//!   (retryable), *except* when the Resource status is `REJECTED`, which sets
//!   the message state to `REJECTED` and leaves the link alone.
//! * `reference/Reticulum/RNS/Link.py:1143-1150` and
//!   `reference/Reticulum/RNS/Resource.py:1106-1110` — a receiver cancel
//!   (`RESOURCE_RCL`) on the sending side is exactly what produces
//!   `Resource.REJECTED`. Our equivalent is `NodeEvent::ResourceFailed` with
//!   `is_sender: true` and `ResourceError::Cancelled`
//!   (`leviculum-core/src/node/link_management.rs:2805-2816`).

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;

use leviculum_core::{
    resource::ResourceError, Action, Clock, DestinationHash, Identity, InterfaceId, LinkId,
    MemoryStorage, NodeCore, NodeCoreBuilder, NodeEvent, TickOutput,
};
use leviculum_lxmf::{
    announce,
    node::{DeliveryRepresentation, LxmfNode, LxmfNodeConfig, LxmfNodeError, LxmfNodeEvent},
    router::{LxmfRouter, MessageState, RouterConfig, RouterError, RouterEvent, RouterOutput},
    storage::MemoryLxmfStorage,
    DeliveryMethod, Message,
};
use rand_core::OsRng;

const NOW_UNIX: f64 = 1_700_000_000.0;

#[derive(Clone)]
struct TestClock(Rc<Cell<u64>>);

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
    /// The router derives every wall-clock field from `Transport::emission_secs`
    /// (Codeberg #182), so wall time is injected through the platform clock.
    fn wall_unix_secs(&self) -> Option<u64> {
        Some(NOW_UNIX as u64)
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

fn test_node(clock: TestClock) -> TestNode {
    NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults())
}

fn take_packets(actions: Vec<Action>) -> Vec<Vec<u8>> {
    actions
        .into_iter()
        .map(|action| match action {
            Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
        })
        .collect()
}

/// A sender: `NodeCore` driven through a full `LxmfRouter`.
struct Sender {
    node: TestNode,
    router: LxmfRouter,
    clock: Rc<Cell<u64>>,
    destination: DestinationHash,
    signing_identity: Identity,
    events: Vec<RouterEvent>,
}

fn sender(seed: u8) -> Sender {
    sender_with(seed, RouterConfig::default())
}

fn sender_with(seed: u8, config: RouterConfig) -> Sender {
    let clock = Rc::new(Cell::new(1_000));
    let mut node = test_node(TestClock(Rc::clone(&clock)));
    let identity = identity_from(seed);
    let private = identity.private_key_bytes().expect("private delivery key");
    let identity_hash = *identity.hash();
    let destination = LxmfNode::delivery_destination(identity).expect("delivery destination");
    let destination_hash = *destination.hash();
    let lxmf = LxmfNode::register(&mut node, destination, LxmfNodeConfig::default())
        .expect("register delivery destination");
    Sender {
        node,
        router: LxmfRouter::new(lxmf, identity_hash, config),
        clock,
        destination: destination_hash,
        signing_identity: Identity::from_private_key_bytes(&private)
            .expect("signing identity copy"),
        events: Vec::new(),
    }
}

impl Sender {
    fn absorb_core(&mut self, core: TickOutput) -> Vec<Vec<u8>> {
        let mut actions = core.actions;
        let mut events: VecDeque<NodeEvent> = core.events.into();
        while let Some(event) = events.pop_front() {
            let follow_up = self
                .router
                .handle_event(&mut self.node, &event)
                .expect("router handles NodeCore event");
            self.events.extend(follow_up.events);
            actions.extend(follow_up.core.actions);
            events.extend(follow_up.core.events);
        }
        take_packets(actions)
    }

    fn absorb_router(&mut self, output: RouterOutput) -> Vec<Vec<u8>> {
        self.events.extend(output.events);
        self.absorb_core(output.core)
    }

    fn receive(&mut self, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut outbound = Vec::new();
        for packet in packets {
            let core = self.node.handle_packet(InterfaceId(0), &packet);
            outbound.extend(self.absorb_core(core));
        }
        outbound
    }

    fn advance_ms(&mut self, delta: u64) {
        self.clock.set(self.clock.get() + delta);
    }

    fn tick(&mut self) -> Vec<Vec<u8>> {
        let output = self.router.tick(&mut self.node).expect("tick");
        self.absorb_router(output)
    }

    fn attempts(&self, id: &[u8; 32]) -> u8 {
        self.router
            .outbound()
            .get(id)
            .expect("message still queued")
            .attempts
    }

    fn state(&self, id: &[u8; 32]) -> Option<MessageState> {
        self.router.outbound().get(id).map(|entry| entry.state())
    }

    /// How many times the router has told the caller a build is waiting for
    /// this message. `ResourceBuildPending` is emitted nowhere else, so a
    /// change in this count is proof that the deferral path ran, and it does
    /// not depend on the entry state the deferral leaves behind.
    fn builds_pending(&self, id: &[u8; 32]) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, RouterEvent::ResourceBuildPending(m) if m == id))
            .count()
    }
}

/// A receiver: `NodeCore` with a plain registered LXMF delivery destination.
struct Receiver {
    node: TestNode,
    lxmf: LxmfNode,
    destination: DestinationHash,
    /// Hashes of the transfers this receiver has started accepting. NodeCore
    /// only emits `ResourceTransferStarted` on the receiving side, so this is
    /// where the sender's in-flight Resource hash is observable.
    accepted_resources: Vec<[u8; 32]>,
    /// The messages that actually arrived. A transfer hash says a Resource
    /// crossed; only the body says *which* bytes crossed, which is what a
    /// stale build is about.
    delivered: Vec<Message>,
}

fn receiver(seed: u8) -> Receiver {
    let clock = Rc::new(Cell::new(1_000));
    let mut node = test_node(TestClock(clock));
    let identity = identity_from(seed);
    let destination = LxmfNode::delivery_destination(identity).expect("delivery destination");
    let destination_hash = *destination.hash();
    let lxmf = LxmfNode::register(&mut node, destination, LxmfNodeConfig::default())
        .expect("register delivery destination");
    Receiver {
        node,
        lxmf,
        destination: destination_hash,
        accepted_resources: Vec::new(),
        delivered: Vec::new(),
    }
}

impl Receiver {
    fn absorb_core(&mut self, core: TickOutput) -> Vec<Vec<u8>> {
        let mut actions = core.actions;
        let mut events: VecDeque<NodeEvent> = core.events.into();
        while let Some(event) = events.pop_front() {
            if let NodeEvent::ResourceTransferStarted {
                resource_hash,
                is_sender: false,
                ..
            } = &event
            {
                self.accepted_resources.push(*resource_hash);
            }
            let follow_up = self
                .lxmf
                .handle_event(&mut self.node, &event)
                .expect("receiver handles NodeCore event");
            for lxmf_event in &follow_up.events {
                if let LxmfNodeEvent::MessageReceived(message) = lxmf_event {
                    self.delivered.push(message.clone());
                }
            }
            actions.extend(follow_up.core.actions);
            events.extend(follow_up.core.events);
        }
        take_packets(actions)
    }

    fn receive(&mut self, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut outbound = Vec::new();
        for packet in packets {
            let core = self.node.handle_packet(InterfaceId(0), &packet);
            outbound.extend(self.absorb_core(core));
        }
        outbound
    }
}

/// Shuttle packets between the two peers until the exchange quiesces.
fn pump(sender: &mut Sender, receiver: &mut Receiver, mut to_receiver: Vec<Vec<u8>>) {
    let mut to_sender: Vec<Vec<u8>> = Vec::new();
    for _ in 0..512 {
        if !to_receiver.is_empty() {
            to_sender.extend(receiver.receive(std::mem::take(&mut to_receiver)));
        }
        if !to_sender.is_empty() {
            to_receiver.extend(sender.receive(std::mem::take(&mut to_sender)));
        }
        if to_sender.is_empty() && to_receiver.is_empty() {
            return;
        }
    }
    panic!("NodeCore exchange did not quiesce");
}

fn exchange_announces(sender: &mut Sender, receiver: &mut Receiver) {
    let app_data = announce::delivery(Some(b"peer"), None);
    let from_sender = sender
        .node
        .announce_destination(&sender.destination, Some(&app_data))
        .expect("announce sender");
    let to_receiver = sender.absorb_core(from_sender);
    let from_receiver = receiver
        .node
        .announce_destination(&receiver.destination, Some(&app_data))
        .expect("announce receiver");
    let to_sender = receiver.absorb_core(from_receiver);

    let replies_to_receiver = sender.receive(to_sender);
    pump(sender, receiver, replies_to_receiver);
    let replies_to_sender = receiver.receive(to_receiver);
    let mut back = replies_to_sender;
    for _ in 0..8 {
        if back.is_empty() {
            break;
        }
        back = sender.receive(std::mem::take(&mut back));
        if back.is_empty() {
            break;
        }
        back = receiver.receive(std::mem::take(&mut back));
    }
}

fn direct_message(
    seed: u8,
    destination: DestinationHash,
    source: &Identity,
    body: Vec<u8>,
) -> Message {
    Message::create(
        destination.into_bytes(),
        [seed; 16],
        source,
        NOW_UNIX + 0.25,
        b"direct".to_vec(),
        body,
        Vec::new(),
        DeliveryMethod::Direct,
    )
    .expect("direct message")
}

/// Shuttle at most `rounds` request/response round trips, so a transfer can
/// be observed mid-flight instead of run to completion.
fn exchange_rounds(
    sender: &mut Sender,
    receiver: &mut Receiver,
    mut to_receiver: Vec<Vec<u8>>,
    rounds: usize,
) {
    for _ in 0..rounds {
        let to_sender = receiver.receive(std::mem::take(&mut to_receiver));
        if to_sender.is_empty() {
            return;
        }
        to_receiver = sender.receive(to_sender);
        if to_receiver.is_empty() {
            return;
        }
    }
}

/// Queue a direct message and drive the router until NodeCore has it,
/// returning the message id, the link that carries it, and the packets the
/// submitting tick produced.
fn queue_and_submit(
    sender: &mut Sender,
    receiver: &mut Receiver,
    body: Vec<u8>,
    seed: u8,
) -> ([u8; 32], LinkId, Vec<Vec<u8>>) {
    let destination = receiver.destination;
    let message = direct_message(seed, destination, &sender.signing_identity, body);
    let id = message.message_id;
    let output = sender
        .router
        .enqueue(&sender.node, message)
        .expect("enqueue direct message");
    let packets = sender.absorb_router(output);
    pump(sender, receiver, packets);

    // Tick 1 finds no link and starts one. Python charges exactly this
    // attempt (LXMRouter.py:2825 with 2831/2837). A later tick submits over
    // the established link, which Python does without touching the counter
    // (LXMRouter.py:2784-2791). Stop at the submitting tick without pumping
    // its packets, so the observation is of a message in flight rather than
    // of one the receiver has already proved.
    let mut submitted = Vec::new();
    for _ in 0..8 {
        let packets = sender.tick();
        if sender.state(&id) == Some(MessageState::Sending) {
            submitted = packets;
            break;
        }
        pump(sender, receiver, packets);
        sender.advance_ms(11_000);
    }
    assert_eq!(
        sender.state(&id),
        Some(MessageState::Sending),
        "the direct message was never handed to NodeCore"
    );

    let link_id = sender
        .router
        .node()
        .direct_link(&destination)
        .expect("an active direct link carries the submission");
    (id, link_id, submitted)
}

#[test]
fn direct_submission_over_an_established_link_consumes_no_extra_attempt() {
    let mut sender = sender(21);
    let mut receiver = receiver(121);
    exchange_announces(&mut sender, &mut receiver);
    assert!(
        sender.node.has_path(&receiver.destination),
        "the sender must know a path before the direct attempt starts"
    );

    let (id, _link_id, _packets) = queue_and_submit(
        &mut sender,
        &mut receiver,
        b"small direct payload".to_vec(),
        7,
    );

    // One direct delivery cycle -- discover/establish, then submit -- is one
    // attempt in the reference (LXMRouter.py:2820-2839).
    assert_eq!(
        sender.attempts(&id),
        1,
        "one direct delivery cycle must consume exactly one delivery attempt"
    );
}

#[test]
fn a_retryable_outgoing_resource_failure_tears_down_its_direct_link() {
    let mut sender = sender(22);
    let mut receiver = receiver(122);
    exchange_announces(&mut sender, &mut receiver);

    let (_id, link_id, packets) =
        queue_and_submit(&mut sender, &mut receiver, vec![0x5au8; 2_048], 8);
    // One round trip: the receiver accepts the advertisement and the sender
    // starts the transfer. Its parts are then dropped, leaving the Resource
    // in flight.
    exchange_rounds(&mut sender, &mut receiver, packets, 1);
    let resource_hash = *receiver
        .accepted_resources
        .last()
        .expect("the submission is an outgoing Resource");

    let output = sender
        .router
        .handle_event(
            &mut sender.node,
            &NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error: ResourceError::Timeout,
                is_sender: true,
            },
        )
        .expect("router handles the failed outgoing Resource");
    let _ = sender.absorb_router(output);

    // LXMessage.py:604-606 tears the link down before the message becomes
    // eligible for another attempt.
    assert!(
        sender.node.link(&link_id).is_none(),
        "a retryable Resource failure must tear its direct link down"
    );
}

#[test]
fn a_receiver_cancelled_resource_is_rejected_and_keeps_the_link() {
    let mut sender = sender(23);
    let mut receiver = receiver(123);
    exchange_announces(&mut sender, &mut receiver);

    let (id, link_id, packets) =
        queue_and_submit(&mut sender, &mut receiver, vec![0x3cu8; 2_048], 9);
    exchange_rounds(&mut sender, &mut receiver, packets, 1);
    let resource_hash = *receiver
        .accepted_resources
        .last()
        .expect("the submission is an outgoing Resource");

    sender.events.clear();
    let output = sender
        .router
        .handle_event(
            &mut sender.node,
            &NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error: ResourceError::Cancelled,
                is_sender: true,
            },
        )
        .expect("router handles the receiver's cancel");
    let _ = sender.absorb_router(output);

    // Resource.py:1106-1110 -> LXMessage.py:601-602: REJECTED is terminal and
    // does not tear the link down.
    assert!(
        sender.events.iter().any(|event| matches!(
            event,
            RouterEvent::MessageState {
                message_id,
                state: MessageState::Rejected,
            } if *message_id == id
        )),
        "a receiver cancel is a terminal rejection, not a retry: {:?}",
        sender.events
    );
    assert!(
        !sender.router.outbound().contains_key(&id),
        "a rejected message must leave the outbound queue"
    );
    assert!(
        sender.node.link(&link_id).is_some(),
        "a rejection must preserve the reusable direct link"
    );
}

/// The point of `defer_resource_builds`: the build a tick would have run is
/// handed out instead, so the tick emits nothing for that message and the
/// transfer only starts when the caller returns what it built.
///
/// The state assertions are the load-bearing part. Handing a build out is not
/// a submission, so the deferring tick must leave the entry `Outbound` — it is
/// `commit_resource_build` that turns it into `Sending`, at the point the core
/// accepts the transfer. The version of this test written against `efdaac39`
/// asserted `Sending` right after the tick, which is defect D1 stated as an
/// expectation.
#[test]
fn a_deferred_resource_build_leaves_the_tick_and_starts_on_commit() {
    let mut sender = sender_with(
        60,
        RouterConfig {
            defer_resource_builds: true,
            ..RouterConfig::default()
        },
    );
    let mut receiver = receiver(160);
    exchange_announces(&mut sender, &mut receiver);

    let message = direct_message(
        11,
        receiver.destination,
        &sender.signing_identity,
        vec![0x7au8; 2_048],
    );
    let id = message.message_id;
    let output = sender
        .router
        .enqueue(&sender.node, message)
        .expect("enqueue direct message");
    let packets = sender.absorb_router(output);
    pump(&mut sender, &mut receiver, packets);

    let mut deferring_tick = Vec::new();
    let mut handed_out = false;
    for _ in 0..8 {
        let packets = sender.tick();
        if sender
            .events
            .iter()
            .any(|event| matches!(event, RouterEvent::ResourceBuildPending(m) if *m == id))
        {
            deferring_tick = packets;
            handed_out = true;
            break;
        }
        pump(&mut sender, &mut receiver, packets);
        sender.advance_ms(11_000);
    }
    assert!(handed_out, "the caller was never told a build is waiting");
    assert_eq!(
        sender.state(&id),
        Some(MessageState::Outbound),
        "handing a build out is not a submission: the entry stays Outbound \
         until the built transfer is committed"
    );
    assert!(
        deferring_tick.is_empty(),
        "the deferring tick must not put the Resource on the wire"
    );

    let mut builds = sender.router.take_resource_builds();
    assert_eq!(builds.len(), 1, "exactly one build was handed out");
    let built = builds
        .remove(0)
        .build(&mut OsRng)
        .expect("build off the caller's lock");
    let output = sender
        .router
        .commit_resource_build(&mut sender.node, built)
        .expect("commit the built transfer");
    let packets = sender.absorb_router(output);
    assert!(!packets.is_empty(), "commit must advertise the Resource");
    assert_eq!(
        sender.state(&id),
        Some(MessageState::Sending),
        "the commit is the submission, and must mark the entry Sending"
    );
    pump(&mut sender, &mut receiver, packets);

    assert!(
        !receiver.accepted_resources.is_empty(),
        "the receiver never saw the transfer start"
    );
}

// ---------------------------------------------------------------------------
// Codeberg #196 / PR #212, batch A: what the handoff still gets wrong.
//
// Every test below is named in the plan at
// `~/.local/state/leviculum/attachments/pr212-plan.md` section 6a. Those that
// describe behaviour the tree does not have yet carry `#[ignore]` with the
// batch that must remove it, so the suite stays green while the red is on
// record. Running them is `cargo test -p leviculum-lxmf --test
// direct_delivery_attempts -- --ignored`.
//
// Three vacuity guards run in each of them, because every one of these paths
// has a green-for-nothing failure mode:
//
// * `LxmfNode::representation(&message)` is `DirectResource` — a body under
//   `DIRECT_PACKET_MDU` (431 bytes) would take the packet path and never
//   build anything.
// * `RouterEvent::ResourceBuildPending`, emitted nowhere else, is what says
//   the deferral ran. The entry state is not usable for that: batch B1
//   changes it.
// * where arrival matters, the receiver's `accepted_resources` is non-empty,
//   so a link packet cannot pass for a Resource.
// ---------------------------------------------------------------------------

fn deferring_config() -> RouterConfig {
    RouterConfig {
        defer_resource_builds: true,
        ..RouterConfig::default()
    }
}

/// Assert the message really takes the Resource path. `DIRECT_PACKET_MDU` is
/// 431 bytes (`leviculum-lxmf/src/node.rs:39`), so the 2 KiB bodies below are
/// `DirectResource` — but only an assertion says so.
fn assert_direct_resource(message: &Message) {
    assert_eq!(
        LxmfNode::representation(message),
        Ok(DeliveryRepresentation::DirectResource),
        "the test payload must take the Resource path, not the packet path"
    );
}

/// Queue a 2 KiB direct message and return its id.
fn enqueue_resource(sender: &mut Sender, receiver: &mut Receiver, seed: u8) -> [u8; 32] {
    let message = direct_message(
        seed,
        receiver.destination,
        &sender.signing_identity,
        vec![0x7au8; 2_048],
    );
    assert_direct_resource(&message);
    let id = message.message_id;
    let output = sender
        .router
        .enqueue(&sender.node, message)
        .expect("enqueue direct message");
    let packets = sender.absorb_router(output);
    pump(sender, receiver, packets);
    id
}

/// Tick until the router hands out one more build for `id`, and return the
/// packets that tick produced.
///
/// Detection is on `ResourceBuildPending`, not on `MessageState::Sending`:
/// the state the deferral leaves behind is precisely what batch B1 changes,
/// and a test that keyed on it would stop finding the build.
fn tick_until_next_build(
    sender: &mut Sender,
    receiver: &mut Receiver,
    id: &[u8; 32],
) -> Vec<Vec<u8>> {
    let before = sender.builds_pending(id);
    for _ in 0..8 {
        let packets = sender.tick();
        if sender.builds_pending(id) > before {
            return packets;
        }
        pump(sender, receiver, packets);
        sender.advance_ms(11_000);
    }
    panic!(
        "no build was handed out within 8 ticks: state {:?}, attempts {:?}, builds queued {}",
        sender.state(id),
        sender.router.outbound().get(id).map(|entry| entry.attempts),
        sender.router.take_resource_builds().len()
    );
}

/// Take exactly one handed-out build and run it, off the router as a real
/// caller would.
fn take_one_built(sender: &mut Sender, id: &[u8; 32]) -> leviculum_lxmf::BuiltResource {
    let mut builds = sender.router.take_resource_builds();
    assert_eq!(builds.len(), 1, "exactly one build was handed out");
    assert_eq!(
        builds[0].message_id(),
        *id,
        "the handed-out build belongs to another message"
    );
    builds
        .remove(0)
        .build(&mut OsRng)
        .expect("build off the caller's lock")
}

/// Establish the direct link ahead of the measurement, so a timed tick times
/// the send and not the link handshake.
fn link_to(sender: &mut Sender, receiver: &mut Receiver) {
    let destination = receiver.destination;
    let (_state, output) = sender
        .router
        .node_mut()
        .ensure_direct_link(&mut sender.node, destination)
        .expect("start a direct link");
    let packets = take_packets(output.core.actions);
    pump(sender, receiver, packets);
    assert!(
        sender.router.node().direct_link(&destination).is_some(),
        "the direct link never came up"
    );
}

/// U2 — a build the caller never returns must come back around.
///
/// `efdaac39` answers the deferral with `Ok(LxmfNodeOutput::default())` and
/// the arm behind it sets `MessageState::Sending`. The `Sending` guard in
/// `tick` sits above the attempt check, so the entry is skipped on every
/// later tick and never retried, never failed: it is stranded. A host that
/// drains the queue and crashes, or whose build errors, produces exactly
/// this.
#[test]
fn a_build_that_is_never_committed_is_retried_not_stranded() {
    let mut sender = sender_with(61, deferring_config());
    let mut receiver = receiver(161);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 12);

    let deferring = tick_until_next_build(&mut sender, &mut receiver, &id);
    assert!(
        deferring.is_empty(),
        "the deferring tick must not put the Resource on the wire"
    );

    // The caller drains the work and returns nothing: a crash, a build error,
    // a channel whose consumer went away.
    let abandoned = sender.router.take_resource_builds();
    assert_eq!(abandoned.len(), 1, "exactly one build was handed out");
    drop(abandoned);

    // DELIVERY_RETRY_WAIT_MS is 10 s, and `tick_until_next_build` advances
    // 11 s per round.
    let retried = tick_until_next_build(&mut sender, &mut receiver, &id);
    assert!(
        retried.is_empty(),
        "the retry must defer again, not build inside the tick"
    );
    assert_eq!(
        sender.router.take_resource_builds().len(),
        1,
        "the abandoned message must be handed out again"
    );
}

/// U2's negative control: with `defer_resource_builds` off the same message
/// is built inside the tick, goes on the wire in that tick, and no build is
/// ever handed out — so U2's red is the deferral's, not the harness's.
#[test]
fn a_composed_resource_send_hands_out_no_build_and_leaves_in_its_tick() {
    let mut sender = sender_with(62, RouterConfig::default());
    let mut receiver = receiver(162);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 13);

    let mut submitted = Vec::new();
    for _ in 0..8 {
        let packets = sender.tick();
        if sender.state(&id) == Some(MessageState::Sending) {
            submitted = packets;
            break;
        }
        pump(&mut sender, &mut receiver, packets);
        sender.advance_ms(11_000);
    }
    assert_eq!(
        sender.state(&id),
        Some(MessageState::Sending),
        "the composed send never handed the message to NodeCore"
    );
    assert!(
        !submitted.is_empty(),
        "the composed send must advertise the Resource in its own tick"
    );
    assert_eq!(
        sender.builds_pending(&id),
        0,
        "no build may be handed out with the flag off"
    );
    assert!(
        sender.router.take_resource_builds().is_empty(),
        "the build queue must stay empty with the flag off"
    );
    pump(&mut sender, &mut receiver, submitted);
    assert!(
        !receiver.accepted_resources.is_empty(),
        "the receiver never saw the composed transfer start"
    );
}

/// U3 — two due Resources to one destination, no host misbehaviour.
///
/// `enqueue` sets `next_attempt_ms = now_ms`, so both are due in one tick.
/// Phase 1 rejects only on `has_outgoing_resource` and the deferral installs
/// nothing, so both capture. The first commit installs; the second is refused
/// by the core, and under `efdaac39` its entry is already `Sending` and is
/// therefore never retried. Master delivers the same pair.
#[test]
fn two_due_resources_to_one_destination_both_complete() {
    let mut sender = sender_with(63, deferring_config());
    let mut receiver = receiver(163);
    exchange_announces(&mut sender, &mut receiver);
    let first = enqueue_resource(&mut sender, &mut receiver, 14);
    let second = enqueue_resource(&mut sender, &mut receiver, 15);
    assert_ne!(first, second, "the two messages must be distinct");

    // Drive to the tick that captures both.
    let mut builds = Vec::new();
    for _ in 0..8 {
        let packets = sender.tick();
        builds = sender.router.take_resource_builds();
        if builds.len() == 2 {
            assert!(
                packets.is_empty(),
                "the deferring tick must not put a Resource on the wire"
            );
            break;
        }
        for build in builds.drain(..) {
            let built = build.build(&mut OsRng).expect("build off the lock");
            let output = sender
                .router
                .commit_resource_build(&mut sender.node, built)
                .expect("early commit");
            let packets = sender.absorb_router(output);
            pump(&mut sender, &mut receiver, packets);
        }
        pump(&mut sender, &mut receiver, packets);
        sender.advance_ms(11_000);
    }
    assert_eq!(
        builds.len(),
        2,
        "both due messages must capture a build in one tick"
    );
    assert_eq!(sender.builds_pending(&first), 1);
    assert_eq!(sender.builds_pending(&second), 1);

    // `take_resource_builds` hands them back in queue order, which is by
    // message id, not by enqueue order. Pick by id so the two arms of the
    // assertion below cannot swap.
    let position = builds
        .iter()
        .position(|build| build.message_id() == first)
        .expect("the first message's build");
    let built_first = builds
        .remove(position)
        .build(&mut OsRng)
        .expect("build one");
    let built_second = builds.remove(0).build(&mut OsRng).expect("build two");

    let output = sender
        .router
        .commit_resource_build(&mut sender.node, built_first)
        .expect("the first commit installs its transfer");
    let packets = sender.absorb_router(output);
    assert!(!packets.is_empty(), "the first commit must advertise");

    // The link now carries a transfer. This is the exact error, not
    // `is_err()`. The plan expected B2's epoch to take over this refusal;
    // measured against the epoch it does not, and the mechanism says why: the
    // epoch is per-entry, the first commit's `Sending` transition moves only
    // the FIRST message's epoch, and this second commit belongs to the second
    // message, whose entry is unchanged since its capture. The refusal
    // therefore still comes from the core's transfer guard. (Switching the
    // assertion to `StaleBuild` was observed red against the B2 tree.)
    let refused = sender
        .router
        .commit_resource_build(&mut sender.node, built_second)
        .expect_err("one link cannot carry two outgoing Resources");
    assert_eq!(
        refused,
        RouterError::Node(LxmfNodeError::Resource(ResourceError::TransferInProgress)),
        "the second commit must be refused by the core's transfer guard"
    );
    pump(&mut sender, &mut receiver, packets);

    // A host loop: tick, commit whatever comes back, repeat.
    for _ in 0..24 {
        if sender.router.outbound().is_empty() {
            break;
        }
        let packets = sender.tick();
        pump(&mut sender, &mut receiver, packets);
        for build in sender.router.take_resource_builds() {
            let built = build.build(&mut OsRng).expect("build off the lock");
            match sender.router.commit_resource_build(&mut sender.node, built) {
                Ok(output) => {
                    let packets = sender.absorb_router(output);
                    pump(&mut sender, &mut receiver, packets);
                }
                Err(RouterError::Node(LxmfNodeError::Resource(
                    ResourceError::TransferInProgress,
                ))) => {}
                Err(error) => panic!("unexpected commit error: {error:?}"),
            }
        }
        sender.advance_ms(11_000);
    }

    for (label, id) in [("first", first), ("second", second)] {
        assert!(
            sender.events.iter().any(|event| matches!(
                event,
                RouterEvent::MessageState {
                    message_id,
                    state: MessageState::Delivered,
                } if *message_id == id
            )),
            "the {label} message was never delivered: state {:?}",
            sender.state(&id)
        );
    }
    assert!(
        receiver.accepted_resources.len() >= 2,
        "the receiver saw {} transfers, not two",
        receiver.accepted_resources.len()
    );
}

/// U4 — a cancel between capture and commit must keep the message off the air.
///
/// `commit_resource_build` never checks the message is still queued, and
/// `cancel` removes the entry without touching the build queue or the builds
/// already handed out. There is no negative control for this one: with the
/// flag off there is no build to hold across a cancel, so the pairing would
/// be an empty test rather than a control.
#[test]
fn a_cancelled_message_does_not_go_on_the_air_after_its_build_returns() {
    let mut sender = sender_with(64, deferring_config());
    let mut receiver = receiver(164);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 16);

    tick_until_next_build(&mut sender, &mut receiver, &id);
    let built = take_one_built(&mut sender, &id);

    let output = sender
        .router
        .cancel(&mut sender.node, &id)
        .expect("cancel the queued message");
    let packets = sender.absorb_router(output);
    pump(&mut sender, &mut receiver, packets);
    assert!(
        !sender.router.outbound().contains_key(&id),
        "the cancelled message must leave the queue"
    );

    match sender.router.commit_resource_build(&mut sender.node, built) {
        Err(error) => assert_eq!(
            error,
            RouterError::StaleBuild,
            "a build for a cancelled message is stale, not merely unlucky"
        ),
        Ok(output) => {
            let packets = sender.absorb_router(output);
            pump(&mut sender, &mut receiver, packets);
            panic!(
                "the cancelled message went on the air: the receiver started {} \
                 transfer(s) and took delivery of {} message(s)",
                receiver.accepted_resources.len(),
                receiver.delivered.len()
            );
        }
    }
    assert!(
        receiver.accepted_resources.is_empty(),
        "nothing may reach the receiver for a cancelled message"
    );
}

/// U5 — a stamp that lands between capture and commit rewrites the message.
///
/// `set_outbound_stamp` is `pub`, rewrites `entry.message` through
/// `set_stamp` and makes the entry due again, with no state check at any
/// time. A build captured before it carries the pre-stamp bytes, and
/// membership cannot see the difference: the entry is still there and still
/// has the same id, because the id does not cover the stamp.
///
/// Negative control: with the flag off there is no window between capture and
/// commit, so the pairing is vacuous. What is not vacuous is the delivered
/// body, which is why the harness keeps it.
#[test]
fn a_stamp_that_arrives_mid_build_invalidates_that_build() {
    let mut sender = sender_with(65, deferring_config());
    let mut receiver = receiver(165);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 17);

    tick_until_next_build(&mut sender, &mut receiver, &id);
    let built = take_one_built(&mut sender, &id);

    let stamp = vec![0xa5u8; 32];
    let now_ms = sender.clock.get();
    let output = sender
        .router
        .set_outbound_stamp(&id, stamp.clone(), now_ms)
        .expect("attach a stamp to the queued message");
    let _ = sender.absorb_router(output);
    assert_eq!(
        sender
            .router
            .outbound()
            .get(&id)
            .expect("the stamped message is still queued")
            .message()
            .stamp
            .as_deref(),
        Some(stamp.as_slice()),
        "the queue entry must carry the new stamp"
    );

    match sender.router.commit_resource_build(&mut sender.node, built) {
        Err(error) => assert_eq!(
            error,
            RouterError::StaleBuild,
            "a build from before the stamp is stale"
        ),
        Ok(output) => {
            let packets = sender.absorb_router(output);
            pump(&mut sender, &mut receiver, packets);
            panic!(
                "the pre-stamp build went on the air: the receiver got {} message(s), \
                 stamp {:?}, while the queue holds stamp {:?}",
                receiver.delivered.len(),
                receiver.delivered.last().map(|m| m.stamp.clone()),
                Some(stamp),
            );
        }
    }
}

/// U6 — a build from before a failed delivery must be refused (S2, ABA).
///
/// This test cannot be arranged in this harness, and the attempt is kept so
/// the reason is on record rather than in prose. Three separate blockers,
/// found in review and confirmed here:
///
/// 1. Under `efdaac39` the deferral sets `Sending`, so the entry is skipped
///    and a second build is not capturable at all (that is D1, not S2).
/// 2. A synthetic `NodeEvent::ResourceFailed` does not clear the *core's*
///    `has_outgoing_resource`, so phase 1 keeps returning
///    `TransferInProgress` no matter what the router believes.
/// 3. The only failure that keeps the link is `ResourceError::Cancelled`, and
///    the router treats it as terminal: the message becomes `Rejected` and
///    leaves `outbound` (`router.rs`, the `DeliveryFailed` /
///    `Resource(Cancelled)` arm). It never returns to `Outbound`, so the ABA
///    sequence S2 describes has no second `A`.
///
/// What it needs is a receiver that cancels an accepted incoming Resource, so
/// the core itself concludes the transfer. `LxmfNode` does not expose that
/// (`node.rs`: "Cancellation is intentionally not exposed here until
/// `leviculum-core` can cancel an already accepted inbound Resource by
/// hash"). That is test infrastructure in `leviculum-core`, not a line in
/// this file.
#[test]
#[ignore = "CANNOT BE ARRANGED (#196 S2): needs a core-side inbound Resource cancel; see the doc comment. Not evidence for or against any batch."]
fn a_build_from_before_a_failed_delivery_is_refused() {
    let mut sender = sender_with(66, deferring_config());
    let mut receiver = receiver(166);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 18);

    tick_until_next_build(&mut sender, &mut receiver, &id);
    let built = take_one_built(&mut sender, &id);
    let output = sender
        .router
        .commit_resource_build(&mut sender.node, built)
        .expect("the first commit installs its transfer");
    let packets = sender.absorb_router(output);
    exchange_rounds(&mut sender, &mut receiver, packets, 1);
    let resource_hash = *receiver
        .accepted_resources
        .last()
        .expect("the submission is an outgoing Resource");
    let link_id = sender
        .router
        .node()
        .direct_link(&receiver.destination)
        .expect("the transfer's link");

    // Blocker 3: the only link-preserving failure is terminal.
    let output = sender
        .router
        .handle_event(
            &mut sender.node,
            &NodeEvent::ResourceFailed {
                link_id,
                resource_hash,
                error: ResourceError::Cancelled,
                is_sender: true,
            },
        )
        .expect("router handles the receiver's cancel");
    let _ = sender.absorb_router(output);
    assert!(
        !sender.router.outbound().contains_key(&id),
        "a receiver cancel is terminal: the entry never returns to Outbound, \
         so there is no ABA window to test"
    );

    // Blocker 2: the core still believes a Resource is outgoing on that link.
    let params = sender.router.node_mut().resource_send_params(
        &sender.node,
        &direct_message(
            18,
            receiver.destination,
            &sender.signing_identity,
            vec![0x7au8; 2_048],
        ),
    );
    assert_eq!(
        params.err(),
        Some(LxmfNodeError::Resource(ResourceError::TransferInProgress)),
        "a synthetic ResourceFailed does not clear the core's outgoing Resource, \
         so no second build can be captured"
    );

    panic!(
        "S2 is unreachable from this harness: see the doc comment. \
         It needs a receiver-side cancel produced by leviculum-core."
    );
}

/// U11 — `commit_resource_build` is a public router operation and must end
/// like one: one coalesced `PersistenceRequested` and the queue's deadline on
/// its output. It ends with neither, so a host that only ever commits never
/// learns it has state to write, and its event loop gets no wake-up time.
#[test]
fn commit_resource_build_requests_persistence_and_applies_its_deadline() {
    let mut sender = sender_with(67, deferring_config());
    let mut receiver = receiver(167);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 19);

    tick_until_next_build(&mut sender, &mut receiver, &id);
    let built = take_one_built(&mut sender, &id);

    // `apply_deadline` reports the *earliest* of the core's deadline and the
    // queue's, so the queue's has to be strictly the earlier one or the
    // assertion below would hold whether or not commit applies it at all.
    // Two steps get there: service the core's overdue timers, then queue a
    // second message, which `enqueue` makes due immediately.
    let serviced = sender.node.handle_timeout();
    let packets = sender.absorb_core(serviced);
    pump(&mut sender, &mut receiver, packets);
    let second = direct_message(
        24,
        receiver.destination,
        &sender.signing_identity,
        b"a second message, due now".to_vec(),
    );
    let output = sender
        .router
        .enqueue(&sender.node, second)
        .expect("enqueue a second message");
    let _ = sender.absorb_router(output);

    let output = sender
        .router
        .commit_resource_build(&mut sender.node, built)
        .expect("commit the built transfer");
    let expected_deadline = sender
        .router
        .next_deadline()
        .expect("the queue has a deadline");
    assert!(
        sender
            .node
            .next_deadline()
            .is_none_or(|core| core > expected_deadline),
        "the core holds the earlier deadline, so this assertion could not tell \
         whether commit applied the queue's: core {:?}, queue {expected_deadline}",
        sender.node.next_deadline()
    );
    assert!(
        output
            .events
            .iter()
            .any(|event| matches!(event, RouterEvent::PersistenceRequested)),
        "commit changed the queue and must ask for persistence: {:?}",
        output.events
    );
    assert_eq!(
        output.core.next_deadline_ms,
        Some(expected_deadline),
        "commit must carry the queue's own deadline out to the host"
    );
    let packets = sender.absorb_router(output);
    pump(&mut sender, &mut receiver, packets);
    assert!(
        !receiver.accepted_resources.is_empty(),
        "the commit under test must really have started a transfer"
    );
}

/// U12 — a build captured before a `restore` belongs to the queue that was
/// replaced.
///
/// `restore` swaps `outbound` wholesale and touches neither `pending_builds`
/// nor the builds already handed out. Membership cannot catch it: the
/// restored snapshot contains the same message id, so the old build lands on
/// an entry it never came from.
#[test]
fn a_build_captured_before_a_restore_is_refused() {
    let mut sender = sender_with(68, deferring_config());
    let mut receiver = receiver(168);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 20);

    tick_until_next_build(&mut sender, &mut receiver, &id);
    let built = take_one_built(&mut sender, &id);

    let mut storage = MemoryLxmfStorage::new(1 << 20);
    sender
        .router
        .persist(&mut storage)
        .expect("persist the queue that holds the message");
    sender
        .router
        .restore(&storage)
        .expect("restore the persisted queue");
    assert!(
        sender.router.outbound().contains_key(&id),
        "the restored queue still holds the same message id, so membership \
         alone cannot refuse the old build"
    );

    match sender.router.commit_resource_build(&mut sender.node, built) {
        Err(error) => assert_eq!(
            error,
            RouterError::StaleBuild,
            "a build from a replaced queue is stale"
        ),
        Ok(output) => {
            let packets = sender.absorb_router(output);
            pump(&mut sender, &mut receiver, packets);
            panic!(
                "a build from before the restore was installed: the receiver \
                 started {} transfer(s) and took delivery of {} message(s)",
                receiver.accepted_resources.len(),
                receiver.delivered.len()
            );
        }
    }
}

/// U13 — one message, at most one outstanding build.
///
/// Green today, and for a reason that is about to be removed: the `Sending`
/// mark `efdaac39` sets is what keeps the entry out of the due list. Batch B1
/// removes that mark, and only C8's outstanding-build set keeps this green
/// afterwards. Without it, `tick` appends a full `Message` clone every 10 s
/// per message against a consumer whose latency is unbounded — the same
/// unbounded growth D2 is a defect for.
#[test]
fn one_message_has_at_most_one_outstanding_build() {
    let mut sender = sender_with(69, deferring_config());
    let mut receiver = receiver(169);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 21);

    tick_until_next_build(&mut sender, &mut receiver, &id);

    // Four retry intervals pass with the build outstanding and uncommitted.
    for _ in 0..4 {
        sender.advance_ms(11_000);
        let packets = sender.tick();
        pump(&mut sender, &mut receiver, packets);
    }

    assert_eq!(
        sender.builds_pending(&id),
        1,
        "the router announced more than one build for one message"
    );
    assert_eq!(
        sender.router.take_resource_builds().len(),
        1,
        "the build queue grew while one build was outstanding"
    );
}

/// U15 — `enqueue` returns a `RouterOutput` and must put its own deadline on
/// it.
///
/// It does not call `apply_deadline`, so a host driven by the returned
/// `next_deadline_ms` sleeps past a message that is due now. Asserting
/// through `next_deadline()` would be green today, because that folds in
/// every entry's `next_attempt_ms` — the observation has to be the output
/// `enqueue` itself hands back. Flag-independent: there is no control here,
/// because `defer_resource_builds` is not on this path.
#[test]
fn enqueue_returns_its_own_deadline_in_its_output() {
    let mut sender = sender_with(70, RouterConfig::default());
    let mut receiver = receiver(170);
    exchange_announces(&mut sender, &mut receiver);

    // One tick first, so the scheduler cursor is not itself sitting at 0 and
    // the asserted deadline can only have come from the new entry.
    let packets = sender.tick();
    pump(&mut sender, &mut receiver, packets);

    let now_ms = sender.clock.get();
    let message = direct_message(
        22,
        receiver.destination,
        &sender.signing_identity,
        vec![0x7au8; 2_048],
    );
    let id = message.message_id;
    let output = sender
        .router
        .enqueue(&sender.node, message)
        .expect("enqueue direct message");
    assert_eq!(
        sender
            .router
            .outbound()
            .get(&id)
            .expect("the message is queued")
            .next_attempt_ms,
        now_ms,
        "enqueue makes the entry due immediately, so there is a deadline to report"
    );
    assert_eq!(
        output.core.next_deadline_ms,
        Some(now_ms),
        "enqueue must report the deadline it just created"
    );
}

/// U16 — the point of the whole change: the build leaves the tick.
///
/// Nothing else in the suite observes it. Every other test here proves "still
/// delivers"; this one proves the tick stopped paying for the payload.
///
/// One due message per measurement, not the eight the lock-budget page uses:
/// a second Resource to the same link cannot build in the composed arm at all
/// (`TransferInProgress`), so an N-message comparison would not be comparing
/// like with like. The payload axis is the one this change is about.
///
/// The fixture is incompressible on purpose. `vec![0x5a; n]`, which the PR's
/// own harness used, is collapsed by bz2's RLE front end before the BWT, so a
/// table built on it measures the compressor's best case rather than a
/// message.
///
/// The bound is deliberately loose. What must hold is an order-of-magnitude
/// statement — the deferred tick does not carry the build — not a number.
///
/// The name overstates what the deferral achieves, and the measurement says
/// so: the deferring tick still clones the whole `Message` into the pending
/// build, which is a memcpy that scales linearly. Measured on this tree, a
/// 16x payload costs the deferred tick about 6x (49 us at 16 KiB, 313 us at
/// 256 KiB) while the composed tick at 256 KiB is 174 ms. What left the tick
/// is the *build* — pack, compress, encrypt, hash — not every byte-sized
/// cost. `Arc<Message>` is what would remove the remainder, and the plan
/// defers it to its own issue.
#[test]
fn a_deferred_tick_does_not_scale_with_the_payload() {
    let small = 16 * 1024;
    let large = 256 * 1024;

    let deferred_small = median_resource_tick(80, true, small, incompressible);
    let deferred_large = median_resource_tick(82, true, large, incompressible);
    let composed_small = median_resource_tick(84, false, small, incompressible);
    let composed_large = median_resource_tick(86, false, large, incompressible);

    std::println!(
        "deferred {small}B {deferred_small:?} | composed {small}B {composed_small:?} | \
         deferred {large}B {deferred_large:?} | composed {large}B {composed_large:?}"
    );

    // The build is gone from the tick at every size measured, not just at the
    // largest one.
    for (len, deferred, composed) in [
        (small, deferred_small, composed_small),
        (large, deferred_large, composed_large),
    ] {
        assert!(
            composed > deferred * 20,
            "at {len}B the deferring tick must cost an order of magnitude less \
             than the composed one: deferred {deferred:?} vs composed {composed:?}"
        );
    }

    // What remains in the tick is the `Message` clone. A 16x payload may not
    // cost it 4x plus 2 ms — 2 ms being about one percent of what the composed
    // tick pays at the same size.
    assert!(
        deferred_large < deferred_small * 4 + core::time::Duration::from_millis(2),
        "the deferred tick grew with the payload: {deferred_small:?} at {small}B \
         vs {deferred_large:?} at {large}B (composed at {large}B is {composed_large:?})"
    );
}

/// The numeric harness behind U16, `#[ignore]`d with the other timing
/// reports. Medians of five with one warm-up, both arms, all three payload
/// classes:
///
/// ```text
/// cargo test -p leviculum-lxmf --release --test direct_delivery_attempts \
///     measure_deferred_tick_costs -- --ignored --nocapture
/// ```
///
/// The 1 MiB row is segment 1 of a two-segment transfer:
/// `RESOURCE_MAX_EFFICIENT_SIZE` is 1 048 575 bytes and the packed message is
/// above it. Segments 2..N are built on the receive path, under the caller's
/// lock, and neither this change nor this measurement touches them.
///
/// Every column names its payload class, because what is being timed is a
/// compressor and a compressor's cost is a property of the bytes it is given.
#[test]
#[ignore]
fn measure_deferred_tick_costs() {
    // Per-cell warm-up does not cover the first cell's process-level costs.
    // Both arms, because it is the composed one that grows the allocator's
    // arena to bz2's working-set size; without it the first cell measured
    // reads high on the deferred column for a reason that has nothing to do
    // with its bytes.
    let _ = median_resource_tick(200, true, 16 * 1024, payloads::incompressible);
    let _ = median_resource_tick(210, false, 16 * 1024, payloads::incompressible);

    let mut seed = 100u8;
    for (class, generate) in payloads::GENERATORS {
        for len in [16 * 1024usize, 256 * 1024, 1024 * 1024] {
            let deferred = median_resource_tick(seed, true, len, generate);
            seed = seed.wrapping_add(2);
            let composed = median_resource_tick(seed, false, len, generate);
            seed = seed.wrapping_add(2);
            std::println!(
                "tick with one due {len:>7}B {class:>14} Resource: \
                 deferred {deferred:?} | composed {composed:?}"
            );
        }
    }
}

/// The named payload classes behind `core-lock-budget.md`, shared with the
/// `leviculum-lxmf` unit harness (`measure_send_lock_costs`) so both measure
/// the same bytes under the same column names.
#[path = "common/payloads.rs"]
mod payloads;

use payloads::incompressible;

/// Median of five timed `LxmfRouter::tick` calls, each with exactly one due
/// `DirectResource` message on an already-established link, after one
/// discarded warm-up run.
fn median_resource_tick(
    seed: u8,
    defer: bool,
    len: usize,
    generate: payloads::Generator,
) -> core::time::Duration {
    let mut samples = Vec::new();
    for round in 0..6u8 {
        samples.push(one_resource_tick(
            seed.wrapping_add(round),
            defer,
            len,
            generate,
        ));
    }
    samples.remove(0); // warm-up: first run pays one-time setup
    samples.sort();
    samples[samples.len() / 2]
}

fn one_resource_tick(
    seed: u8,
    defer: bool,
    len: usize,
    generate: payloads::Generator,
) -> core::time::Duration {
    let config = if defer {
        deferring_config()
    } else {
        RouterConfig::default()
    };
    let mut sender = sender_with(seed, config);
    let mut receiver = receiver(seed.wrapping_add(128));
    exchange_announces(&mut sender, &mut receiver);
    link_to(&mut sender, &mut receiver);

    let message = direct_message(
        seed,
        receiver.destination,
        &sender.signing_identity,
        generate(len),
    );
    assert_direct_resource(&message);
    let id = message.message_id;
    let output = sender
        .router
        .enqueue(&sender.node, message)
        .expect("enqueue direct message");
    let _ = sender.absorb_router(output);

    let started = std::time::Instant::now();
    let output = sender.router.tick(&mut sender.node).expect("tick");
    let elapsed = started.elapsed();

    // The timed tick must have done the work, or the number is of an empty
    // loop. This is the same vacuity trap as an FFI test that reports ok in
    // 0.00 s because the library was absent.
    if defer {
        assert!(
            output
                .events
                .iter()
                .any(|event| matches!(event, RouterEvent::ResourceBuildPending(m) if *m == id)),
            "the timed tick did not hand out a build"
        );
        assert!(
            output.core.actions.is_empty(),
            "the deferring tick put something on the wire"
        );
    } else {
        assert!(
            !output.core.actions.is_empty(),
            "the timed tick did not advertise the Resource"
        );
        assert_eq!(
            sender.state(&id),
            Some(MessageState::Sending),
            "the timed tick did not submit the message"
        );
    }
    elapsed
}

/// The livelock guard on the epoch: a no-op write must not move it.
///
/// The tick's deferral arm re-asserts `state = Outbound` on an entry that
/// already is `Outbound` every retry interval. If that assignment advanced
/// the epoch, every build older than one interval would come back
/// `StaleBuild` and a consumer slower than the interval could never land a
/// commit. This holds a build across two retry intervals — with the deferral
/// arm running each time — and then commits it successfully.
#[test]
fn a_build_held_across_retry_intervals_still_commits() {
    let mut sender = sender_with(72, deferring_config());
    let mut receiver = receiver(172);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 25);

    tick_until_next_build(&mut sender, &mut receiver, &id);
    let built = take_one_built(&mut sender, &id);

    // Two retry intervals pass with the build in the caller's hands. The
    // drain released the outstanding-build marker, so the first of these
    // ticks captures a fresh build and the second sees its marker; both runs
    // of the deferral arm re-assert `Outbound` on the unchanged entry.
    for _ in 0..2 {
        sender.advance_ms(11_000);
        let packets = sender.tick();
        pump(&mut sender, &mut receiver, packets);
    }

    let output = sender
        .router
        .commit_resource_build(&mut sender.node, built)
        .expect("a build held across no-op re-emissions must still commit");
    let packets = sender.absorb_router(output);
    assert_eq!(
        sender.state(&id),
        Some(MessageState::Sending),
        "the late commit is still the submission"
    );
    pump(&mut sender, &mut receiver, packets);
    assert!(
        !receiver.accepted_resources.is_empty(),
        "the committed transfer must reach the receiver"
    );
}

/// U17 — cancel, re-enqueue the same content, and the build from the previous
/// lifetime must still be refused.
///
/// The message id is derived from the content, so a re-enqueue restores the
/// same key: membership passes, and a per-entry counter created fresh at
/// `enqueue` would be back at its initial value. Only a router-level
/// monotonic epoch never repeats.
#[test]
fn a_re_enqueued_message_refuses_a_build_from_before_its_cancel() {
    let mut sender = sender_with(71, deferring_config());
    let mut receiver = receiver(171);
    exchange_announces(&mut sender, &mut receiver);
    let id = enqueue_resource(&mut sender, &mut receiver, 23);

    tick_until_next_build(&mut sender, &mut receiver, &id);
    let built = take_one_built(&mut sender, &id);

    let output = sender
        .router
        .cancel(&mut sender.node, &id)
        .expect("cancel the queued message");
    let packets = sender.absorb_router(output);
    pump(&mut sender, &mut receiver, packets);

    let again = direct_message(
        23,
        receiver.destination,
        &sender.signing_identity,
        vec![0x7au8; 2_048],
    );
    assert_eq!(
        again.message_id, id,
        "the re-enqueued message must carry the same id, or the test proves \
         nothing membership does not already catch"
    );
    let output = sender
        .router
        .enqueue(&sender.node, again)
        .expect("re-enqueue the same message");
    let packets = sender.absorb_router(output);
    pump(&mut sender, &mut receiver, packets);

    match sender.router.commit_resource_build(&mut sender.node, built) {
        Err(error) => assert_eq!(
            error,
            RouterError::StaleBuild,
            "a build from a previous entry lifetime is stale"
        ),
        Ok(output) => {
            let packets = sender.absorb_router(output);
            pump(&mut sender, &mut receiver, packets);
            panic!(
                "a build from before the cancel was installed on the re-enqueued \
                 entry: the receiver started {} transfer(s) and took delivery of \
                 {} message(s)",
                receiver.accepted_resources.len(),
                receiver.delivered.len()
            );
        }
    }
}
