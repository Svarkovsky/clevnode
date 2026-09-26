//! mvr: link DATA echo storm with two relays on one shared channel
//! (Codeberg #226 — the direct sibling of the LRPROOF echo storm fixed in
//! f15ac64, one path over).
//!
//! Topology: the rig-cell chain alpha-beta-gamma-delta on ONE shared
//! LoRa-like medium, adjacent pairs in range, everything else deaf.
//! lncp-shaped endpoints R (destination owner) and I (initiator) attach to
//! the edge daemons over shared-instance IPC legs:
//!
//!   R(4) =ipc= alpha(0) --air-- beta(1) --air-- gamma(2) --air-- delta(3) =ipc= I(5)
//!
//! The link path I->R crosses TWO same-interface relays (beta and gamma).
//! One relay cannot show this bug — a node never hears its own TX, so the
//! 3-node cells stay green over it. With two, beta's repeat is heard by
//! gamma and vice versa: the link-table DATA repeat picks its direction by
//! interface only, logs "forwarding anyway" on hop mismatch and forwards,
//! and skips add_packet_hash for link-routed data. Every link data packet
//! (the RTT leg of the handshake included) ping-pongs beta<->gamma, bounded
//! only by max_hops.
//!
//! Contract under test:
//!   1. the link establishes AND its wake (the RTT leg is already link
//!      data) goes quiet within the round cap;
//!   2. app link DATA sent by I arrives at R exactly ONCE;
//!   3. each relay transmits that data packet exactly once;
//!   4. the medium goes quiet within the round cap after the send.
//!
//! Python peers protect this path twice (vendored Transport.py): a same-
//! interface repeat happens ONLY when the taken hops equal the frozen taken
//! or remaining count (:1653-1656), and every actual repeat adds the packet
//! hash to the dedup filter (:1675). Re-heard echo copies arrive with a
//! count matching neither and die silently.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::DestinationType;
use crate::memory_storage::MemoryStorage;
use crate::node::mvr_lrproof_echo_storm::{
    attach_link_destination, describe_mesh_wires, make_air_node, Mesh,
};
use crate::node::NodeCoreBuilder;
use crate::packet::{Packet, PacketType};
use crate::test_log_capture::with_captured_logs;
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};

fn is_link_data_for(data: &[u8], link_id: &[u8; 16]) -> bool {
    Packet::unpack(data)
        .map(|p| {
            p.flags.packet_type == PacketType::Data
                && p.flags.dest_type == DestinationType::Link
                && p.destination_hash == *link_id
        })
        .unwrap_or(false)
}

/// The bug's minimal repro: a real I->R link through beta AND gamma on one
/// shared medium, then link DATA over it. RED while the link-table DATA
/// repeat echoes: beta and gamma re-forward every data packet to each other
/// unboundedly, the far endpoint sees duplicates, the medium never goes
/// quiet, the round cap trips.
#[test]
fn test_two_relay_shared_medium_link_data() {
    const LOCAL_HW_MTU: u32 = 262_144; // interfaces/local.rs
    const RNODE_HW_MTU: u32 = 508; // rnode.rs
    const NAMES: [&str; 6] = ["alpha", "beta", "gamma", "delta", "R", "I"];
    const IFACE_NAMES: [&[&str]; 6] = [
        &["airA", "ipcA"],
        &["airB"],
        &["airC"],
        &["airD", "ipcD"],
        &["ipcR"],
        &["ipcI"],
    ];
    const PAYLOAD: &[u8] = b"echo storm probe #226";

    let mut hs_outcome = None;
    let mut data_outcome = None;
    let mut r_data_count = 0usize;
    let mut i_echo_count = 0usize;
    let mut beta_data_tx = 0usize;
    let mut gamma_data_tx = 0usize;
    let mut hs_wire_dump = String::new();
    let mut data_wire_dump = String::new();

    let ((), logs) = with_captured_logs(|| {
        // Air daemons: the A-B-C-D chain on the shared channel.
        let mut alpha = make_air_node("airA");
        let ipc_a = alpha
            .transport
            .register_interface(Box::new(MockInterface::new("ipcA", 1)));
        alpha.set_interface_name(ipc_a, String::from("ipcA"));
        alpha.set_interface_local_client(ipc_a, true);
        alpha.set_interface_hw_mtu(0, RNODE_HW_MTU);
        alpha.set_interface_hw_mtu(ipc_a, LOCAL_HW_MTU);

        let mut beta = make_air_node("airB");
        beta.set_interface_hw_mtu(0, RNODE_HW_MTU);

        let mut gamma = make_air_node("airC");
        gamma.set_interface_hw_mtu(0, RNODE_HW_MTU);

        let mut delta = make_air_node("airD");
        let ipc_d = delta
            .transport
            .register_interface(Box::new(MockInterface::new("ipcD", 1)));
        delta.set_interface_name(ipc_d, String::from("ipcD"));
        delta.set_interface_local_client(ipc_d, true);
        delta.set_interface_hw_mtu(0, RNODE_HW_MTU);
        delta.set_interface_hw_mtu(ipc_d, LOCAL_HW_MTU);

        // Endpoints (lncp-shaped: NOT transport nodes, one IPC leg each).
        let clock = MockClock::new(TEST_TIME_MS);
        let mut r_node = NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults());
        let ipc_r = r_node
            .transport
            .register_interface(Box::new(MockInterface::new("ipcR", 0)));
        r_node.set_interface_name(ipc_r, String::from("ipcR"));
        r_node.set_interface_hw_mtu(ipc_r, LOCAL_HW_MTU);
        let (dest_hash, signing_key, announce) = attach_link_destination(&mut r_node);

        let clock = MockClock::new(TEST_TIME_MS);
        let mut i_node = NodeCoreBuilder::new().build(OsRng, clock, MemoryStorage::with_defaults());
        let ipc_i = i_node
            .transport
            .register_interface(Box::new(MockInterface::new("ipcI", 0)));
        i_node.set_interface_name(ipc_i, String::from("ipcI"));
        i_node.set_interface_hw_mtu(ipc_i, LOCAL_HW_MTU);

        let mut mesh = Mesh {
            nodes: std::vec![alpha, beta, gamma, delta, r_node, i_node],
            routes: std::vec![
                // The shared air: adjacent pairs hear each other, the rest
                // are deaf. The link path must cross beta AND gamma.
                ((0, 0), std::vec![(1, 0)]),
                ((1, 0), std::vec![(0, 0), (2, 0)]),
                ((2, 0), std::vec![(1, 0), (3, 0)]),
                ((3, 0), std::vec![(2, 0)]),
                // IPC legs.
                ((0, 1), std::vec![(4, 0)]),
                ((4, 0), std::vec![(0, 1)]),
                ((3, 1), std::vec![(5, 0)]),
                ((5, 0), std::vec![(3, 1)]),
            ],
            iface_counts: std::vec![2, 1, 1, 2, 1, 1],
        };

        // 1. R's announce reaches I over the whole chain. Generous advances
        //    fire the rebroadcast timers at every relay.
        let ann_pump = mesh.pump(std::vec![(4, 0, announce)], 24, 100_000);
        assert!(ann_pump.quiet, "announce flood must settle");
        assert!(
            mesh.nodes[5].hops_to(&dest_hash).is_some(),
            "the initiator endpoint I must learn a path to R's destination"
        );

        // 2. I opens a REAL link to R (the lncp handshake). Small advances:
        //    no timers fire, every wire is a reaction to a received packet.
        let (link_id, _routed, out) = mesh.nodes[5].connect(dest_hash, &signing_key);
        let mut seed = Vec::new();
        Mesh::collect_actions(
            &out,
            5,
            1,
            &mut seed,
            &mut std::vec![false; 6],
            &mut Vec::new(),
        );
        assert!(!seed.is_empty(), "connect() must emit the LinkRequest");

        let hs = mesh.pump(seed, 48, 100);
        hs_wire_dump = describe_mesh_wires(&hs.wires, &NAMES, &IFACE_NAMES);
        // Establishment must complete even today (f15ac64) — a failure here
        // is NOT this bug and must be investigated separately.
        assert!(
            hs.established[5],
            "the initiator lncp endpoint I must establish the link \
             (LRPROOF relay across two hops, f15ac64).\n\
             --- wires ---\n{hs_wire_dump}"
        );
        hs_outcome = Some(hs);

        // 3. Link DATA from I to R across both relays.
        let (_ph, out) = mesh.nodes[5]
            .send_packet_on_link(&link_id, PAYLOAD)
            .expect("established link must accept data");
        let mut seed = Vec::new();
        Mesh::collect_actions(
            &out,
            5,
            1,
            &mut seed,
            &mut std::vec![false; 6],
            &mut Vec::new(),
        );
        assert!(!seed.is_empty(), "send_packet_on_link must emit the packet");

        let dp = mesh.pump(seed, 32, 100);
        r_data_count = dp
            .link_data
            .iter()
            .filter(|(node, data)| *node == 4 && data == PAYLOAD)
            .count();
        i_echo_count = dp
            .link_data
            .iter()
            .filter(|(node, data)| *node == 5 && data == PAYLOAD)
            .count();
        let link_id_bytes = *link_id.as_bytes();
        beta_data_tx = dp
            .wires
            .iter()
            .filter(|(src, iface, data)| {
                *src == 1 && *iface == 0 && is_link_data_for(data, &link_id_bytes)
            })
            .count();
        gamma_data_tx = dp
            .wires
            .iter()
            .filter(|(src, iface, data)| {
                *src == 2 && *iface == 0 && is_link_data_for(data, &link_id_bytes)
            })
            .count();
        data_wire_dump = describe_mesh_wires(&dp.wires, &NAMES, &IFACE_NAMES);
        data_outcome = Some(dp);
    });

    let hs = hs_outcome.expect("handshake pump ran");
    let dp = data_outcome.expect("data pump ran");

    // Contract 2: the payload reaches the far endpoint exactly once.
    assert_eq!(
        r_data_count, 1,
        "R must receive the link data exactly once — every further copy is \
         an echo Python would silently drop (Transport.py:1653-1656).\n\
         --- data wires ---\n{data_wire_dump}\n--- logs ---\n{logs}"
    );

    // Contract 2b: the sender must never hear its own payload back. delta
    // repeats I's data ipc->air; if that repeat skips add_packet_hash
    // (local-client ingress exemption), gamma's air repeat re-enters delta,
    // passes ingress dedup, and the cross-interface arm forwards it back
    // onto the ipc leg — a phantom self-delivery at I, decryptable with the
    // symmetric link key. Python caches the hash on EVERY repeat
    // (Transport.py:1675), so the re-heard copy dies in the dedup filter.
    assert_eq!(
        i_echo_count, 0,
        "the initiator I must NOT receive its own payload back — a phantom \
         self-delivery means the relay skipped the hash-add on its \
         local-client-ingress repeat (Python Transport.py:1675 adds \
         unconditionally).\n\
         --- data wires ---\n{data_wire_dump}\n--- logs ---\n{logs}"
    );

    // Contract 3: each relay repeats the data packet exactly once.
    assert_eq!(
        beta_data_tx, 1,
        "beta must transmit the link data on the air exactly once.\n\
         --- data wires ---\n{data_wire_dump}\n--- logs ---\n{logs}"
    );
    assert_eq!(
        gamma_data_tx, 1,
        "gamma must transmit the link data on the air exactly once.\n\
         --- data wires ---\n{data_wire_dump}\n--- logs ---\n{logs}"
    );

    // Contract 4: a storm never lets the medium go quiet — the cap trips.
    assert!(
        dp.quiet,
        "the medium must go quiet within the round cap after the data send \
         — a still-busy medium is a forwarding loop (the link DATA echo \
         storm).\n--- data wires ---\n{data_wire_dump}\n--- logs ---\n{logs}"
    );

    // Contract 1 (tail): the handshake wake itself must go quiet — the RTT
    // leg the initiator sends on establishment is already link data and
    // storms through the same handle_data path.
    assert!(
        hs.quiet,
        "the medium must go quiet within the round cap after the handshake \
         — the RTT link-data leg is echo-storming.\n\
         --- handshake wires ---\n{hs_wire_dump}\n--- logs ---\n{logs}"
    );
}
