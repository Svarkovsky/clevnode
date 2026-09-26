//! Codeberg #217: two distinct messages created inside one second must be two
//! messages, not one duplicate.
//!
//! `Message::create` hashes the timestamp into the message ID, so the ID's only
//! distinguishing value between two otherwise identical messages is the clock.
//! While the router stamped whole seconds, a second "OK" reply sent inside the
//! same second produced a byte-identical ID and was refused at the
//! `outbound.contains_key` check with [`RouterError::Duplicate`]. The reference
//! writes `time.time()` (`reference/LXMF/LXMF/LXMessage.py:357`) — fractional
//! seconds — so a Python node sends both. That is a semantic-compatibility
//! defect, not a hash collision.
//!
//! Every test here runs on a CLOCKLESS node (`Clock::wall_unix_secs` → `None`,
//! the LNode shape) whose emission timebase is seeded through
//! `NodeCore::set_wall_time_unix_secs` and then advanced only by the monotonic
//! clock. That is deliberate and does two things at once:
//!
//! - it makes the sub-second advance *deterministic* — a test that leaned on
//!   the host wall clock would be red or green depending on whether the two
//!   creations straddled a second boundary, and a flaky test is not a test;
//! - it pins the arm of the source-priority chain that has no calendar to read
//!   from, which is exactly the arm where sub-second precision has to be
//!   derived rather than looked up.
//!
//! The wall-clock arm is pinned separately, in `leviculum-std`'s clock tests
//! (`SystemClock::wall_unix_micros` is not truncated) and in
//! `leviculum-core`'s transport tests (`emission_micros` prefers it).

use core::cell::Cell;
use std::rc::Rc;

use leviculum_core::transport::TimeSource;
use leviculum_core::{Clock, DestinationHash, Identity, MemoryStorage, NodeCore, NodeCoreBuilder};
use leviculum_lxmf::router::{
    LxmfRouter, MessageState, PropagationClientConfig, RouterConfig, RouterError,
};
use leviculum_lxmf::storage::MemoryLxmfStorage;
use leviculum_lxmf::{DeliveryMethod, LxmfNode, LxmfNodeConfig, Message, PropagationTransport};
use rand_core::OsRng;

/// A plausible timebase (above `EMISSION_PLAUSIBLE_MIN_SECS`, below the learn
/// ceiling) that no argument in this file passes to the router.
const INJECTED_UNIX: u64 = 1_777_123_456;

const BOOT_MS: u64 = 1_000;

const RECIPIENT: [u8; 16] = [0x71; 16];

/// The LNode shape — a monotonic timer and no calendar — with the tick count
/// held outside the node so a test can advance it.
struct ClocklessClock(Rc<Cell<u64>>);

impl Clock for ClocklessClock {
    fn now_ms(&self) -> u64 {
        self.0.get()
    }
}

type TestNode = NodeCore<OsRng, ClocklessClock, MemoryStorage>;

fn identity_from(seed: u8) -> Identity {
    let mut private = [0u8; 64];
    for (index, byte) in private.iter_mut().enumerate() {
        *byte = seed.wrapping_add(index as u8);
    }
    Identity::from_private_key_bytes(&private).expect("deterministic identity")
}

fn identity_copy(identity: &Identity) -> Identity {
    Identity::from_private_key_bytes(
        &identity
            .private_key_bytes()
            .expect("test identity has private keys"),
    )
    .expect("identity copy")
}

/// A router on a clockless node, its timebase seeded and its monotonic clock
/// held by the returned handle.
fn seeded_router(seed: u8) -> (LxmfRouter, TestNode, Rc<Cell<u64>>) {
    let ticks = Rc::new(Cell::new(BOOT_MS));
    let mut core = NodeCoreBuilder::new().build(
        OsRng,
        ClocklessClock(Rc::clone(&ticks)),
        MemoryStorage::with_defaults(),
    );
    let identity = identity_from(seed);
    let identity_hash = *identity.hash();
    let destination = LxmfNode::delivery_destination(identity).expect("delivery destination");
    let node = LxmfNode::register(&mut core, destination, LxmfNodeConfig::default())
        .expect("register delivery destination");
    assert!(core.set_wall_time_unix_secs(INJECTED_UNIX, TimeSource::Host));
    (
        LxmfRouter::new(node, identity_hash, RouterConfig::default()),
        core,
        ticks,
    )
}

/// The same message twice over: same destination, title, content and fields.
/// Only the clock may distinguish them.
fn identical_message(
    router: &LxmfRouter,
    node: &TestNode,
    method: DeliveryMethod,
) -> Result<Message, RouterError> {
    router.create_message(
        node,
        RECIPIENT,
        b"Re: status".to_vec(),
        b"OK".to_vec(),
        Vec::new(),
        method,
    )
}

/// Codeberg #217, the defect itself.
///
/// Two identical replies one millisecond apart are two messages to every
/// Python peer. Before the fix both received the same whole-second timestamp,
/// hashed to the same ID, and the second was refused as a duplicate of the
/// first — with the first still queued, so the application's second send simply
/// vanished.
#[test]
fn two_identical_messages_in_one_second_get_distinct_ids() {
    let (mut router, node, ticks) = seeded_router(1);

    let first = identical_message(&router, &node, DeliveryMethod::Direct).expect("first message");
    let first_timestamp = first.timestamp;
    let first_id = first.message_id;
    let _ = router.enqueue(&node, first).expect("queue the first reply");

    // One millisecond later. Same wall-clock second, different instant.
    ticks.set(BOOT_MS + 1);

    let second = identical_message(&router, &node, DeliveryMethod::Direct).expect("second message");

    assert_eq!(
        first_timestamp.floor(),
        second.timestamp.floor(),
        "the two creations must fall inside one whole second, or this test \
         proves nothing about #217"
    );
    assert_ne!(
        first_timestamp, second.timestamp,
        "a message created one millisecond later carries a later timestamp; \
         the reference writes time.time() (LXMessage.py:357)"
    );
    assert_ne!(
        first_id, second.message_id,
        "the timestamp is hashed into the message ID, so distinct instants \
         must yield distinct IDs"
    );

    let second_id = second.message_id;
    match router.enqueue(&node, second) {
        Ok(_) => {}
        Err(error) => panic!("the second reply is a new message, not a duplicate: {error:?}"),
    }
    assert_eq!(
        router.outbound().len(),
        2,
        "both replies are queued for delivery"
    );
    assert!(router.outbound().contains_key(&first_id));
    assert!(router.outbound().contains_key(&second_id));
}

/// Control: dedup still works. Sub-second precision must not become a licence
/// to queue the same message twice.
///
/// The clock does not move between the two creations, so both messages hash the
/// same payload and the same instant. Python's `LXMRouter.handle_outbound` has
/// the same property — an identical message re-submitted at the same
/// `time.time()` is the same message ID — and our router must still refuse the
/// second at the `outbound` check.
#[test]
fn the_same_instant_and_payload_still_produce_one_id() {
    let (mut router, node, _ticks) = seeded_router(2);

    let first = identical_message(&router, &node, DeliveryMethod::Direct).expect("first message");
    let second = identical_message(&router, &node, DeliveryMethod::Direct).expect("second message");

    assert_eq!(
        first.timestamp, second.timestamp,
        "a clock that did not move produces the same timestamp"
    );
    assert_eq!(
        first.message_id, second.message_id,
        "same source, destination, payload and instant is one message ID"
    );

    let _ = router.enqueue(&node, first).expect("queue the message");
    assert!(
        matches!(router.enqueue(&node, second), Err(RouterError::Duplicate)),
        "re-submitting a queued message is still a duplicate"
    );
    assert_eq!(router.outbound().len(), 1);
}

/// Control: a delivery retry keeps the original timestamp and ID.
///
/// The timestamp is stamped once at creation and lives on the queued message;
/// nothing on the retry path re-reads the clock. Wall time moves by a full
/// minute between attempts here, so a retry that re-stamped would be visible.
#[test]
fn a_retry_preserves_the_original_timestamp_and_id() {
    let (mut router, mut node, ticks) = seeded_router(3);

    let message = identical_message(&router, &node, DeliveryMethod::Direct).expect("message");
    let id = message.message_id;
    let timestamp = message.timestamp;
    let packed = message.pack();
    let _ = router.enqueue(&node, message).expect("queue the message");

    // No path to the recipient exists, so every tick drives the entry further
    // down the retry ladder (a pre-emptive path request first, then counted
    // attempts). Stop short of MAX_DELIVERY_ATTEMPTS: a failed entry leaves the
    // queue entirely and there would be nothing left to inspect.
    for step in 1..=3u64 {
        ticks.set(BOOT_MS + step * 60_000);
        let _ = router.tick(&mut node).expect("tick");
    }

    let entry = router
        .outbound()
        .get(&id)
        .expect("the entry is still keyed by its original ID");
    assert_eq!(entry.state(), MessageState::Outbound);
    assert!(
        entry.attempts > 0,
        "the retry ladder must actually have run, else this control is vacuous"
    );
    assert_eq!(
        entry.message().timestamp,
        timestamp,
        "a retry re-sends the message it already built"
    );
    assert_eq!(entry.message().message_id, id);
    assert_eq!(
        entry.message().pack(),
        packed,
        "the signed wire bytes are unchanged across retries"
    );
}

/// Control: falling back from direct delivery to propagation keeps the
/// timestamp and ID.
///
/// The fallback is the application cancelling the direct attempt and
/// re-submitting the same `Message` with `method` switched — the timestamp and
/// ID live on the message, so they survive by construction. Pinned because the
/// natural way to get this wrong is to rebuild the message from its parts.
#[test]
fn a_propagation_fallback_preserves_the_timestamp_and_id() {
    let (mut router, mut node, ticks) = seeded_router(4);

    let delivery_identity = {
        let destination = node
            .destination(&router.node().delivery_destination_hash())
            .expect("registered delivery destination");
        identity_copy(destination.identity().expect("delivery identity"))
    };
    let propagation_destination =
        PropagationTransport::destination(delivery_identity).expect("propagation destination");
    let transport = PropagationTransport::register(&mut node, propagation_destination)
        .expect("register propagation client");
    router
        .enable_propagation_client(transport, PropagationClientConfig::default())
        .expect("matching delivery identity");
    let _ = router
        .set_outbound_propagation_node(&mut node, Some(DestinationHash::new([0x92; 16])))
        .expect("select a propagation node");

    let direct = identical_message(&router, &node, DeliveryMethod::Direct).expect("direct message");
    let id = direct.message_id;
    let timestamp = direct.timestamp;
    let mut fallback = direct.clone();
    let _ = router
        .enqueue(&node, direct)
        .expect("queue the direct send");

    // Direct delivery gives up; the application falls back to propagation.
    ticks.set(BOOT_MS + 90_000);
    let _ = router
        .cancel(&mut node, &id)
        .expect("cancel the direct send");
    fallback.method = DeliveryMethod::Propagated;

    assert_eq!(
        fallback.message_id, id,
        "changing the delivery method does not touch the hashed payload"
    );
    assert_eq!(fallback.timestamp, timestamp);
    let _ = router
        .enqueue(&node, fallback)
        .expect("the fallback is not a duplicate of the cancelled attempt");

    let entry = router.outbound().get(&id).expect("queued for propagation");
    assert_eq!(entry.message().timestamp, timestamp);
    assert_eq!(entry.message().method, DeliveryMethod::Propagated);
}

/// Control: a restored outbound message keeps its persisted timestamp.
///
/// The checkpoint stores the signed wire bytes, so the msgpack float64
/// timestamp round-trips exactly — including its fractional part, which is the
/// part #217 introduces. A lossy round-trip would change the ID on restore and
/// silently orphan the queue entry.
#[test]
fn a_restored_outbound_message_keeps_its_persisted_timestamp() {
    let (mut router, node, ticks) = seeded_router(5);

    // A timestamp with a fractional part, so the round-trip has something to
    // lose.
    ticks.set(BOOT_MS + 137);
    let message = identical_message(&router, &node, DeliveryMethod::Direct).expect("message");
    let id = message.message_id;
    let timestamp = message.timestamp;
    let _ = router.enqueue(&node, message).expect("queue the message");

    let mut storage = MemoryLxmfStorage::new(128 * 1024);
    router
        .persist(&mut storage)
        .expect("persist the checkpoint");

    let (mut restored, _node, _ticks) = seeded_router(5);
    restored.restore(&storage).expect("restore the checkpoint");

    let entry = restored
        .outbound()
        .get(&id)
        .expect("the restored entry is keyed by its original ID");
    assert_eq!(
        entry.message().timestamp,
        timestamp,
        "the persisted timestamp survives the checkpoint bit for bit"
    );
    assert_eq!(entry.message().message_id, id);
}
