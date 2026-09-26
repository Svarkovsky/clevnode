//! mvr: characterization of the diamond relay-failure return path (Codeberg
//! #38).
//!
//! This mvr was written to reproduce a hypothesised gap and instead REFUTES it:
//! our stack already delivers the LRPROOF in the diamond failover scenario, so
//! the reactive-path-request fix proposed for #38 addresses a failure mode our
//! endpoint does not have. The test is kept as a GREEN regression guard on that
//! robustness. See the header analysis below and the #38 report.
//!
//! Field scenario (interop `test_diamond_relay_and_failure_recovery`, Phase 2):
//!
//! ```text
//!            x  (R1 dead)
//!   A ------|                          |------ B
//!            \------ R2 (relay) -------/
//! ```
//!
//! Phase 1 establishes both endpoints via R1: A learns a path to B via R1 and
//! B learns a path to A via R1. R1 then dies. Only one side re-announces in a
//! way that refreshes the OTHER side: A re-announces, so B (via R2) learns a
//! fresh path to A, and R2 learns a path to A. But nothing re-announces B to A,
//! so A keeps its stale `B -> R1` path (R1 is dead).
//!
//! B now opens a link to A. The LinkRequest routes B -> R2 -> A and reaches A.
//! A must send the LRPROOF back to B.
//!
//! Hypothesised gap (from Codeberg #38 / commit `ea1cc46`): A routes the LRPROOF
//! by a path-table lookup for the sender, hits its stale dead-R1 entry, and the
//! proof never reaches B.
//!
//! Measured reality (asserted below): A sends the LRPROOF on the responder
//! link's ATTACHED interface (the one the request arrived on, i.e. toward the
//! live R2), NOT via a path-table lookup for the sender. A emits NOTHING toward
//! the stale dead-R1 next hop. R2 reverse-routes the proof to B from its own
//! link table (built when it forwarded the request), and the link establishes.
//! A's stale path to B is never consulted, so it cannot break the return path.
//! This matches Python, whose responder also proves on `attached_interface`
//! (`Link.py` `prove()` after `attached_interface = packet.receiving_interface`;
//! `Transport.outbound` broadcasts a LINK packet only on that interface).
//!
//! Sans-I/O: no LoRa, no Docker, no Python, sub-second wall clock. Per-packet
//! delivery is scripted, so "R1 is dead" is simply declining to route anything
//! through R1 in Phase 2.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId, TickOutput};

// Tracing capture routes through the shared global-subscriber helper so a
// callsite registered by an earlier parallel test cannot hide events.
use crate::test_log_capture::with_captured_logs;

// ----------------------------------------------------------------------------
// Sans-I/O node helpers
// ----------------------------------------------------------------------------

type Node = NodeCore<OsRng, MockClock, MemoryStorage>;

fn add_iface(node: &mut Node, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

/// Build a transport-enabled endpoint node owning one link-accepting Single
/// destination. Returns (node, dest_hash, verifying_key).
fn make_endpoint(app: &str, aspect: &'static str) -> (Node, crate::DestinationHash, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let verifying_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        clock,
        MemoryStorage::with_defaults(),
    );

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        app,
        &[aspect],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    (node, dest_hash, verifying_key)
}

fn make_relay() -> Node {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        clock,
        MemoryStorage::with_defaults(),
    )
}

/// All (iface, bytes) an output wants to put on the wire this step.
fn actions_on(output: &TickOutput) -> Vec<(Option<usize>, Vec<u8>)> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::SendPacket { iface, data, .. } => (Some(iface.0), data.clone()),
            Action::Broadcast { data, .. } => (None, data.clone()),
        })
        .collect()
}

fn link_established(output: &TickOutput) -> bool {
    output
        .events
        .iter()
        .any(|e| matches!(e, NodeEvent::LinkEstablished { .. }))
}

/// Feed an announce into a relay on `in_iface`, then advance the relay's clock
/// past the rebroadcast delay window and collect the forwarded announce bytes.
/// A transport relay does not re-emit an announce inline; it schedules the
/// rebroadcast, which surfaces on the next `handle_timeout()`.
fn forward_announce(relay: &mut Node, in_iface: usize, raw: &[u8]) -> Vec<Vec<u8>> {
    let _ = relay.handle_packet(InterfaceId(in_iface), raw);
    let now = relay.transport().clock().now_ms();
    relay.transport().clock().set(now + 100_000);
    let out = relay.handle_timeout();
    actions_on(&out).into_iter().map(|(_, d)| d).collect()
}

// ----------------------------------------------------------------------------
// Scenario
// ----------------------------------------------------------------------------

struct Outcome {
    b_established: bool,
    /// Interface indices on A that the LRPROOF step emitted packets on
    /// (`None` = broadcast on all interfaces).
    a_proof_ifaces: Vec<Option<usize>>,
    /// A's interface index toward the (live) R2 relay.
    a_r2_index: usize,
    /// A's interface index toward the (dead) R1 relay.
    a_r1_index: usize,
    logs: String,
}

/// Interface index map (fixed order of `add_iface` calls per node):
///   A: 0 = A->R1, 1 = A->R2
///   B: 0 = B->R1, 1 = B->R2
///   R1: 0 = R1->A, 1 = R1->B
///   R2: 0 = R2->A, 1 = R2->B
fn run_scenario() -> Outcome {
    let mut b_established = false;
    let mut a_proof_ifaces: Vec<Option<usize>> = Vec::new();

    let ((), logs) = with_captured_logs(|| {
        let (mut a, dest_a, key_a) = make_endpoint("diamond", "a");
        let (mut b, dest_b, _key_b) = make_endpoint("diamond", "b");
        let mut r1 = make_relay();
        let mut r2 = make_relay();

        let a_r1 = add_iface(&mut a, "A->R1");
        let a_r2 = add_iface(&mut a, "A->R2");
        let b_r1 = add_iface(&mut b, "B->R1");
        let b_r2 = add_iface(&mut b, "B->R2");
        let r1_a = add_iface(&mut r1, "R1->A");
        let r1_b = add_iface(&mut r1, "R1->B");
        let r2_a = add_iface(&mut r2, "R2->A");
        let r2_b = add_iface(&mut r2, "R2->B");

        // --- Phase 1: both endpoints reachable via R1 -----------------------

        // B announces dest_b; R1 forwards it to A. A learns B via R1 (2 hops).
        let out = b.announce_destination(&dest_b, Some(b"B")).unwrap();
        for (_iface, raw) in actions_on(&out) {
            for fwd in forward_announce(&mut r1, r1_b, &raw) {
                let _ = a.handle_packet(InterfaceId(a_r1), &fwd);
            }
        }

        // A announces dest_a; R1 forwards it to B. B learns A via R1 (2 hops).
        let out = a.announce_destination(&dest_a, Some(b"A")).unwrap();
        for (_iface, raw) in actions_on(&out) {
            for fwd in forward_announce(&mut r1, r1_a, &raw) {
                let _ = b.handle_packet(InterfaceId(b_r1), &fwd);
            }
        }

        assert!(a.has_path(&dest_b), "Phase 1: A must learn a path to B");
        assert!(b.has_path(&dest_a), "Phase 1: B must learn a path to A");
        assert_eq!(a.hops_to(&dest_b), Some(2), "A's B-path is 2 hops via R1");

        // Real time passes between Phase 1 and the failover (R1's TCP death is
        // detected, endpoints re-announce). Advance the endpoint clocks so the
        // Phase-2 re-announce is not rejected as a burst duplicate of the
        // Phase-1 announce (ingress rate control), and counts as fresher. Stay
        // well under the ~15 s link establishment timeout.
        let t2 = TEST_TIME_MS + 10_000;
        a.transport().clock().set(t2);
        b.transport().clock().set(t2);

        // --- Phase 2: R1 dies; only A re-announces (refreshes B and R2) -----
        // Nothing re-announces B to A, so A keeps its stale `B -> R1` path.

        let out = a.announce_destination(&dest_a, Some(b"A2")).unwrap();
        for (_iface, raw) in actions_on(&out) {
            // Fresh announce reaches R2 (R1 is dead). R2 learns A directly and
            // forwards to B, so B learns a fresh A-path via R2.
            for fwd in forward_announce(&mut r2, r2_a, &raw) {
                let _ = b.handle_packet(InterfaceId(b_r2), &fwd);
            }
        }

        assert!(r2.has_path(&dest_a), "Phase 2: R2 must learn a path to A");
        assert!(
            a.has_path(&dest_b),
            "Phase 2: A still holds its (stale) B-path"
        );
        assert_eq!(
            a.hops_to(&dest_b),
            Some(2),
            "A's B-path is still the stale 2-hop R1 route"
        );

        // --- Phase 2: B opens a link to A via R2 ----------------------------

        let (_link, _routed, out) = b.connect(dest_a, &key_a);
        // The LinkRequest routes toward A via R2.
        let requests = actions_on(&out);
        assert_eq!(requests.len(), 1, "connect emits one link request");
        let (_ri, request) = requests.into_iter().next().unwrap();

        // R2 forwards the request to A (R1 is dead and not consulted).
        let out = r2.handle_packet(InterfaceId(r2_b), &request);
        let fwds = actions_on(&out);
        assert_eq!(fwds.len(), 1, "R2 forwards the link request to A");
        let (_fi, r2_fwd) = fwds.into_iter().next().unwrap();

        // A accepts the link and emits the LRPROOF.
        let out = a.handle_packet(InterfaceId(a_r2), &r2_fwd);
        let proof_actions = actions_on(&out);
        a_proof_ifaces = proof_actions.iter().map(|(i, _)| *i).collect();

        // Deliver whatever A emitted through the LIVE relay (R2) only; R1 is
        // dead. Then hand anything R2 relays to B and see if it establishes.
        for (iface, pkt) in proof_actions {
            // A's live interface toward R2 is `a_r2`. A broadcast (None) also
            // reaches R2. Anything A sent only toward R1 (`a_r1`) is lost.
            let reaches_r2 = iface.is_none() || iface == Some(a_r2);
            if !reaches_r2 {
                continue;
            }
            let out = r2.handle_packet(InterfaceId(r2_a), &pkt);
            for (_i, relayed) in actions_on(&out) {
                let out = b.handle_packet(InterfaceId(b_r2), &relayed);
                if link_established(&out) {
                    b_established = true;
                }
            }
        }
    });

    Outcome {
        b_established,
        a_proof_ifaces,
        a_r2_index: 1,
        a_r1_index: 0,
        logs,
    }
}

// ----------------------------------------------------------------------------
// GREEN characterization: the link establishes because A proves on the attached
// interface, never consulting its stale return path.
// ----------------------------------------------------------------------------

/// The diamond return path already works in our stack. A sends the LRPROOF on
/// the responder link's attached interface (toward the live R2), the relay
/// reverse-routes it to B, and the link establishes. A never emits toward its
/// stale dead-R1 next hop, so the stale path cannot break the return path.
///
/// This refutes the #38 hypothesis that A would route the proof by a path-table
/// lookup for the sender and hit its dead entry: no such lookup happens.
#[test]
fn diamond_lrproof_return_path_establishes_via_attached_interface() {
    let o = run_scenario();

    assert!(
        o.b_established,
        "link did NOT establish: the LRPROOF never reached B. \
         a_proof_ifaces={:?}.\n--- logs ---\n{}",
        o.a_proof_ifaces, o.logs
    );

    // Mechanism: every proof packet went out A's attached interface (toward the
    // live R2). None went toward the stale dead-R1 next hop, and none was a
    // path-routed broadcast. This is what makes the stale B-path irrelevant.
    assert!(
        !o.a_proof_ifaces.is_empty(),
        "A emitted no LRPROOF at all.\n--- logs ---\n{}",
        o.logs
    );
    for iface in &o.a_proof_ifaces {
        assert_eq!(
            *iface,
            Some(o.a_r2_index),
            "A's LRPROOF must go out the attached interface (toward live R2), \
             not toward the dead R1 (idx {}) nor as a path-routed broadcast. \
             a_proof_ifaces={:?}.\n--- logs ---\n{}",
            o.a_r1_index,
            o.a_proof_ifaces,
            o.logs
        );
    }

    // Guard the routing decision at the log level too: the proof send names the
    // attached interface, and nothing is dropped for a missing path.
    assert!(
        o.logs.contains("event=\"PROOF_SEND\""),
        "expected the endpoint PROOF_SEND instrumentation.\n--- logs ---\n{}",
        o.logs
    );
    assert!(
        !o.logs.contains("no known path"),
        "A must not have dropped the proof for a missing path.\n--- logs ---\n{}",
        o.logs
    );
}
