//! #376 §5: the relay's announce hold — the reference's rebroadcast
//! window, pinned.
//!
//! ## The race
//!
//! The 2026-09-09 desk run: the direct copy of a board's announce and
//! the copy the neighbour board re-broadcasts race at the phone, and
//! the phone ended on the 2-hop re-broadcast copy often enough even
//! when the direct copy left 0.6 s earlier (desk log 12:08:21.525
//! direct, 12:08:22.166 relay). A relay that re-broadcast instantly
//! would maximise that race.
//!
//! ## The reference
//!
//! Python-RNS holds every foreign announce before re-broadcasting it:
//! the `announce_table` entry is inserted with `retransmit_timeout =
//! now + (RNS.rand() * Transport.PATHFINDER_RW)` (Transport.py:1873),
//! with `PATHFINDER_RW = 0.5` — "Random window for announce
//! rebroadcast" (Transport.py:70). The hold is uniform in [0, 0.5 s);
//! the job loop fires the entry once the timeout passes.
//!
//! ## Ours
//!
//! `handle_announce` inserts the entry with `retransmit_at_ms = now +
//! deterministic_jitter_ms(dest, announce_jitter_max_ms())`
//! (transport.rs, the `should_rebroadcast` arm), where
//! `announce_jitter_max_ms()` floors at `PATHFINDER_RW_MS` (500 ms —
//! the reference's window) and only grows above it for slow (LoRa)
//! interfaces. Same bounds as the reference, so per the batch's rule
//! ("adopt the reference's hold iff ours is shorter than its lower
//! bound", and the reference's lower bound is 0) there is nothing to
//! adopt — but until this file no test pinned the bounds, and an
//! "optimisation" that re-broadcast immediately would have regressed
//! silently. One deliberate difference stays on record: our jitter is
//! deterministic per (identity, destination) where Python re-rolls per
//! announce; the bounds are identical.
//!
//! Sans-I/O: 1 node, 2 mock interfaces, deterministic, sub-second.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, PATHFINDER_RW_MS, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{Packet, PacketType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, Storage};
use crate::transport::{Action, InterfaceId, TickOutput};

type TransportNode = NodeCore<OsRng, MockClock, MemoryStorage>;

fn add_iface(node: &mut TransportNode, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

fn make_transport_node() -> TransportNode {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        clock,
        MemoryStorage::with_defaults(),
    )
}

/// Build a destination and one direct announce packet for it.
fn make_destination() -> ([u8; TRUNCATED_HASHBYTES], Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["annwindow"],
    )
    .unwrap();
    let dest_hash = *dest.hash().as_bytes();
    let ann = dest
        .announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000)
        .unwrap();
    let mut buf = [0u8; MTU];
    let len = ann.pack(&mut buf).unwrap();
    (dest_hash, buf[..len].to_vec())
}

/// Announce transmissions for `dest` in `out`, any form (broadcast or
/// targeted).
fn announce_tx_count(out: &TickOutput, dest: &[u8; TRUNCATED_HASHBYTES]) -> usize {
    out.actions
        .iter()
        .filter(|a| {
            let data = match a {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => data,
            };
            Packet::unpack(data)
                .map(|p| p.flags.packet_type == PacketType::Announce && &p.destination_hash == dest)
                .unwrap_or(false)
        })
        .count()
}

/// THE pin: a foreign announce is re-broadcast only after a hold whose
/// window equals the reference's — never in the receiving pass, never
/// before its scheduled time, and the window's ceiling is the
/// reference's `PATHFINDER_RW` (0.5 s) when no slow interface widens
/// it.
#[test]
fn a_relayed_announce_waits_out_the_reference_hold_window() {
    let (dest, announce_raw) = make_destination();

    let mut relay = make_transport_node();
    let iface_a = add_iface(&mut relay, "A_announce_in");
    let _iface_b = add_iface(&mut relay, "B_neighbour");

    let t0 = relay.transport().clock().now_ms();

    // The announce arrives. The receiving pass itself must not re-emit
    // it — an instant re-broadcast is exactly the racing relay copy the
    // desk run measured.
    let out = relay.handle_packet(InterfaceId(iface_a), &announce_raw);
    assert_eq!(
        announce_tx_count(&out, &dest),
        0,
        "no re-broadcast in the receiving pass: the hold exists"
    );

    // The window: floored at the reference's PATHFINDER_RW. With only
    // fast interfaces registered the ceiling IS the floor, i.e. the
    // BLE-room shape of the desk run.
    let window_ms = relay.transport().announce_jitter_max_ms();
    assert_eq!(
        window_ms, PATHFINDER_RW_MS,
        "no slow interface: the window is the reference's 0.5 s"
    );

    let entry = relay
        .transport()
        .storage()
        .get_announce(&dest)
        .expect("the announce is queued for rebroadcast");
    let due = entry
        .retransmit_at_ms
        .expect("a transport relay schedules the rebroadcast");
    assert!(
        due >= t0 && due < t0 + window_ms,
        "the hold lies inside the reference window [t0, t0+{window_ms}), got t0+{}",
        due - t0
    );

    // Before the scheduled time: still held (skipped when this node's
    // deterministic jitter for this destination happens to be 0 — the
    // reference's own lower bound).
    if due > t0 {
        relay.transport().clock().set(due - 1);
        let out = relay.handle_timeout();
        assert_eq!(
            announce_tx_count(&out, &dest),
            0,
            "one millisecond before the hold expires nothing goes out"
        );
    }

    // At the scheduled time the rebroadcast fires — the hold delays it,
    // it must not lose it.
    relay.transport().clock().set(due);
    let out = relay.handle_timeout();
    assert_eq!(
        announce_tx_count(&out, &dest),
        1,
        "the held re-broadcast goes out once the window elapsed"
    );
}
