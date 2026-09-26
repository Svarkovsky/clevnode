//! Codeberg #182: LXMF's three cross-lifetime wire fields have one producer.
//!
//! `docs/src/concepts/time-and-clocks.md` §"One value, one producer" binds
//! every wire field a peer compares across our process lifetimes to
//! `Transport::emission_secs`. LXMF has three — the message timestamp, the
//! ticket expiry and the propagation upload timestamp — and until #182 all
//! three were written from a bare `now_unix: f64` parameter that any caller
//! could fill with uptime seconds. That is Codeberg #155 one crate up: a value
//! self-consistent between our writer and our reader and wrong to a peer.
//!
//! The pins here are deliberately of two kinds.
//!
//! - **Structural**: no public router entry point may take a wall-clock
//!   parameter again. A value pin alone would only prove that today's caller
//!   happens to pass the right thing.
//! - **Behavioural**: the emitted bytes must carry the value the *node's*
//!   timebase holds. Every behavioural test below runs on a CLOCKLESS node
//!   (`Clock::wall_unix_secs` → `None`, the LNode shape) whose timebase is
//!   seeded through `NodeCore::set_wall_time_unix_secs`. That value appears
//!   nowhere in any call argument, so a reintroduced parameter cannot satisfy
//!   these assertions by accident.

use core::cell::Cell;

use leviculum_core::transport::TimeSource;
use leviculum_core::{Clock, DestinationHash, Identity, MemoryStorage, NodeCore, NodeCoreBuilder};
use leviculum_lxmf::constants::{FIELD_TICKET, TICKET_EXPIRY};
use leviculum_lxmf::router::{LxmfRouter, PropagationClientConfig, RouterConfig, RouterError};
use leviculum_lxmf::ticket::Ticket;
use leviculum_lxmf::{DeliveryMethod, LxmfNode, LxmfNodeConfig, PropagationTransport};
use rand_core::OsRng;

/// A timebase no argument in this file passes and no uptime can reach.
/// Plausible (above `EMISSION_PLAUSIBLE_MIN_SECS`, below the learn ceiling),
/// and distinct from the 1_700_000_000 used by the rest of the suite.
const INJECTED_UNIX: u64 = 1_777_123_456;

const BOOT_MS: u64 = 1_000;

/// The LNode shape: a monotonic timer and no calendar. `wall_unix_secs` keeps
/// the trait default of `None`, which is what makes `emission_secs` fall
/// through to the injected timebase (source 3 of the chain in
/// `docs/src/concepts/time-and-clocks.md`).
struct ClocklessClock(Cell<u64>);

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

/// A router on a clockless node. The timebase is left unseeded; each test
/// decides whether to inject one.
fn clockless_router() -> (LxmfRouter, TestNode) {
    let mut core = NodeCoreBuilder::new().build(
        OsRng,
        ClocklessClock(Cell::new(BOOT_MS)),
        MemoryStorage::with_defaults(),
    );
    let identity = identity_from(1);
    let identity_hash = *identity.hash();
    let destination = LxmfNode::delivery_destination(identity).expect("delivery destination");
    let node = LxmfNode::register(&mut core, destination, LxmfNodeConfig::default())
        .expect("register delivery destination");
    (
        LxmfRouter::new(node, identity_hash, RouterConfig::default()),
        core,
    )
}

/// Read the msgpack float64 timestamp back out of the packed wire bytes.
///
/// `pack()` lays out destination(16) || source(16) || signature(64) || payload,
/// and the payload's first element is the timestamp (`LXMessage.py:362`).
fn packed_timestamp(packed: &[u8]) -> f64 {
    assert_eq!(packed[96], 0x94, "four-element payload array");
    assert_eq!(packed[97], 0xcb, "timestamp is msgpack float64");
    f64::from_be_bytes(packed[98..106].try_into().unwrap())
}

fn router_sources() -> Vec<(&'static str, String)> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    [
        "router.rs",
        "router/paper_runtime.rs",
        "router/propagation_runtime.rs",
        "router/stamp_runtime.rs",
    ]
    .into_iter()
    .map(|name| {
        let source = std::fs::read_to_string(root.join(name))
            .unwrap_or_else(|error| panic!("read {name}: {error}"));
        (name, source)
    })
    .collect()
}

/// Return the parameter list of every `pub fn` in `source`, with its name.
fn public_parameter_lists(source: &str) -> Vec<(String, String)> {
    let bytes: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut search = 0usize;
    while let Some(offset) = source[search..].find("pub fn ") {
        let start = search + offset;
        search = start + "pub fn ".len();
        // `pub(crate) fn` / `pub(super) fn` are not the public surface.
        if source[..start].ends_with(')') {
            continue;
        }
        let Some(open_rel) = source[search..].find('(') else {
            continue;
        };
        let name = source[search..search + open_rel]
            .split(['<', ' '])
            .next()
            .unwrap_or_default()
            .to_string();
        let open = search + open_rel;
        let mut depth = 0i32;
        let mut end = open;
        for (index, ch) in bytes.iter().enumerate().skip(open) {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = index;
                        break;
                    }
                }
                _ => {}
            }
        }
        out.push((name, source[open..=end].to_string()));
    }
    out
}

/// No public router entry point may take a wall-clock value from its caller.
///
/// This is the pin that bites on a reintroduction rather than on a wrong
/// value. Before #182, `enqueue`, `handle_event`, `tick`, `ingest_paper`,
/// `issue_ticket_field`, `outbound_stamp_cost`, `outbound_stamp_request` and
/// `set_outbound_stamp_result` all took `now_unix: f64`; a caller on a
/// clockless node passing `now_ms as f64 / 1000.0` compiled, ran, and issued
/// tickets every Python peer discards.
///
/// The rule is stated as "no `f64` parameter at all" rather than "no parameter
/// named `now_unix`", because the defect is the *shape* — a caller-supplied
/// wall clock — and renaming it would evade a name-based check. Every time
/// value the router still carries internally is a private parameter threaded
/// from one resolution per public operation; `now_ms: u64` stays, because
/// monotonic scheduling is what a timer is for.
///
/// The complementary half is positive: the crate must actually call the
/// producer. #182's own diagnosis was that `leviculum-lxmf` contained no call
/// to `emission_secs` or `wall_unix_secs` anywhere.
#[test]
fn router_public_surface_takes_no_wall_clock_parameter() {
    let sources = router_sources();
    for (name, source) in &sources {
        for (function, parameters) in public_parameter_lists(source) {
            assert!(
                !parameters.contains("f64"),
                "{name}: `pub fn {function}` takes an f64 parameter: {parameters}\n\
                 Wall-clock values are resolved from NodeCore::emission_secs \
                 (Codeberg #182, docs/src/concepts/time-and-clocks.md)."
            );
        }
    }

    let router = &sources
        .iter()
        .find(|(name, _)| *name == "router.rs")
        .expect("router.rs")
        .1;
    // `emission_secs_f64` is the same producer and the same source-priority
    // chain as `emission_secs`, resolved to milliseconds because the LXMF
    // message timestamp is hashed into the message ID and whole seconds
    // collide (Codeberg #217). Either spelling satisfies #182's positive half;
    // a bare `now_ms` or a caller's parameter satisfies neither.
    assert!(
        router.contains("node.emission_secs_f64()") || router.contains("node.emission_secs()"),
        "router.rs must resolve wall time from the one producer"
    );
    let call_sites: usize = sources
        .iter()
        .map(|(_, source)| source.matches("emission_secs(node)").count())
        .sum();
    assert!(
        call_sites >= 5,
        "every public entry point needing wall time must resolve it, found {call_sites}"
    );
}

/// A clockless node REFUSES to issue a ticket rather than issuing one the peer
/// silently discards.
///
/// Reference: the receiver keeps a ticket only while `time.time() < expires`
/// on ITS clock (`reference/LXMF/LXMF/LXMRouter.py:1854`, inside
/// `lxmf_delivery`). The check is silent — no reply, no error, nothing in the
/// peer's log naming us — and the consequence is that every reply from that
/// peer keeps paying full proof-of-work forever.
///
/// A node emitting uptime seconds produces `expires ≈ 1.8e6`, roughly January
/// 1970. Every Python peer drops it; two leviculum nodes with the same defect
/// accept each other's happily. Refusing turns that silence into
/// [`RouterError::NoWallClock`] at the one place that can still name the
/// cause. The refusal is scoped to *issuing*: it is not a filter on what we
/// accept, and it does not touch the message path, where the reference emits
/// `time.time()` unvalidated (`LXMessage.py:357`) and withholding a message
/// would be far worse than a mis-sorted one.
#[test]
fn clockless_node_refuses_to_issue_a_ticket_a_peer_would_discard() {
    let (mut router, mut node) = clockless_router();
    let remote = [0x61; 16];

    // Born clockless: emission_secs is uptime seconds, orders of magnitude
    // below the plausibility floor.
    assert!(!node.has_plausible_wall_clock());
    assert_eq!(node.emission_secs(), BOOT_MS / 1000);
    assert!(matches!(
        router.issue_ticket_field(&node, remote, &mut OsRng),
        Err(RouterError::NoWallClock)
    ));
    assert_eq!(
        router.tickets().inbound_secrets(&remote, 0.0).len(),
        0,
        "a refused issue must not leave ticket material behind"
    );

    // The same call succeeds once the node has a timebase, and the ticket it
    // then issues satisfies the peer's own acceptance rule.
    assert!(node.set_wall_time_unix_secs(INJECTED_UNIX, TimeSource::Host));
    assert!(node.has_plausible_wall_clock());
    let (field, _) = router
        .issue_ticket_field(&node, remote, &mut OsRng)
        .expect("a node with a timebase issues tickets");
    let ticket = Ticket::from_field_value(&field.expect("first issue is never throttled").1)
        .expect("ticket field value");
    assert!(
        (INJECTED_UNIX as f64) < ticket.expires_unix,
        "the peer's rule, applied to our own value: now < expires"
    );
}

/// The ticket expiry is `emission_secs + TICKET_EXPIRY`, from the node's
/// timebase and nothing else.
///
/// `INJECTED_UNIX` reaches the router only through
/// `NodeCore::set_wall_time_unix_secs` → `Transport::emission_secs`; it is not
/// an argument to any call below. The expected expiry is recomposed from the
/// reference expression `now + TICKET_EXPIRY` (`LXMRouter.py:1095`) rather
/// than read back from the store.
#[test]
fn issued_ticket_expiry_is_the_node_timebase_plus_the_reference_expiry() {
    let (mut router, mut node) = clockless_router();
    assert!(node.set_wall_time_unix_secs(INJECTED_UNIX, TimeSource::Host));

    let (field, _) = router
        .issue_ticket_field(&node, [0x62; 16], &mut OsRng)
        .expect("issue a ticket");
    let (key, value) = field.expect("first issue is never throttled");
    assert_eq!(key, FIELD_TICKET);
    let ticket = Ticket::from_field_value(&value).expect("ticket field value");

    assert_eq!(
        ticket.expires_unix,
        INJECTED_UNIX as f64 + 21.0 * 24.0 * 60.0 * 60.0,
        "expires = emission_secs + TICKET_EXPIRY"
    );
    assert_eq!(TICKET_EXPIRY, 21 * 24 * 60 * 60);

    // The monotonic clock is not the source: advancing uptime by an hour must
    // not move the expiry, and the timebase advance it does cause is carried
    // by emission_secs itself.
    assert_ne!(
        ticket.expires_unix,
        (BOOT_MS / 1000) as f64 + TICKET_EXPIRY as f64,
        "an uptime-derived expiry is exactly the #155 failure mode"
    );
}

/// The message timestamp is the node's timebase, written by the router rather
/// than supplied by the caller — and it is NOT refused when implausible.
///
/// Reference: `self.timestamp = time.time()` (`LXMessage.py:357`), unvalidated,
/// read back at :797 and displayed. No peer discards a message on it, so the
/// "We do not validate our own clock" rule of
/// `docs/src/concepts/time-and-clocks.md` applies: we emit what we have. The
/// contrast with the ticket above is the whole point — one field is discarded
/// by the peer and is worth refusing, the other is not.
#[test]
fn created_message_timestamp_is_the_node_timebase_and_is_never_withheld() {
    let (router, mut node) = clockless_router();

    // Clockless and unseeded: the message is still built, carrying uptime
    // seconds, exactly as the reference would carry a wrong `time.time()`.
    let uptime_stamped = router
        .create_message(
            &node,
            [0x63; 16],
            b"t".to_vec(),
            b"c".to_vec(),
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .expect("an implausible clock never withholds a message");
    assert_eq!(
        packed_timestamp(&uptime_stamped.pack()),
        (BOOT_MS / 1000) as f64
    );

    assert!(node.set_wall_time_unix_secs(INJECTED_UNIX, TimeSource::Host));
    let message = router
        .create_message(
            &node,
            [0x63; 16],
            b"t".to_vec(),
            b"c".to_vec(),
            Vec::new(),
            DeliveryMethod::Direct,
        )
        .expect("create message");
    assert_eq!(
        packed_timestamp(&message.pack()),
        INJECTED_UNIX as f64,
        "the wire timestamp is the node's timebase"
    );
    assert_eq!(
        message.source_hash,
        router.node().delivery_destination_hash().into_bytes()
    );
    assert_eq!(message.timestamp, packed_timestamp(&message.pack()));
}

/// The propagation upload envelope timestamp comes from the same producer.
///
/// Reference: the client packs `msgpack.packb([time.time(), [lxmf_data]])`
/// (`LXMessage.py:436`). No node-side decision depends on the value — both
/// ingest paths bind and drop it (`LXMRouter.py:2238-2240`, `:2344`) — which is
/// why it is not worth refusing on an implausible clock. It is still a
/// cross-lifetime field by construction and must not be able to come from a
/// caller's parameter, so it is pinned to the injected timebase here.
#[test]
fn prepared_propagation_upload_timebase_is_the_node_timebase() {
    let (mut router, mut node) = clockless_router();
    assert!(node.set_wall_time_unix_secs(INJECTED_UNIX, TimeSource::Host));

    // The propagation client shares the router's delivery identity.
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
        .set_outbound_propagation_node(&mut node, Some(DestinationHash::new([0x91; 16])))
        .expect("select a propagation node");

    // The recipient's identity must be known for the upload ciphertext.
    let recipient = identity_from(70);
    let recipient_destination =
        LxmfNode::delivery_destination(identity_copy(&recipient)).expect("recipient destination");
    let recipient_hash = *recipient_destination.hash();
    node.remember_identity(recipient_hash, recipient);

    let message = router
        .create_message(
            &node,
            recipient_hash.into_bytes(),
            b"t".to_vec(),
            b"propagated".to_vec(),
            Vec::new(),
            DeliveryMethod::Propagated,
        )
        .expect("create propagated message");
    let message_id = message.message_id;
    let _ = router
        .enqueue(&node, message)
        .expect("queue the propagated message");
    let _ = router.tick(&mut node).expect("prepare the upload");

    let prepared = router.outbound()[&message_id]
        .propagation()
        .expect("tick prepares the upload envelope");
    assert_eq!(
        prepared.timebase, INJECTED_UNIX as f64,
        "the upload timebase is the node's timebase, not a caller's parameter"
    );
}
