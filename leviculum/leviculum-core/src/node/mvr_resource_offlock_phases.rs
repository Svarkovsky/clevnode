//! mvr: PR #152 (leviculum#29 stage 1) — the phased resource send
//! (params → prepare → commit) must survive everything that can change on the
//! link while the CPU-heavy build runs off-lock, and the phased path must be
//! byte-identical to the composed single-call `send_resource`.
//!
//! Race guards (the build runs with NO node borrow, so all of these can
//! happen between `resource_send_params` and `commit_resource_send`):
//!  - re-key: commit must refuse stale ciphertext with the retryable
//!    `LinkStateChanged`; one rebuild against fresh params must deliver
//!    end-to-end (the std driver's retry loop rebuilds exactly once).
//!  - teardown: commit must fail cleanly with `InvalidRequest`, leave no
//!    half-installed transfer, and a fresh link must work unimpeded.
//!  - competing transfer: commit must refuse with `TransferInProgress` and
//!    must not disturb the transfer that won the race.
//!
//! Byte equivalence: two node pairs with identical deterministic RNGs and
//! clocks, one sending via composed `send_resource`, the other via the three
//! phases, must emit an identical wire byte stream (advertisement, parts,
//! proofs — single-segment and split).

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::{CryptoRng, OsRng, RngCore};

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::LinkId;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::resource::{
    prepare_resource_send, ResourceError, ResourceStrategy, RESOURCE_MAX_EFFICIENT_SIZE,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::NoStorage;
use crate::transport::{Action, InterfaceId, TickOutput};

type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

fn add_iface<R: rand_core::CryptoRngCore>(
    node: &mut NodeCore<R, MockClock, NoStorage>,
    name: &'static str,
) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn action_data(output: &TickOutput) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .map(|a| match a {
            Action::Broadcast { data, .. } | Action::SendPacket { data, .. } => data.clone(),
        })
        .collect()
}

fn make_responder() -> (EndpointNode, crate::DestinationHash, [u8; 32]) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["offlock"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    (node, dest_hash, signing_key)
}

fn make_initiator() -> EndpointNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().build(OsRng, clock, NoStorage)
}

/// Drive a clean initiator <-> responder link to Active on both sides.
fn establish() -> (EndpointNode, EndpointNode, usize, usize, LinkId) {
    let (mut responder, dest_hash, signing_key) = make_responder();
    let mut initiator = make_initiator();
    let r_iface = add_iface(&mut responder, "R_mesh");
    let i_iface = add_iface(&mut initiator, "I_mesh");

    let (caller_link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);

    let mut for_responder = action_data(&out);
    for _ in 0..8 {
        if for_responder.is_empty() {
            break;
        }
        let mut back = Vec::new();
        for pkt in for_responder.drain(..) {
            back.extend(action_data(
                &responder.handle_packet(InterfaceId(r_iface), &pkt),
            ));
        }
        for pkt in back {
            for_responder.extend(action_data(
                &initiator.handle_packet(InterfaceId(i_iface), &pkt),
            ));
        }
    }

    assert_eq!(initiator.active_link_count(), 1, "initiator link active");
    assert_eq!(responder.active_link_count(), 1, "responder link active");
    responder
        .set_resource_strategy(&caller_link_id, ResourceStrategy::AcceptAll)
        .expect("set AcceptAll on responder link");
    (initiator, responder, i_iface, r_iface, caller_link_id)
}

/// Position-dependent test payload.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Bounce packets between the two nodes until nothing is in flight; collect
/// the receiver-side completed data and any failures.
fn drain_transfer(
    initiator: &mut EndpointNode,
    responder: &mut EndpointNode,
    i_iface: usize,
    r_iface: usize,
    first_out: TickOutput,
) -> (Vec<u8>, Vec<(bool, ResourceError)>) {
    let mut received = Vec::new();
    let mut failures = Vec::new();
    let mut absorb = |events: Vec<NodeEvent>, received: &mut Vec<u8>| {
        for e in events {
            match e {
                NodeEvent::ResourceCompleted {
                    is_sender: false,
                    data,
                    ..
                } => received.extend_from_slice(&data),
                NodeEvent::ResourceFailed {
                    is_sender, error, ..
                } => failures.push((is_sender, error)),
                _ => {}
            }
        }
    };

    let mut to_responder = action_data(&first_out);
    absorb(first_out.events, &mut received);
    for _ in 0..20_000 {
        if to_responder.is_empty() {
            break;
        }
        let mut from_responder = Vec::new();
        for pkt in to_responder.drain(..) {
            let o = responder.handle_packet(InterfaceId(r_iface), &pkt);
            from_responder.extend(action_data(&o));
            absorb(o.events, &mut received);
        }
        for pkt in from_responder {
            let o = initiator.handle_packet(InterfaceId(i_iface), &pkt);
            to_responder.extend(action_data(&o));
            absorb(o.events, &mut received);
        }
    }
    (received, failures)
}

/// (b) Re-key between prepare and commit: commit refuses the stale ciphertext
/// with the retryable `LinkStateChanged`, exactly one rebuild (mirroring the
/// std driver's single-retry loop) succeeds, and the receiver decrypts the
/// delivered transfer end-to-end under the rotated key.
#[test]
fn rekey_between_prepare_and_commit_is_refused_then_one_rebuild_delivers() {
    let (mut initiator, mut responder, i_iface, r_iface, link_id) = establish();
    let data = pattern(4096);

    // Phase 1 + 2 under the original token key K1.
    let params = initiator
        .resource_send_params(&link_id)
        .expect("params under K1");
    let prepared = prepare_resource_send(&params, &data, None, true, &mut OsRng).expect("prepare");

    // The link re-keys while the build runs off-lock. Rotate BOTH ends to the
    // same fresh key (as a real re-establishment would) so the rebuilt
    // transfer is decryptable.
    let k2 = [0xC3u8; 64];
    initiator
        .link_mut(&link_id)
        .expect("initiator link")
        .set_link_key_for_test(k2);
    responder
        .link_mut(&link_id)
        .expect("responder link")
        .set_link_key_for_test(k2);

    // Commit must refuse: the ciphertext was built under K1.
    let err = initiator
        .commit_resource_send(prepared)
        .expect_err("commit of a stale-key build must be refused");
    assert_eq!(
        err,
        ResourceError::LinkStateChanged,
        "the refusal must be the retryable LinkStateChanged"
    );
    // No half-installed transfer.
    assert!(
        !initiator
            .link(&link_id)
            .expect("link still present")
            .has_outgoing_resource(),
        "refused commit must not leave a half-installed transfer"
    );

    // Exactly one rebuild against fresh params (the std driver's retry loop).
    let params = initiator
        .resource_send_params(&link_id)
        .expect("fresh params under K2");
    let prepared = prepare_resource_send(&params, &data, None, true, &mut OsRng).expect("rebuild");
    let (_hash, out) = initiator
        .commit_resource_send(prepared)
        .expect("commit of the rebuilt transfer must succeed");

    let (received, failures) =
        drain_transfer(&mut initiator, &mut responder, i_iface, r_iface, out);
    assert!(failures.is_empty(), "no failures: {failures:?}");
    assert_eq!(
        received, data,
        "receiver must decrypt the rebuilt transfer end-to-end"
    );
}

/// (c) Teardown between prepare and commit: clean `InvalidRequest`, no
/// half-installed state, and the registry stays consistent — a fresh link on
/// the same pair carries a full transfer afterwards.
#[test]
fn teardown_between_prepare_and_commit_fails_clean() {
    let (mut initiator, mut responder, i_iface, r_iface, link_id) = establish();
    let data = pattern(4096);

    let params = initiator.resource_send_params(&link_id).expect("params");
    let prepared = prepare_resource_send(&params, &data, None, true, &mut OsRng).expect("prepare");

    // The link closes while the build runs off-lock.
    let close_out = initiator.close_link(&link_id);
    for pkt in action_data(&close_out) {
        let _ = responder.handle_packet(InterfaceId(r_iface), &pkt);
    }
    assert_eq!(initiator.active_link_count(), 0, "link closed");

    let err = initiator
        .commit_resource_send(prepared)
        .expect_err("commit onto a closed link must fail");
    assert_eq!(err, ResourceError::InvalidRequest, "clean error");
    assert!(
        initiator.link(&link_id).is_none(),
        "registry consistent: the closed link must not resurrect"
    );

    // The pair is not poisoned: a fresh link carries a full transfer.
    let (mut initiator2, mut responder2, i_iface2, r_iface2, link_id2) = establish();
    let (_hash, out) = initiator2
        .send_resource(&link_id2, &data, None, true)
        .expect("fresh link transfer");
    let (received, failures) =
        drain_transfer(&mut initiator2, &mut responder2, i_iface2, r_iface2, out);
    assert!(failures.is_empty(), "no failures: {failures:?}");
    assert_eq!(received, data);
    let _ = (i_iface, initiator, responder);
}

/// (d) A second transfer wins the race between prepare and commit: commit
/// must refuse with `TransferInProgress` per its re-validation, and the
/// transfer that won must complete untouched.
#[test]
fn competing_transfer_between_prepare_and_commit_is_refused() {
    let (mut initiator, mut responder, i_iface, r_iface, link_id) = establish();
    let data_a = pattern(4096);
    let data_b: Vec<u8> = pattern(5000).iter().map(|b| b ^ 0xFF).collect();

    let params = initiator.resource_send_params(&link_id).expect("params");
    let prepared_a =
        prepare_resource_send(&params, &data_a, None, true, &mut OsRng).expect("prepare A");

    // B races in through the composed path while A's build runs off-lock.
    let (_hash_b, out_b) = initiator
        .send_resource(&link_id, &data_b, None, true)
        .expect("B wins the race");

    let err = initiator
        .commit_resource_send(prepared_a)
        .expect_err("commit of A must be refused while B is in flight");
    assert_eq!(err, ResourceError::TransferInProgress, "re-validation");

    // B completes untouched and delivers exactly B's bytes.
    let (received, failures) =
        drain_transfer(&mut initiator, &mut responder, i_iface, r_iface, out_b);
    assert!(failures.is_empty(), "no failures: {failures:?}");
    assert_eq!(received, data_b, "the winning transfer must be undisturbed");
}

// ---------------------------------------------------------------------------
// (e) Byte equivalence of the composed and the phased path.
// ---------------------------------------------------------------------------

/// Deterministic RNG (SplitMix64) so two node pairs evolve in lockstep.
/// Not cryptographically secure — test-only.
struct DetRng(u64);

impl RngCore for DetRng {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
impl CryptoRng for DetRng {}

type DetNode = NodeCore<DetRng, MockClock, NoStorage>;

fn make_det_pair(
    initiator_seed: u64,
    responder_seed: u64,
) -> (DetNode, DetNode, usize, usize, LinkId) {
    let mut id_rng = DetRng(responder_seed ^ 0xA5A5_A5A5);
    let identity = Identity::generate(&mut id_rng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let mut responder = NodeCoreBuilder::new().build(
        DetRng(responder_seed),
        MockClock::new(TEST_TIME_MS),
        NoStorage,
    );
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["offlock"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    responder.register_destination(dest);

    let mut initiator = NodeCoreBuilder::new().build(
        DetRng(initiator_seed),
        MockClock::new(TEST_TIME_MS),
        NoStorage,
    );
    let r_iface = add_iface(&mut responder, "R_mesh");
    let i_iface = add_iface(&mut initiator, "I_mesh");

    let (caller_link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
    let mut for_responder = action_data(&out);
    for _ in 0..8 {
        if for_responder.is_empty() {
            break;
        }
        let mut back = Vec::new();
        for pkt in for_responder.drain(..) {
            back.extend(action_data(
                &responder.handle_packet(InterfaceId(r_iface), &pkt),
            ));
        }
        for pkt in back {
            for_responder.extend(action_data(
                &initiator.handle_packet(InterfaceId(i_iface), &pkt),
            ));
        }
    }
    assert_eq!(initiator.active_link_count(), 1);
    assert_eq!(responder.active_link_count(), 1);
    responder
        .set_resource_strategy(&caller_link_id, ResourceStrategy::AcceptAll)
        .expect("AcceptAll");
    (initiator, responder, i_iface, r_iface, caller_link_id)
}

/// Compare the COMPLETE wire byte streams of a composed-path transfer and a
/// phased-path transfer on two lockstep-deterministic node pairs.
fn assert_wire_equivalence(data: &[u8], metadata: Option<&[u8]>) {
    let (mut i1, mut r1, ii1, ri1, l1) = make_det_pair(7, 99);
    let (mut i2, mut r2, ii2, ri2, l2) = make_det_pair(7, 99);
    assert_eq!(
        l1, l2,
        "deterministic pairs must produce identical link ids (test harness sanity)"
    );

    // Pair 1: composed single-call path.
    let (hash1, out1) = i1
        .send_resource(&l1, data, metadata, true)
        .expect("composed send");

    // Pair 2: the three phases, exactly as the std driver runs them — the
    // build consumes the node's own RNG stream so the pairs stay in lockstep.
    let params = i2.resource_send_params(&l2).expect("params");
    let prepared =
        prepare_resource_send(&params, data, metadata, true, &mut i2.rng).expect("prepare");
    let (hash2, out2) = i2.commit_resource_send(prepared).expect("commit");

    assert_eq!(hash1, hash2, "resource hash must be identical");
    let mut fly1 = action_data(&out1);
    let mut fly2 = action_data(&out2);
    assert_eq!(fly1, fly2, "advertisement bytes must be identical");

    // Lockstep ping-pong: every packet both directions must stay identical.
    let mut done1 = Vec::new();
    let mut done2 = Vec::new();
    let absorb = |events: Vec<NodeEvent>, done: &mut Vec<u8>| {
        for e in events {
            if let NodeEvent::ResourceCompleted {
                is_sender: false,
                data,
                ..
            } = e
            {
                done.extend_from_slice(&data);
            }
        }
    };
    for round in 0..20_000 {
        if fly1.is_empty() && fly2.is_empty() {
            break;
        }
        assert_eq!(
            fly1.len(),
            fly2.len(),
            "round {round}: in-flight packet counts diverged"
        );
        let mut back1 = Vec::new();
        let mut back2 = Vec::new();
        for (p1, p2) in fly1.drain(..).zip(fly2.drain(..)) {
            assert_eq!(p1, p2, "round {round}: initiator->responder bytes diverged");
            let o1 = r1.handle_packet(InterfaceId(ri1), &p1);
            let o2 = r2.handle_packet(InterfaceId(ri2), &p2);
            back1.extend(action_data(&o1));
            back2.extend(action_data(&o2));
            absorb(o1.events, &mut done1);
            absorb(o2.events, &mut done2);
        }
        assert_eq!(back1, back2, "round {round}: responder->initiator diverged");
        for (p1, p2) in back1.into_iter().zip(back2) {
            assert_eq!(p1, p2, "round {round}: responder->initiator bytes diverged");
            let o1 = i1.handle_packet(InterfaceId(ii1), &p1);
            let o2 = i2.handle_packet(InterfaceId(ii2), &p2);
            fly1.extend(action_data(&o1));
            fly2.extend(action_data(&o2));
        }
    }
    assert_eq!(done1, data, "composed path must deliver the payload");
    assert_eq!(done2, data, "phased path must deliver the payload");
}

/// (e) Single-segment: advertisement and all parts byte-identical between the
/// composed and the phased path.
#[test]
fn composed_and_phased_paths_are_byte_identical_single_segment() {
    assert_wire_equivalence(&pattern(40_000), Some(b"meta"));
}

/// (e) Split transfer: segment plans, per-segment advertisements and parts
/// byte-identical between the composed and the phased path.
#[test]
fn composed_and_phased_paths_are_byte_identical_split() {
    assert_wire_equivalence(&pattern(RESOURCE_MAX_EFFICIENT_SIZE + 4096), None);
}

// ---------------------------------------------------------------------------
// (f) The activity clock belongs to the commit, not to the capture (S4).
// ---------------------------------------------------------------------------

/// The advertisement timeout this prepared build will run under, read off the
/// built resource itself (it derives from the link RTT, so it must not be
/// hardcoded) and expressed relative to the capture timestamp.
fn adv_timeout_of(prepared: &crate::resource::outgoing::PreparedResourceSend, rtt_ms: u64) -> u64 {
    use crate::resource::outgoing::PreparedSendKind;
    let res = match &prepared.kind {
        PreparedSendKind::Single(res) => res,
        PreparedSendKind::Split { segment1, .. } => segment1,
    };
    res.next_deadline(rtt_ms)
        .expect("an advertised resource has a deadline")
        .saturating_sub(res.last_activity_ms())
}

/// Run one phased send that stalls `stall_ms` between capture and commit, poll
/// once a millisecond after the commit, and return
/// `(advertisement timeout in effect, adv_retries after that poll, packets that
/// poll emitted, delivered bytes, failures)`.
fn phased_send_stalled_by(
    stall_ms: u64,
) -> (u64, usize, usize, Vec<u8>, Vec<(bool, ResourceError)>) {
    let (mut initiator, mut responder, i_iface, r_iface, link_id) = establish();
    let data = pattern(4096);

    let params = initiator.resource_send_params(&link_id).expect("params");
    let prepared = prepare_resource_send(&params, &data, None, true, &mut OsRng).expect("prepare");

    let rtt_ms = initiator.link(&link_id).expect("link").rtt_ms();
    let adv_timeout_ms = adv_timeout_of(&prepared, rtt_ms);

    // The build waits in the router's hand-out queue while time passes.
    initiator.transport().clock().advance(stall_ms);
    let (_hash, out) = initiator
        .commit_resource_send(prepared)
        .expect("commit of the stalled build");

    // The very first poll after the commit: one millisecond of the resource's
    // own life has elapsed, nothing else.
    initiator.transport().clock().advance(1);
    let poll_out = initiator.handle_timeout();
    let poll_packets = action_data(&poll_out).len();
    let adv_retries = initiator
        .link(&link_id)
        .expect("link still present")
        .outgoing_resource()
        .expect("the transfer is still installed")
        .adv_retries();

    let (received, failures) =
        drain_transfer(&mut initiator, &mut responder, i_iface, r_iface, out);
    (
        adv_timeout_ms,
        adv_retries,
        poll_packets,
        received,
        failures,
    )
}

/// (f) U14 / S4: a build handed back to the router and committed long after its
/// parameters were captured must start its advertisement clock at the commit.
/// Otherwise the resource is installed already older than its advertisement
/// timeout, and the first poll burns an advertisement retry before a byte has
/// moved — elapsed time is staleness no epoch can see.
///
/// Control arm: the same send with no stall must behave identically (it does
/// before and after the fix), so the assertions are not vacuous.
#[test]
fn a_build_committed_late_starts_its_advertisement_clock_at_commit() {
    let data = pattern(4096);

    // Control arm: capture and commit at the same instant.
    let (adv_timeout_ms, adv_retries, poll_packets, received, failures) = phased_send_stalled_by(0);
    assert_eq!(
        adv_retries, 0,
        "control: the first poll after a prompt commit must not charge a retry"
    );
    assert_eq!(
        poll_packets, 0,
        "control: the first poll after a prompt commit must not retransmit the advertisement"
    );
    assert!(failures.is_empty(), "control: no failures: {failures:?}");
    assert_eq!(received, data, "control: the transfer must proceed");

    // Late arm: the same send, committed well past the advertisement timeout
    // that the resource itself is running under.
    let stall_ms = adv_timeout_ms + 500;
    assert!(
        stall_ms > adv_timeout_ms,
        "the stall ({stall_ms} ms) must exceed the advertisement timeout in effect \
         ({adv_timeout_ms} ms) or the test proves nothing"
    );
    let (late_timeout_ms, adv_retries, poll_packets, received, failures) =
        phased_send_stalled_by(stall_ms);
    assert_eq!(
        late_timeout_ms, adv_timeout_ms,
        "both arms must run under the same advertisement timeout (test harness sanity)"
    );
    assert_eq!(
        adv_retries, 0,
        "a build committed {stall_ms} ms after capture must start its advertisement \
         clock at the commit, not charge a retry for time it spent in the queue"
    );
    assert_eq!(
        poll_packets, 0,
        "the first poll after a late commit must not retransmit the advertisement"
    );
    assert!(failures.is_empty(), "no failures: {failures:?}");
    assert_eq!(received, data, "the transfer must proceed");
}
