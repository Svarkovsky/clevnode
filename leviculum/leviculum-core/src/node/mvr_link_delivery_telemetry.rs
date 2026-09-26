//! mvr: PR #154 (leviculum#35) — the per-link delivery telemetry must be
//! *accountable*, not merely nonzero:
//!
//!  - accounting identity: after a mixed workload of proofed channel sends
//!    and a completed resource transfer, `bytes_delivered` equals the exact
//!    sum of envelope packed sizes (header + payload) and the resource's
//!    transfer size at proof time — computed here from first principles
//!    (envelope header constant, token-encryption layout), not read back
//!    from the counters under test.
//!  - backpressure discrimination: window-full rejections surface as
//!    `busy_rejections` on the congested link while an app-limited idle link
//!    stays at zero — the floor-vs-ceiling bit a delivery-rate consumer
//!    needs.
//!  - Karn gating: a retransmitted exchange must not feed `min_rtt_ms`; only
//!    clean (first-try) samples lower the floor.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::CHANNEL_ENVELOPE_HEADER_SIZE;
use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::{Link, LinkId};
use crate::node::send::SendError;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::resource::{ResourceStrategy, RESOURCE_RANDOM_HASH_SIZE};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, NoStorage};
use crate::transport::{Action, InterfaceId, TickOutput};

type EndpointNode = NodeCore<OsRng, MockClock, NoStorage>;

fn add_iface(node: &mut EndpointNode, name: &'static str) -> usize {
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

fn deliver_all(target: &mut EndpointNode, iface: usize, packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for pkt in packets {
        out.extend(action_data(&target.handle_packet(InterfaceId(iface), &pkt)));
    }
    out
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
        &["telemetry"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    node.register_destination(dest);
    (node, dest_hash, signing_key)
}

fn establish() -> (EndpointNode, EndpointNode, usize, usize, LinkId) {
    let (mut responder, dest_hash, signing_key) = make_responder();
    let mut initiator =
        NodeCoreBuilder::new().build(OsRng, MockClock::new(TEST_TIME_MS), NoStorage);
    let r_iface = add_iface(&mut responder, "R_mesh");
    let i_iface = add_iface(&mut initiator, "I_mesh");

    let (link_id, _routed, out) = initiator.connect(dest_hash, &signing_key);
    let mut for_responder = action_data(&out);
    for _ in 0..8 {
        if for_responder.is_empty() {
            break;
        }
        let back = deliver_all(&mut responder, r_iface, for_responder);
        for_responder = deliver_all(&mut initiator, i_iface, back);
    }
    assert_eq!(initiator.active_link_count(), 1, "initiator link active");
    assert_eq!(responder.active_link_count(), 1, "responder link active");
    (initiator, responder, i_iface, r_iface, link_id)
}

/// Send one channel payload and drive it to a delivered proof, advancing the
/// initiator clock by `rtt_advance_ms` between data-out and proof-in. Skips
/// past channel pacing beforehand.
fn proofed_channel_send(
    initiator: &mut EndpointNode,
    responder: &mut EndpointNode,
    i_iface: usize,
    r_iface: usize,
    link_id: &LinkId,
    payload: &[u8],
    rtt_advance_ms: u64,
) {
    let out = loop {
        match initiator.send_on_link(link_id, payload) {
            Ok(out) => break out,
            Err(SendError::PacingDelay { ready_at_ms }) => {
                let now = initiator.transport().clock().now_ms();
                initiator
                    .transport()
                    .clock()
                    .advance(ready_at_ms.saturating_sub(now) + 1);
            }
            Err(e) => panic!("unexpected send error: {e:?}"),
        }
    };
    let replies = deliver_all(responder, r_iface, action_data(&out));
    initiator.transport().clock().advance(rtt_advance_ms);
    let leftover = deliver_all(initiator, i_iface, replies);
    // Nothing should still be in flight for a single proofed envelope.
    let _ = deliver_all(responder, r_iface, leftover);
}

/// (b) Accounting identity: `bytes_delivered` equals the exact expected sum —
/// `CHANNEL_ENVELOPE_HEADER_SIZE + payload_len` per proofed channel send plus
/// the resource's transfer size at proof time, which for an uncompressed
/// transfer is the token-encryption size of `wire_random + data`.
#[test]
fn bytes_delivered_matches_the_exact_expected_sum() {
    let (mut initiator, mut responder, i_iface, r_iface, link_id) = establish();
    responder
        .set_resource_strategy(&link_id, ResourceStrategy::AcceptAll)
        .expect("AcceptAll");

    // Three channel sends of known sizes, each proofed.
    let payloads: [&[u8]; 3] = [&[0x11; 100], &[0x22; 200], &[0x33; 17]];
    let mut expected: u64 = 0;
    for p in payloads {
        proofed_channel_send(
            &mut initiator,
            &mut responder,
            i_iface,
            r_iface,
            &link_id,
            p,
            10,
        );
        expected += (CHANNEL_ENVELOPE_HEADER_SIZE + p.len()) as u64;
    }

    let channel_only = initiator
        .link_stats(&link_id)
        .expect("stats")
        .bytes_delivered();
    assert_eq!(
        channel_only, expected,
        "channel accounting must be the exact envelope packed-size sum"
    );

    // One resource transfer, auto_compress OFF so the transfer size is the
    // deterministic token-encryption size of wire_random + data (no metadata).
    let data = std::vec![0x5Au8; 4096];
    let (_hash, out) = initiator
        .send_resource(&link_id, &data, None, false)
        .expect("send_resource");
    expected += Link::encrypted_size(RESOURCE_RANDOM_HASH_SIZE + data.len()) as u64;

    let mut completed = false;
    let mut to_responder = action_data(&out);
    for _ in 0..10_000 {
        if to_responder.is_empty() {
            break;
        }
        let mut back = Vec::new();
        for pkt in to_responder.drain(..) {
            back.extend(action_data(
                &responder.handle_packet(InterfaceId(r_iface), &pkt),
            ));
        }
        for pkt in back {
            let o = initiator.handle_packet(InterfaceId(i_iface), &pkt);
            completed |= o.events.iter().any(|e| {
                matches!(
                    e,
                    NodeEvent::ResourceCompleted {
                        is_sender: true,
                        ..
                    }
                )
            });
            to_responder.extend(action_data(&o));
        }
    }
    assert!(completed, "the resource transfer must complete");

    let total = initiator
        .link_stats(&link_id)
        .expect("stats")
        .bytes_delivered();
    assert_eq!(
        total, expected,
        "bytes_delivered must equal envelope packed sizes plus the resource \
         transfer size at proof time — exactly"
    );
}

/// (c) Backpressure discrimination: driving the channel window full counts
/// `busy_rejections` (exactly as many as the Busy errors the app saw) on the
/// congested link, while an app-limited link that idles inside its window
/// stays at zero on every rejection counter.
#[test]
fn busy_rejections_discriminate_congested_from_app_limited() {
    // Congested pair: send without ever delivering proofs, hopping over the
    // pacer, until the window rejects.
    let (mut congested, _responder_c, _ii_c, _ri_c, link_c) = establish();
    let mut busy_seen = 0u64;
    for _ in 0..64 {
        match congested.send_on_link(&link_c, b"flood") {
            Ok(_) => {}
            Err(SendError::Busy) => {
                busy_seen += 1;
                if busy_seen == 3 {
                    break;
                }
            }
            Err(SendError::PacingDelay { ready_at_ms }) => {
                let now = congested.transport().clock().now_ms();
                congested
                    .transport()
                    .clock()
                    .advance(ready_at_ms.saturating_sub(now) + 1);
            }
            Err(e) => panic!("unexpected send error: {e:?}"),
        }
    }
    assert_eq!(busy_seen, 3, "the window must reject once it is full");

    let stats_c = congested.link_stats(&link_c).expect("stats");
    assert_eq!(
        stats_c.busy_rejections(),
        busy_seen,
        "every Busy the app saw must be counted, and nothing else"
    );
    assert_eq!(
        stats_c.iface_pacing_rejections(),
        0,
        "no interface gate in this harness"
    );

    // App-limited pair: one proofed send well inside the window, then idle.
    let (mut idle, mut responder_i, ii, ri, link_i) = establish();
    proofed_channel_send(
        &mut idle,
        &mut responder_i,
        ii,
        ri,
        &link_i,
        b"app-limited",
        10,
    );
    let stats_i = idle.link_stats(&link_i).expect("stats");
    assert!(stats_i.bytes_delivered() > 0, "the idle link did deliver");
    assert_eq!(
        stats_i.busy_rejections(),
        0,
        "an app-limited link must show ZERO busy rejections — this is the \
         floor-vs-ceiling discriminator"
    );
}

/// (d) Karn gating end-to-end: a retransmitted exchange must not feed
/// `min_rtt_ms`; only clean first-try samples lower the floor.
#[test]
fn min_rtt_ignores_retransmitted_exchanges() {
    let (mut initiator, mut responder, i_iface, r_iface, link_id) = establish();

    // Send one envelope and DROP it (never delivered). Advance time until the
    // channel retransmits.
    let out = initiator
        .send_on_link(&link_id, b"lost-then-retransmitted")
        .expect("send");
    drop(action_data(&out)); // the packet is lost

    let mut retransmit = Vec::new();
    for _ in 0..30 {
        initiator.transport().clock().advance(5_000);
        let out = initiator.handle_timeout();
        let saw_retransmit = out
            .events
            .iter()
            .any(|e| matches!(e, NodeEvent::ChannelRetransmit { .. }));
        let pkts = action_data(&out);
        if saw_retransmit {
            retransmit = pkts;
            break;
        }
    }
    assert!(
        !retransmit.is_empty(),
        "the channel must retransmit the lost envelope"
    );

    // Deliver the RETRANSMITTED copy and its proof, with a huge apparent RTT
    // contribution on the initiator clock.
    let replies = deliver_all(&mut responder, r_iface, retransmit);
    initiator.transport().clock().advance(500);
    let leftover = deliver_all(&mut initiator, i_iface, replies);
    let _ = deliver_all(&mut responder, r_iface, leftover);

    let stats = initiator.link_stats(&link_id).expect("stats");
    assert!(
        stats.bytes_delivered() > 0,
        "the retransmitted envelope WAS delivered (its bytes count)"
    );
    assert_eq!(
        stats.min_rtt_ms(),
        None,
        "a retransmitted exchange must not feed min_rtt (Karn)"
    );

    // A clean exchange seeds the floor with its real sample …
    proofed_channel_send(
        &mut initiator,
        &mut responder,
        i_iface,
        r_iface,
        &link_id,
        b"clean-40ms",
        40,
    );
    let stats = initiator.link_stats(&link_id).expect("stats");
    assert_eq!(
        stats.min_rtt_ms(),
        Some(40),
        "the first clean sample seeds the floor"
    );

    // … a slower clean exchange never raises it, a faster one lowers it.
    proofed_channel_send(
        &mut initiator,
        &mut responder,
        i_iface,
        r_iface,
        &link_id,
        b"clean-200ms",
        200,
    );
    assert_eq!(
        initiator.link_stats(&link_id).expect("stats").min_rtt_ms(),
        Some(40),
        "a slower clean sample must not raise the floor"
    );
    proofed_channel_send(
        &mut initiator,
        &mut responder,
        i_iface,
        r_iface,
        &link_id,
        b"clean-15ms",
        15,
    );
    assert_eq!(
        initiator.link_stats(&link_id).expect("stats").min_rtt_ms(),
        Some(15),
        "a faster clean sample lowers the floor"
    );
}
