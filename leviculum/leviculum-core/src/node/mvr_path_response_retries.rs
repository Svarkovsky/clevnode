//! mvr: a targeted path response is transmitted ONCE, not twice (#192).
//!
//! ## The defect
//!
//! `handle_path_request` case 2b inserts the path-response announce entry
//! with `retries: 0` (`transport.rs`), the value the reference uses for a
//! received announce it will rebroadcast network-wide. A path-response entry
//! is not that: the reference inserts it with `retries = PATHFINDER_R`
//! (`Transport.path_request`, Transport.py:2970), and the job loop completes
//! an entry once `retries > PATHFINDER_R` (Transport.py:585-587). Starting at
//! `PATHFINDER_R` therefore means "fire once, then done", while starting at 0
//! means "fire twice, PATHFINDER_G apart" — which is exactly what a received
//! announce should do (Transport.py:1867) and exactly what a targeted
//! response should not.
//!
//! The consequence is a duplicate 189-byte announce on the requesting
//! interface ~5 s after the response the requester already has. The
//! requester's `packet_hashlist` absorbs it, so nothing breaks and nothing
//! reports it — it is pure surplus traffic, and on a shared LoRa medium
//! surplus traffic is airtime other nodes cannot use.
//!
//! Measured on the `status_parity` two-daemon script (Codeberg #192): lnsd
//! transmitted 62 announce-sized frames where the reference transmitted 59,
//! and the three surplus frames decoded as exactly the three duplicated
//! path responses.
//!
//! ## What is pinned
//!
//! The count over a window long enough for a second retry to have fired
//! (`PATHFINDER_G_MS` plus the retry jitter), and the announce table being
//! empty afterwards, so a fix that merely delays the duplicate past the
//! window cannot pass.
//!
//! The precondition matters: no announce for D may be pending rebroadcast
//! when the request arrives. With one pending, the #170 hold restores the
//! held entry when the response fires, which overwrites the response entry
//! and hides the extra retry (see [`super::mvr_announce_hold`]). The window
//! this mvr covers — a path request for a destination whose announce has
//! long since been rebroadcast — is the common case on a live mesh.
//!
//! Sans-I/O: no LoRa, no Docker, no Python, sub-second wall clock.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, PATHFINDER_G_MS, PATH_REQUEST_GRACE_MS, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, Storage as _};
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

/// Build a destination D and one direct (wire hops 0) announce packet for it.
fn make_destination() -> (crate::DestinationHash, Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["prretry"],
    )
    .unwrap();
    let dest_hash = *dest.hash();
    let ann = dest
        .announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000)
        .unwrap();
    let mut buf = [0u8; MTU];
    let len = ann.pack(&mut buf).unwrap();
    (dest_hash, buf[..len].to_vec())
}

/// Build a path-request packet addressed to `path_req_hash`, requesting `dest`.
fn build_path_request(
    path_req_hash: &[u8; TRUNCATED_HASHBYTES],
    dest: &[u8; TRUNCATED_HASHBYTES],
    requester_id: &[u8; TRUNCATED_HASHBYTES],
    tag: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<u8> {
    let mut data = Vec::with_capacity(48);
    data.extend_from_slice(dest);
    data.extend_from_slice(requester_id);
    data.extend_from_slice(tag);

    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Plain,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: *path_req_hash,
        context: PacketContext::None,
        data: PacketData::Owned(data),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// Announces for `dest` sent as a TARGETED path response on `iface`.
fn targeted_responses(out: &TickOutput, iface: usize, dest: &[u8; TRUNCATED_HASHBYTES]) -> usize {
    out.actions
        .iter()
        .filter(|a| match a {
            Action::SendPacket { iface: i, data, .. } if i.0 == iface => Packet::unpack(data)
                .map(|p| {
                    p.flags.packet_type == PacketType::Announce
                        && p.context == PacketContext::PathResponse
                        && &p.destination_hash == dest
                })
                .unwrap_or(false),
            _ => false,
        })
        .count()
}

/// Drive the node from `from_ms` to `until_ms` in one-second steps, counting
/// every targeted path response for `dest` on `iface` along the way. Stepping
/// (rather than one long jump) is what makes a second retry observable: the
/// scheduler fires at most one due entry per pass.
fn count_responses_until(
    relay: &mut TransportNode,
    iface: usize,
    dest: &[u8; TRUNCATED_HASHBYTES],
    from_ms: u64,
    until_ms: u64,
) -> usize {
    let mut seen = 0;
    let mut t = from_ms;
    while t <= until_ms {
        relay.transport().clock().set(t);
        let out = relay.handle_timeout();
        seen += targeted_responses(&out, iface, dest);
        t += 1_000;
    }
    seen
}

/// THE bug (#192): the path-response entry starts at `retries = 0` and the
/// retry scheduler therefore transmits the targeted response TWICE,
/// `PATHFINDER_G` apart. The reference starts it at `retries = PATHFINDER_R`
/// (Transport.py:2970) and fires it exactly once.
#[test]
fn targeted_path_response_is_transmitted_once() {
    let (dest_d, announce_raw) = make_destination();
    let dest = *dest_d.as_bytes();

    let mut relay = make_transport_node();
    let iface_a = add_iface(&mut relay, "A_announce_in");
    let iface_b = add_iface(&mut relay, "B_requester");

    let t0 = relay.transport().clock().now_ms();

    // D's announce arrives on A and is rebroadcast network-wide. Run the
    // announce entry all the way to completion FIRST, so nothing is pending
    // when the request arrives and the #170 hold plays no part.
    let _ = relay.handle_packet(InterfaceId(iface_a), &announce_raw);
    let mut t = t0;
    while relay.transport().storage().get_announce(&dest).is_some() {
        t += 1_000;
        assert!(
            t < t0 + 120_000,
            "the received announce's own entry never completed; the mvr's \
             precondition (no pending rebroadcast) cannot be established"
        );
        relay.transport().clock().set(t);
        let _ = relay.handle_timeout();
    }

    // B requests the path for D. The response is scheduled one grace period
    // out (Transport.py:2984, `retransmit_timeout = now + PATH_REQUEST_GRACE`).
    let t1 = t;
    let path_req_hash = *relay.transport().path_request_hash();
    let request = build_path_request(&path_req_hash, &dest, &[0xBB; 16], &[0xA1; 16]);
    let _ = relay.handle_packet(InterfaceId(iface_b), &request);

    relay
        .transport()
        .clock()
        .set(t1 + PATH_REQUEST_GRACE_MS + 1);
    let out = relay.handle_timeout();
    assert_eq!(
        targeted_responses(&out, iface_b, &dest),
        1,
        "the targeted path response must fire on the requesting interface \
         at grace expiry"
    );

    // A second retry would be due one PATHFINDER_G later, plus the retry
    // jitter window; watch four times as long as that.
    let extra = count_responses_until(
        &mut relay,
        iface_b,
        &dest,
        t1 + PATH_REQUEST_GRACE_MS + 1_000,
        t1 + PATH_REQUEST_GRACE_MS + 4 * PATHFINDER_G_MS + 20_000,
    );
    assert_eq!(
        extra, 0,
        "a targeted path response is transmitted ONCE: the reference inserts \
         the response entry with retries = PATHFINDER_R (Transport.py:2970) \
         and completes it at retries > PATHFINDER_R (Transport.py:585-587). \
         On master the entry starts at retries = 0 and the requester gets a \
         duplicate announce ~5 s later — Codeberg #192"
    );
    assert!(
        relay.transport().storage().get_announce(&dest).is_none(),
        "the response entry must be gone once it has fired, not merely \
         deferred past the observation window"
    );
}

/// Guard on the other side of the same constant: a RECEIVED announce still
/// fires twice. `retries = 0` is correct there (Transport.py:1867) — the fix
/// must change the path-response arm only, not the shared scheduler.
#[test]
fn received_announce_still_rebroadcasts_twice() {
    let (dest_d, announce_raw) = make_destination();
    let dest = *dest_d.as_bytes();

    let mut relay = make_transport_node();
    let iface_a = add_iface(&mut relay, "A_announce_in");
    let _iface_b = add_iface(&mut relay, "B_peer");

    let t0 = relay.transport().clock().now_ms();
    let _ = relay.handle_packet(InterfaceId(iface_a), &announce_raw);

    let mut broadcasts = 0;
    let mut t = t0;
    while t <= t0 + 60_000 {
        relay.transport().clock().set(t);
        let out = relay.handle_timeout();
        broadcasts += out
            .actions
            .iter()
            .filter(|a| match a {
                Action::Broadcast { data, .. } => Packet::unpack(data)
                    .map(|p| {
                        p.flags.packet_type == PacketType::Announce && p.destination_hash == dest
                    })
                    .unwrap_or(false),
                _ => false,
            })
            .count();
        t += 500;
    }
    assert_eq!(
        broadcasts, 2,
        "a received announce is rebroadcast twice (retries 0 -> 1 -> 2, then \
         retries > PATHFINDER_RETRIES completes the entry)"
    );
}
