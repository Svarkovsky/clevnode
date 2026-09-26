//! Codeberg #217, on a real wall clock: two identical messages created back to
//! back are two messages.
//!
//! The unit test in `leviculum-lxmf/tests/subsecond_message_timestamps.rs`
//! drives a synthetic clock, which proves the router reads the timebase but
//! says nothing about whether the *platform* clock resolves finely enough. This
//! one runs the scenario the issue actually reports — "repeated short responses
//! such as OK", a scripted client in a loop — against a `SystemClock`-shaped
//! clock, with no sleeps and nothing to slow the loop down.
//!
//! It is also the measurement that chose the unit. Two consecutive
//! `create_message` calls take ~115 µs (each signs an Ed25519 message), so at
//! whole seconds every pair collides and at milliseconds every pair still
//! collides — 20 out of 20, measured. Microseconds separate them.

use leviculum_core::traits::Clock;
use leviculum_core::{Identity, MemoryStorage, NodeCore, NodeCoreBuilder};
use leviculum_lxmf::router::{LxmfRouter, RouterConfig, RouterError};
use leviculum_lxmf::{DeliveryMethod, LxmfNode, LxmfNodeConfig};
use rand_core::OsRng;

/// `leviculum_std::clock::SystemClock` is crate-private to construct, so this
/// mirrors it exactly: the same `SystemTime` readings at the same three
/// resolutions. If `SystemClock` changes, this test is the reason to change it
/// here too.
struct SystemClockShape;

impl Clock for SystemClockShape {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as u64)
            .unwrap_or(0)
    }
    fn wall_unix_secs(&self) -> Option<u64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| since.as_secs())
    }
    fn wall_unix_micros(&self) -> Option<u64> {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|since| u64::try_from(since.as_micros()).ok())
    }
}

type TestNode = NodeCore<OsRng, SystemClockShape, MemoryStorage>;

fn wall_clock_router() -> (LxmfRouter, TestNode) {
    let mut core =
        NodeCoreBuilder::new().build(OsRng, SystemClockShape, MemoryStorage::with_defaults());
    let identity = Identity::generate(&mut OsRng);
    let identity_hash = *identity.hash();
    let destination = LxmfNode::delivery_destination(identity).expect("delivery destination");
    let node = LxmfNode::register(&mut core, destination, LxmfNodeConfig::default())
        .expect("register delivery destination");
    (
        LxmfRouter::new(node, identity_hash, RouterConfig::default()),
        core,
    )
}

/// Twenty pairs of identical "OK" replies, each pair created as fast as the
/// code can create them. Every message must be its own message.
#[test]
fn rapid_identical_replies_are_never_refused_as_duplicates() {
    let (mut router, node) = wall_clock_router();
    let recipient = [0x71; 16];

    let mut queued = 0usize;
    for _ in 0..20 {
        for _ in 0..2 {
            let message = router
                .create_message(
                    &node,
                    recipient,
                    b"Re: status".to_vec(),
                    b"OK".to_vec(),
                    Vec::new(),
                    DeliveryMethod::Direct,
                )
                .expect("create_message");
            match router.enqueue(&node, message) {
                Ok(_) => queued += 1,
                Err(RouterError::Duplicate) => panic!(
                    "a distinct reply was refused as a duplicate after {queued} queued: the \
                     wall clock does not resolve two consecutive create_message calls \
                     (~115 µs apart)"
                ),
                Err(error) => panic!("unexpected enqueue error: {error:?}"),
            }
        }
    }

    assert_eq!(queued, 40, "every reply is its own outbound message");
    assert_eq!(router.outbound().len(), 40);

    // The timestamps must be strictly increasing, not merely distinct: a value
    // that jitters would produce distinct IDs while telling the recipient the
    // replies arrived out of order.
    let mut timestamps: Vec<f64> = router
        .outbound()
        .values()
        .map(|entry| entry.message().timestamp)
        .collect();
    timestamps.sort_by(|a, b| a.partial_cmp(b).expect("finite timestamps"));
    for pair in timestamps.windows(2) {
        assert!(
            pair[1] > pair[0],
            "timestamps must be distinct and ordered: {pair:?}"
        );
    }
    assert!(
        timestamps[0] > 1_700_000_000.0,
        "the timestamps are unix seconds, not uptime"
    );
    assert!(
        timestamps[timestamps.len() - 1] - timestamps[0] < 1.0,
        "the whole run fits inside one second, which is what makes this a #217 test"
    );
}
