//! mvr: host-side repro of the two-LNode path-resolve stall (Bug B).
//!
//! ## Field failure (LoRa integ `lora_lnode_lncp_bidir_slow`)
//!
//! Two LNodes on the same SX1262 chip, SF10, bidirectional. The host drives
//! both over serial. The test wedges at the FIRST `wait_for_path` step: the
//! host asks LNode `alpha` to resolve LNode `beta`'s probe destination. On the
//! device `alpha` TXs a small packet over LoRa and then waits forever for a
//! reply that never arrives (`alpha` gets stuck in escalating post-TX RX
//! timeouts; `beta` stays healthier). It PASSED on 2026-07-07 (`8cfc0de7`) and
//! FAILS on `5fb1db0` — a genuine regression in the 2026-07-07..14 window. It
//! is NOT OOM (heap flat) and NOT the firmware radio code
//! (`leviculum-nrf/src/lora.rs` unchanged in-window), so the suspect is shared
//! `leviculum-core` path/announce logic and/or the LNode interface-mode wiring.
//!
//! ## The LNode topology this models (authoritative: the firmware bins)
//!
//! `leviculum-nrf/src/bin/{t114,rak4631}.rs` register, per LNode:
//!   - interface 0 = `serial_usb`, set to **Gateway** mode (`5b46d023`, #117).
//!     This is the host-facing link. It is NOT marked as a shared-instance local
//!     client, so a path request arriving from the host is a *network* request
//!     (`from_local == false`); Gateway mode's `discovers_paths() == true` is
//!     exactly what lets the node re-originate discovery on the host's behalf.
//!   - interface 1 = `lora_sx1262`, left in the default **Full** mode. This is
//!     the LNode-to-LNode radio link.
//!
//! So the host-side, radio-free path this exercises is:
//!   host --(serial/Gateway)--> alpha : path request for beta's probe
//!   alpha --(lora/Full)------> beta  : re-originated discovery (case 3)
//!   beta  --(lora/Full)------> alpha : beta answers as the destination owner
//!                                      (`handle_path_request` case 1) with its
//!                                      announce
//!   alpha : learns beta's probe path
//!
//! ## What this test asserts
//!
//! Driving both `NodeCore`s with a `MockClock` and an in-process LoRa medium,
//! after injecting the host's path request `alpha` must learn a path to beta's
//! probe destination within a bounded number of rounds.
//!
//! Two outcomes, both diagnostic:
//!   - RED here: the stall reproduces in pure path/announce logic, radio-free.
//!   - GREEN here: the logic round-trips fine host-side, which pins the
//!     on-device failure to radio/timing behaviour, not shared core logic.
//!
//! Sans-I/O: no LoRa, no Docker, no Python, sub-second wall clock.

extern crate std;

use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::constants::{MTU, TRUNCATED_HASHBYTES};
use crate::destination::{Destination, DestinationType, Direction};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::{Clock, InterfaceMode};
use crate::transport::{Action, InterfaceId, TickOutput};

type Node = NodeCore<OsRng, MockClock, MemoryStorage>;

fn make_node() -> Node {
    let clock = MockClock::new(TEST_TIME_MS);
    NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        clock,
        MemoryStorage::with_defaults(),
    )
}

fn add_iface(node: &mut Node, name: &'static str) -> usize {
    let idx = node
        .transport
        .register_interface(std::boxed::Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    idx
}

/// Register a link-less announce-able Single destination on `node`, mirroring an
/// LNode's probe destination. Returns its hash.
fn register_probe(node: &mut Node, aspect: &'static str) -> crate::DestinationHash {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "lnode",
        &[aspect],
    )
    .unwrap();
    let hash = *dest.hash();
    node.register_destination(dest);
    hash
}

/// Build a network path request (dest_hash + requester_transport_id + tag)
/// addressed to the well-known path-request destination, as a peer on a Gateway
/// interface would send it.
fn build_network_path_request(
    path_req_hash: &[u8; TRUNCATED_HASHBYTES],
    dest: &crate::DestinationHash,
    requester_id: &[u8; TRUNCATED_HASHBYTES],
    tag: &[u8; TRUNCATED_HASHBYTES],
) -> Vec<u8> {
    let mut data = Vec::with_capacity(48);
    data.extend_from_slice(dest.as_bytes());
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

fn is_announce_for(data: &[u8], dest: &crate::DestinationHash) -> bool {
    match Packet::unpack(data) {
        Ok(p) => {
            p.flags.packet_type == PacketType::Announce && p.destination_hash == *dest.as_bytes()
        }
        Err(_) => false,
    }
}

fn is_path_request_for(data: &[u8], dest: &crate::DestinationHash) -> bool {
    match Packet::unpack(data) {
        Ok(p) => {
            p.flags.packet_type == PacketType::Data
                && p.data.as_slice().len() >= TRUNCATED_HASHBYTES
                && &p.data.as_slice()[..TRUNCATED_HASHBYTES] == dest.as_bytes()
        }
        Err(_) => false,
    }
}

/// Every packet an output wants to put on the wire, with the sending interface
/// resolved. A `Broadcast` reaches an interface iff it is not excluded.
fn lora_bound(output: &TickOutput, lora_iface: usize) -> Vec<Vec<u8>> {
    output
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::SendPacket { iface, data, .. } => (iface.0 == lora_iface).then(|| data.clone()),
            Action::Broadcast {
                data,
                exclude_iface,
                exclude_ifaces,
            } => {
                let excluded = exclude_iface.map(|i| i.0) == Some(lora_iface)
                    || exclude_ifaces.iter().any(|i| i.0 == lora_iface);
                (!excluded).then(|| data.clone())
            }
        })
        .collect()
}

/// Alpha's LoRa interface index (`serial`=0 Gateway, `lora`=1 Full).
const ALPHA_LORA: usize = 1;
/// Beta's LoRa interface index (its only interface).
const BETA_LORA: usize = 0;

struct Trace {
    resolved: bool,
    rounds: usize,
    alpha_reoriginated: bool,
    beta_answered: bool,
    alpha_hops: Option<u8>,
}

/// Run the radio-free path-resolve round trip and report what happened.
///
/// `beta_preannounce` models whether beta has already broadcast its probe once
/// (its `MGMT_ANNOUNCE_INITIAL_DELAY_MS` 15 s startup announce has fired),
/// which populates beta's own announce cache. `handle_path_request` case 1
/// only schedules a deferred answer when that cache is present, so this toggle
/// separates a warm beta from a cold-start beta.
fn run(beta_preannounce: bool) -> (Trace, crate::DestinationHash) {
    let mut alpha = make_node();
    let alpha_serial = add_iface(&mut alpha, "serial_usb");
    let alpha_lora = add_iface(&mut alpha, "lora_sx1262");
    alpha.set_interface_mode(alpha_serial, InterfaceMode::Gateway);
    // alpha_lora stays Full (default). Sanity-check the modelled modes.
    assert_eq!(alpha_serial, 0);
    assert_eq!(alpha_lora, ALPHA_LORA);
    assert!(
        alpha
            .transport()
            .interface_mode(alpha_serial)
            .discovers_paths(),
        "the serial interface must discover paths on the host's behalf"
    );
    assert!(
        !alpha
            .transport()
            .interface_mode(alpha_lora)
            .discovers_paths(),
        "the lora interface must be a non-discovering Full link"
    );

    let mut beta = make_node();
    let beta_lora = add_iface(&mut beta, "lora_sx1262");
    assert_eq!(beta_lora, BETA_LORA);
    let beta_probe = register_probe(&mut beta, "probe");

    // Beta announces its probe once at startup, exactly as an LNode does. This
    // populates BETA's own announce cache (needed for the case-1 deferred
    // answer) but we deliver it NOWHERE: alpha never heard it (or it expired),
    // which is precisely why the host now has to ask alpha to resolve it.
    if beta_preannounce {
        let _ = beta
            .announce_destination(&beta_probe, Some(b"beta"))
            .unwrap();
    }
    assert_eq!(
        alpha.hops_to(&beta_probe),
        None,
        "precondition: alpha has no path to beta's probe yet"
    );

    // The host issues a path request for beta's probe over the serial link.
    let path_req_hash = *alpha.transport().path_request_hash();
    let requester_id = [0x77u8; TRUNCATED_HASHBYTES]; // stand-in host transport id
    let tag = [0x33u8; TRUNCATED_HASHBYTES];
    let request = build_network_path_request(&path_req_hash, &beta_probe, &requester_id, &tag);

    // Pending deliveries across the in-process LoRa medium.
    let mut to_beta: Vec<Vec<u8>> = Vec::new();
    let mut to_alpha: Vec<Vec<u8>> = Vec::new();

    let mut alpha_reoriginated = false;
    let mut beta_answered = false;

    // Round 0: inject the host request into alpha's serial interface.
    let out = alpha.handle_packet(InterfaceId(alpha_serial), &request);
    for pkt in lora_bound(&out, alpha_lora) {
        if is_path_request_for(&pkt, &beta_probe) {
            alpha_reoriginated = true;
        }
        to_beta.push(pkt);
    }

    const STEP_MS: u64 = 1_000; // past the 400 ms path-request grace each round
    const MAX_ROUNDS: usize = 24;
    let mut rounds = 0;

    while rounds < MAX_ROUNDS && alpha.hops_to(&beta_probe).is_none() {
        rounds += 1;

        // Deliver everything queued for each node.
        let deliver_beta = core::mem::take(&mut to_beta);
        for pkt in deliver_beta {
            let out = beta.handle_packet(InterfaceId(beta_lora), &pkt);
            for reply in lora_bound(&out, beta_lora) {
                if is_announce_for(&reply, &beta_probe) {
                    beta_answered = true;
                }
                to_alpha.push(reply);
            }
        }
        let deliver_alpha = core::mem::take(&mut to_alpha);
        for pkt in deliver_alpha {
            let out = alpha.handle_packet(InterfaceId(alpha_lora), &pkt);
            for fwd in lora_bound(&out, alpha_lora) {
                to_beta.push(fwd);
            }
        }

        // Advance both clocks and fire deferred work (beta's grace-delayed
        // announce answer, alpha's schedulers).
        let a_now = alpha.transport().clock().now_ms();
        alpha.transport().clock().set(a_now + STEP_MS);
        let b_now = beta.transport().clock().now_ms();
        beta.transport().clock().set(b_now + STEP_MS);

        let out = beta.handle_timeout();
        for reply in lora_bound(&out, beta_lora) {
            if is_announce_for(&reply, &beta_probe) {
                beta_answered = true;
            }
            to_alpha.push(reply);
        }
        let out = alpha.handle_timeout();
        for fwd in lora_bound(&out, alpha_lora) {
            to_beta.push(fwd);
        }
    }

    let alpha_hops = alpha.hops_to(&beta_probe);
    (
        Trace {
            resolved: alpha_hops.is_some(),
            rounds,
            alpha_reoriginated,
            beta_answered,
            alpha_hops,
        },
        beta_probe,
    )
}

/// The host-side path-resolve round trip: alpha must learn beta's probe path.
///
/// If this is RED, the two-LNode stall reproduces in pure path/announce logic.
/// If GREEN, the round trip works host-side and the on-device failure is
/// radio/timing-specific.
#[test]
fn lnode_pathresolve_alpha_learns_beta_probe() {
    let (t, _beta_probe) = run(true);

    assert!(
        t.resolved,
        "alpha never learned a path to beta's probe within {} rounds \
         (alpha_reoriginated={}, beta_answered={}, alpha_hops={:?}). \
         If alpha_reoriginated is false, alpha dropped the host's path request \
         instead of discovering; if beta_answered is false, beta did not answer \
         as the destination owner; if both are true but alpha still has no path, \
         alpha rejected beta's returning announce.",
        t.rounds, t.alpha_reoriginated, t.beta_answered, t.alpha_hops
    );

    // Guard that the GREEN is meaningful: the full modelled round trip actually
    // fired, rather than the path being resolved by some trivial shortcut.
    assert!(
        t.alpha_reoriginated,
        "alpha must have re-originated the host's path request onto the lora link"
    );
    assert!(
        t.beta_answered,
        "beta must have answered as the destination owner with its announce"
    );
    assert_eq!(
        t.alpha_hops,
        Some(1),
        "alpha's learned path to beta's probe is one lora hop"
    );
}

/// Cold-start characterization: the SAME round trip when beta has not yet fired
/// its 15 s startup announce, so it holds no cached announce for its probe when
/// alpha's re-originated request arrives. Documents whether the case-1
/// "schedule only with a cache" gating leaves the first request unanswered.
#[test]
fn lnode_pathresolve_cold_start_beta_answers_without_prior_announce() {
    let (t, _beta_probe) = run(false);

    assert!(
        t.resolved,
        "cold-start: alpha never learned beta's probe within {} rounds \
         (alpha_reoriginated={}, beta_answered={}, alpha_hops={:?}). \
         A false beta_answered here means beta, holding no cached announce, \
         did not answer the first path request as the destination owner.",
        t.rounds, t.alpha_reoriginated, t.beta_answered, t.alpha_hops
    );
}
