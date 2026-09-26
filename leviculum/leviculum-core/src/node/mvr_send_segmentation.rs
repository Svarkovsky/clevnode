//! mvr: sender-side file segmentation (#27).
//!
//! Files whose combined `metadata + data` exceeds `RESOURCE_MAX_EFFICIENT_SIZE`
//! are RECEIVED as multiple segments (our receiver already reassembles them),
//! but the SENDER used to emit only a single segment, so `lncp` could not send
//! files larger than ~1 MiB and a Python `rncp` receiver never got the rest.
//!
//! These tests drive a real initiator <-> responder link end to end: the sender
//! splits the file into `ceil(total_size / MAX)` segments (segment 1 carries the
//! metadata block, later segments carry `MAX`-sized data chunks), advertises the
//! next segment only after the previous segment's proof arrives, and the
//! receiver reassembles the exact original bytes across the per-segment
//! `ResourceCompleted` events (segment_index .. total_segments).
//!
//! Compression is enabled so each ~1 MiB segment collapses to a few parts,
//! keeping the sans-I/O ping-pong deterministic and fast; the exact-byte and
//! per-segment hash checks still catch any boundary/offset error. The
//! incompressible / Python-interop coverage lives in the rnsd_interop suite.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::link::LinkId;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::resource::{ResourceError, ResourceStrategy, RESOURCE_MAX_EFFICIENT_SIZE};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::transport::{Action, InterfaceId, TickOutput};

type EndpointNode = NodeCore<OsRng, MockClock, crate::traits::NoStorage>;

const MAX: usize = RESOURCE_MAX_EFFICIENT_SIZE;

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
    let mut node = NodeCoreBuilder::new().build(OsRng, clock, crate::traits::NoStorage);

    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["segment"],
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
    NodeCoreBuilder::new().build(OsRng, clock, crate::traits::NoStorage)
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
        let back = deliver_all(&mut responder, r_iface, for_responder);
        for_responder = deliver_all(&mut initiator, i_iface, back);
    }

    assert_eq!(initiator.active_link_count(), 1, "initiator link active");
    assert_eq!(responder.active_link_count(), 1, "responder link active");
    (initiator, responder, i_iface, r_iface, caller_link_id)
}

/// Position-dependent, compressible test payload. Any misaligned segment
/// boundary changes the reassembled bytes and fails the exact comparison.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[derive(Default)]
struct Reassembled {
    data: Vec<u8>,
    metadata: Option<Vec<u8>>,
    /// (segment_index, total_segments) for each receiver-side completed segment.
    receiver_segments: Vec<(u32, u32)>,
    /// total_segments reported on the sender's single completion event.
    sender_completed: Option<u32>,
    failures: Vec<(bool, ResourceError)>,
}

fn absorb(re: &mut Reassembled, events: Vec<NodeEvent>) {
    for e in events {
        match e {
            NodeEvent::ResourceCompleted {
                is_sender: false,
                data,
                metadata,
                segment_index,
                total_segments,
                ..
            } => {
                re.data.extend_from_slice(&data);
                if metadata.is_some() {
                    re.metadata = metadata;
                }
                re.receiver_segments.push((segment_index, total_segments));
            }
            NodeEvent::ResourceCompleted {
                is_sender: true,
                total_segments,
                ..
            } => {
                re.sender_completed = Some(total_segments);
            }
            NodeEvent::ResourceFailed {
                is_sender, error, ..
            } => re.failures.push((is_sender, error)),
            _ => {}
        }
    }
}

/// Send `data` (+ optional `metadata`) as a resource and bounce every packet
/// between the two nodes until the transfer quiesces.
fn run_transfer(data: &[u8], metadata: Option<&[u8]>) -> Reassembled {
    let (mut initiator, mut responder, i_iface, r_iface, caller_link_id) = establish();
    responder
        .set_resource_strategy(&caller_link_id, ResourceStrategy::AcceptAll)
        .expect("set AcceptAll on responder link");

    let mut re = Reassembled::default();

    let (_hash, out) = initiator
        .send_resource(&caller_link_id, data, metadata, true)
        .expect("send_resource");
    let mut to_responder = action_data(&out);
    absorb(&mut re, out.events);

    // Generous cap; the loop breaks as soon as no packets remain in flight.
    for _ in 0..20_000 {
        if to_responder.is_empty() {
            break;
        }
        let mut from_responder = Vec::new();
        for pkt in to_responder.drain(..) {
            let o = responder.handle_packet(InterfaceId(r_iface), &pkt);
            from_responder.extend(action_data(&o));
            absorb(&mut re, o.events);
        }
        let mut next = Vec::new();
        for pkt in from_responder {
            let o = initiator.handle_packet(InterfaceId(i_iface), &pkt);
            next.extend(action_data(&o));
            absorb(&mut re, o.events);
        }
        to_responder = next;
    }

    assert!(
        re.failures.is_empty(),
        "no resource should fail in the lab: {:?}",
        re.failures
    );
    re
}

fn assert_single_segment(data: &[u8]) {
    let re = run_transfer(data, None);
    assert_eq!(re.receiver_segments, std::vec![(1, 1)], "one segment");
    assert_eq!(re.sender_completed, Some(1), "sender completes once, l=1");
    assert_eq!(re.data, data, "reassembled bytes match source exactly");
}

fn assert_reassembles(data: &[u8], metadata: Option<&[u8]>, expected_segments: u32) {
    let re = run_transfer(data, metadata);
    assert_eq!(
        re.receiver_segments.len() as u32,
        expected_segments,
        "receiver saw {expected_segments} segments, got {:?}",
        re.receiver_segments
    );
    // Segments arrive in order 1..=L, all tagged with the same total.
    for (i, (idx, total)) in re.receiver_segments.iter().enumerate() {
        assert_eq!(*idx, i as u32 + 1, "segment index order");
        assert_eq!(*total, expected_segments, "total_segments tag");
    }
    assert_eq!(
        re.sender_completed,
        Some(expected_segments),
        "sender emits exactly one completion tagged with l=L"
    );
    assert_eq!(re.data, data, "reassembled bytes match source exactly");
    assert_eq!(
        re.metadata.as_deref(),
        metadata,
        "metadata survives on seg 1"
    );
}

/// Boundary: exactly `MAX_EFFICIENT_SIZE` bytes, no metadata -> single segment
/// (unchanged single-resource path).
#[test]
fn exactly_max_is_single_segment() {
    assert_single_segment(&pattern(MAX));
}

/// Boundary: one byte over -> two segments, exact reassembly.
#[test]
fn one_over_max_splits_into_two() {
    assert_reassembles(&pattern(MAX + 1), None, 2);
}

/// A multi-segment file (~3.5x) reassembles to the exact source bytes.
#[test]
fn multi_segment_roundtrip() {
    assert_reassembles(&pattern(7 * MAX / 2), None, 4);
}

/// lncp always sends a filename as metadata; with metadata the split boundary is
/// `metadata_size + data`, and the metadata must come back on segment 1 only.
#[test]
fn split_with_metadata_reassembles() {
    let meta = b"\x81\xa4name\xa8file.bin"; // msgpack {"name": "file.bin"}
    let metadata_size = 3 + meta.len();
    // Data sized so metadata tips it just over MAX (2 segments) and again well
    // over (multi-segment).
    assert_reassembles(&pattern(MAX - metadata_size + 1), Some(meta), 2);
    assert_reassembles(&pattern(2 * MAX), Some(meta), 3);
}
