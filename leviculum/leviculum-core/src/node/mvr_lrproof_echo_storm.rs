//! mvr: LRPROOF echo storm on a shared-medium relay (rig: `lora_3node_relay`,
//! 2026-08-12 — beta re-forwards the SAME proof every ~600 ms for minutes,
//! saturating the channel; the lncp handshake times out).
//!
//! Topology: the fundamental single-channel chain A-B-C on ONE shared
//! LoRa-like medium. A↔B and B↔C are in range, A↮C. All three nodes are
//! transport-enabled, matching the rnode_trio rig cell. C opens a REAL link
//! (LinkRequest + LRPROOF legs, real crypto) to a destination on A, relayed
//! by B back onto the SAME interface both legs arrived on.
//!
//! Contract under test:
//!   1. the handshake COMPLETES (LinkEstablished at the initiator C);
//!   2. B transmits the LRPROOF a bounded number of times — exactly once
//!      absent loss, and this pump has no loss;
//!   3. the medium goes QUIET within the round cap (a storm keeps the air
//!      busy every round and must fail the cap, not spin forever).
//!
//! Python peers do not echo: the LRPROOF relay only repeats a proof whose
//! taken hops equal the frozen remaining count (vendored Transport.py:2176),
//! and every re-heard copy arrives with a different count and is silently
//! dropped (Transport.py:2207). LRPROOF is exempt from packet-hash dedup on
//! both stacks (Transport.py:1502), so that strict hop match is the ONLY
//! loop breaker on a shared medium.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::OsRng;

use crate::destination::{Destination, DestinationType, Direction, ProofStrategy};
use crate::identity::Identity;
use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder, NodeEvent};
use crate::packet::{Packet, PacketContext, PacketType};
use crate::test_log_capture::with_captured_logs;
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::traits::Clock;
use crate::transport::{Action, InterfaceId};

pub(super) type Node = NodeCore<OsRng, MockClock, MemoryStorage>;

pub(super) fn make_air_node(name: &'static str) -> Node {
    let clock = MockClock::new(TEST_TIME_MS);
    let mut node = NodeCoreBuilder::new().enable_transport(true).build(
        OsRng,
        clock,
        MemoryStorage::with_defaults(),
    );
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new(name, 0)));
    node.set_interface_name(idx, String::from(name));
    node
}

/// Attach a link-accepting destination to `node` and return its hash, the
/// Ed25519 verifying key (what the initiator learns from the announce) and a
/// packed announce packet (wire hops 0, as the owner would broadcast it).
pub(super) fn attach_link_destination(
    node: &mut Node,
) -> (crate::DestinationHash, [u8; 32], Vec<u8>) {
    let identity = Identity::generate(&mut OsRng);
    let signing_key = identity.ed25519_verifying().to_bytes();
    let mut dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "mvrapp",
        &["echostorm"],
    )
    .unwrap();
    dest.set_accepts_links(true);
    dest.set_proof_strategy(ProofStrategy::All);
    let dest_hash = *dest.hash();
    let ann = dest
        .announce(None, &mut OsRng, TEST_TIME_MS, TEST_TIME_MS / 1000)
        .unwrap();
    let mut buf = [0u8; crate::constants::MTU];
    let len = ann.pack(&mut buf).unwrap();
    let announce = buf[..len].to_vec();
    node.register_destination(dest);
    (dest_hash, signing_key, announce)
}

/// A↔B and B↔C hear each other; A and C are out of range (the deaf pair).
fn in_range(src: usize, dst: usize) -> bool {
    matches!((src, dst), (0, 1) | (1, 0) | (1, 2) | (2, 1))
}

struct PumpOutcome {
    /// Did the medium go quiet before the round cap?
    quiet: bool,
    /// Every wire transmission of the run: (source node, bytes).
    wires: Vec<(usize, Vec<u8>)>,
    /// Per node: did a `LinkEstablished` event fire?
    established: [bool; 3],
}

/// Shared-medium pump over NodeCore instances (the node-level sibling of
/// `pump_shared_medium`, transport.rs). Each round fires every node's timers,
/// then delivers this round's wire bytes to every in-range neighbour; the
/// packets those deliveries emit become the next round's air. The clock
/// advance per round is SMALL so no retry or keepalive timers fire — every
/// wire after the seed is a direct causal consequence of a received packet,
/// which is exactly the echo-loop shape under test.
fn pump(
    nodes: &mut [Node; 3],
    seed: Vec<(usize, Vec<u8>)>,
    rounds_max: usize,
    advance_ms: u64,
) -> PumpOutcome {
    let mut pending = seed;
    let mut wires = Vec::new();
    let mut established = [false; 3];
    let mut quiet = false;

    for _ in 0..rounds_max {
        for src in 0..nodes.len() {
            let out = nodes[src].handle_timeout();
            if out
                .events
                .iter()
                .any(|e| matches!(e, NodeEvent::LinkEstablished { .. }))
            {
                established[src] = true;
            }
            for a in &out.actions {
                match a {
                    Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => {
                        pending.push((src, data.clone()));
                    }
                }
            }
        }
        if pending.is_empty() {
            quiet = true;
            break;
        }
        for (src, data) in core::mem::take(&mut pending) {
            for dst in 0..nodes.len() {
                if !in_range(src, dst) {
                    continue;
                }
                let out = nodes[dst].handle_packet(InterfaceId(0), &data);
                if out
                    .events
                    .iter()
                    .any(|e| matches!(e, NodeEvent::LinkEstablished { .. }))
                {
                    established[dst] = true;
                }
                for a in &out.actions {
                    match a {
                        Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => {
                            pending.push((dst, data.clone()));
                        }
                    }
                }
            }
            wires.push((src, data));
        }
        for node in nodes.iter() {
            let now = node.transport().clock().now_ms();
            node.transport().clock().set(now + advance_ms);
        }
    }

    PumpOutcome {
        quiet,
        wires,
        established,
    }
}

fn is_lrproof(data: &[u8]) -> bool {
    Packet::unpack(data)
        .map(|p| p.flags.packet_type == PacketType::Proof && p.context == PacketContext::Lrproof)
        .unwrap_or(false)
}

/// Render the wire log compactly for failure messages: one line per TX.
fn describe_wires(wires: &[(usize, Vec<u8>)]) -> String {
    use core::fmt::Write;
    let names = ["A", "B", "C"];
    let mut s = String::new();
    for (i, (src, data)) in wires.iter().enumerate() {
        match Packet::unpack(data) {
            Ok(p) => {
                let _ = writeln!(
                    s,
                    "  tx[{i}] {} -> air: type={:?} ctx={:?} hops={} dst={:02x}{:02x}.. len={}",
                    names[*src],
                    p.flags.packet_type,
                    p.context,
                    p.hops,
                    p.destination_hash[0],
                    p.destination_hash[1],
                    data.len(),
                );
            }
            Err(_) => {
                let _ = writeln!(
                    s,
                    "  tx[{i}] {} -> air: <unparseable> len={}",
                    names[*src],
                    data.len()
                );
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Rig-cell shape: lncp endpoints as LOCAL CLIENTS behind the end daemons.
//
// The rnode_trio cell does not run the link endpoints inside the daemons:
// lncp attaches to alpha/gamma over the shared-instance IPC (LOCAL_HW_MTU =
// 262144, interfaces/local.rs), while the air is an RNode (HW_MTU = 508,
// rnode.rs). Five NodeCores model that: three transport daemons on the air
// plus two endpoint nodes on IPC legs.
//
//   R(3) ==ipc== alpha(0) --air-- beta(1) --air-- gamma(2) ==ipc== I(4)
//
// alpha and gamma are out of range of each other (the rig's deaf filter).
// ---------------------------------------------------------------------------

/// A wire transmission in the mesh: (source node, source interface, bytes).
pub(super) type MeshWire = (usize, usize, Vec<u8>);

/// A (node index, interface index) endpoint in the mesh.
pub(super) type MeshPort = (usize, usize);

pub(super) struct Mesh {
    pub(super) nodes: Vec<Node>,
    /// Point-to-point delivery map: (src node, src iface) -> [(dst, iface)].
    pub(super) routes: Vec<(MeshPort, Vec<MeshPort>)>,
    /// Interface count per node (Broadcast fans out over all of them).
    pub(super) iface_counts: Vec<usize>,
}

impl Mesh {
    fn deliveries(&self, src: usize, src_iface: usize) -> Vec<(usize, usize)> {
        self.routes
            .iter()
            .find(|(k, _)| *k == (src, src_iface))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    pub(super) fn collect_actions(
        out: &crate::transport::TickOutput,
        src: usize,
        iface_count: usize,
        pending: &mut Vec<MeshWire>,
        established: &mut [bool],
        link_data: &mut Vec<(usize, Vec<u8>)>,
    ) {
        if out
            .events
            .iter()
            .any(|e| matches!(e, NodeEvent::LinkEstablished { .. }))
        {
            established[src] = true;
        }
        for e in &out.events {
            if let NodeEvent::LinkDataReceived { data, .. } = e {
                link_data.push((src, data.clone()));
            }
        }
        for a in &out.actions {
            match a {
                Action::SendPacket { iface, data, .. } => {
                    pending.push((src, iface.0, data.clone()));
                }
                Action::Broadcast {
                    data,
                    exclude_iface,
                    exclude_ifaces,
                } => {
                    for i in 0..iface_count {
                        let id = InterfaceId(i);
                        if Some(id) == *exclude_iface || exclude_ifaces.contains(&id) {
                            continue;
                        }
                        pending.push((src, i, data.clone()));
                    }
                }
            }
        }
    }

    /// Same contract as `pump`: timers first, then deliver, small advances
    /// keep retries out of the causal chain. Quiet within the cap or bust.
    pub(super) fn pump(
        &mut self,
        seed: Vec<MeshWire>,
        rounds_max: usize,
        advance_ms: u64,
    ) -> MeshOutcome {
        let n = self.nodes.len();
        let mut pending = seed;
        let mut wires = Vec::new();
        let mut established = std::vec![false; n];
        let mut link_data = Vec::new();
        let mut quiet = false;

        for _ in 0..rounds_max {
            for src in 0..n {
                let out = self.nodes[src].handle_timeout();
                Self::collect_actions(
                    &out,
                    src,
                    self.iface_counts[src],
                    &mut pending,
                    &mut established,
                    &mut link_data,
                );
            }
            if pending.is_empty() {
                quiet = true;
                break;
            }
            for (src, src_iface, data) in core::mem::take(&mut pending) {
                for (dst, dst_iface) in self.deliveries(src, src_iface) {
                    let out = self.nodes[dst].handle_packet(InterfaceId(dst_iface), &data);
                    Self::collect_actions(
                        &out,
                        dst,
                        self.iface_counts[dst],
                        &mut pending,
                        &mut established,
                        &mut link_data,
                    );
                }
                wires.push((src, src_iface, data));
            }
            for node in self.nodes.iter() {
                let now = node.transport().clock().now_ms();
                node.transport().clock().set(now + advance_ms);
            }
        }

        MeshOutcome {
            quiet,
            wires,
            established,
            link_data,
        }
    }
}

pub(super) struct MeshOutcome {
    pub(super) quiet: bool,
    pub(super) wires: Vec<MeshWire>,
    pub(super) established: Vec<bool>,
    /// Every `LinkDataReceived` of the run: (node, decrypted payload).
    pub(super) link_data: Vec<(usize, Vec<u8>)>,
}

pub(super) fn describe_mesh_wires(
    wires: &[MeshWire],
    names: &[&str],
    iface_names: &[&[&str]],
) -> String {
    use core::fmt::Write;
    let mut s = String::new();
    for (i, (src, src_iface, data)) in wires.iter().enumerate() {
        let iface = iface_names[*src].get(*src_iface).copied().unwrap_or("?");
        match Packet::unpack(data) {
            Ok(p) => {
                let _ = writeln!(
                    s,
                    "  tx[{i}] {}/{iface}: type={:?} ctx={:?} hops={} dst={:02x}{:02x}.. len={}",
                    names[*src],
                    p.flags.packet_type,
                    p.context,
                    p.hops,
                    p.destination_hash[0],
                    p.destination_hash[1],
                    data.len(),
                );
            }
            Err(_) => {
                let _ = writeln!(
                    s,
                    "  tx[{i}] {}/{iface}: <unparseable> len={}",
                    names[*src],
                    data.len()
                );
            }
        }
    }
    s
}

/// The rig-cell repro: a real I->R link handshake, lncp-shaped endpoints on
/// IPC legs behind alpha/gamma, beta relaying on the shared air. RED while
/// the LRPROOF relay echoes: the daemons re-forward proof copies endlessly,
/// the air never goes quiet, and the initiator lncp never establishes.
#[test]
fn test_rig_cell_local_client_link_handshake() {
    const LOCAL_HW_MTU: u32 = 262_144; // interfaces/local.rs
    const RNODE_HW_MTU: u32 = 508; // rnode.rs
    const NAMES: [&str; 5] = ["alpha", "beta", "gamma", "R", "I"];
    const IFACE_NAMES: [&[&str]; 5] = [
        &["airA", "ipcA"],
        &["airB"],
        &["airC", "ipcC"],
        &["ipcR"],
        &["ipcI"],
    ];

    let mut i_path_hops = None;
    let mut outcome = None;
    let mut beta_proof_tx = 0usize;
    let mut air_proof_tx = 0usize;
    let mut wire_dump = String::new();

    let ((), logs) = with_captured_logs(|| {
        // Daemons.
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
        let ipc_c = gamma
            .transport
            .register_interface(Box::new(MockInterface::new("ipcC", 1)));
        gamma.set_interface_name(ipc_c, String::from("ipcC"));
        gamma.set_interface_local_client(ipc_c, true);
        gamma.set_interface_hw_mtu(0, RNODE_HW_MTU);
        gamma.set_interface_hw_mtu(ipc_c, LOCAL_HW_MTU);

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
            nodes: std::vec![alpha, beta, gamma, r_node, i_node],
            routes: std::vec![
                // The shared air: alpha↔beta, beta↔gamma; alpha↮gamma (deaf).
                ((0, 0), std::vec![(1, 0)]),
                ((1, 0), std::vec![(0, 0), (2, 0)]),
                ((2, 0), std::vec![(1, 0)]),
                // IPC legs.
                ((0, 1), std::vec![(3, 0)]),
                ((3, 0), std::vec![(0, 1)]),
                ((2, 1), std::vec![(4, 0)]),
                ((4, 0), std::vec![(2, 1)]),
            ],
            iface_counts: std::vec![2, 1, 2, 1, 1],
        };

        // 1. R's announce reaches I: R -> alpha -> air -> beta -> air ->
        //    gamma -> IPC -> I. Generous advances fire rebroadcast timers.
        let ann_pump = mesh.pump(std::vec![(3, 0, announce)], 16, 100_000);
        assert!(ann_pump.quiet, "announce flood must settle");
        i_path_hops = mesh.nodes[4].hops_to(&dest_hash);
        assert!(
            i_path_hops.is_some(),
            "the initiator endpoint I must learn a path to R's destination"
        );

        // 2. I opens a REAL link to R's destination (the lncp handshake).
        let (_link_id, _routed, out) = mesh.nodes[4].connect(dest_hash, &signing_key);
        let mut seed = Vec::new();
        Mesh::collect_actions(
            &out,
            4,
            1,
            &mut seed,
            &mut std::vec![false; 5],
            &mut Vec::new(),
        );
        assert!(!seed.is_empty(), "connect() must emit the LinkRequest");

        let hs = mesh.pump(seed, 32, 100);
        beta_proof_tx = hs
            .wires
            .iter()
            .filter(|(src, iface, data)| *src == 1 && *iface == 0 && is_lrproof(data))
            .count();
        air_proof_tx = hs
            .wires
            .iter()
            .filter(|(_, iface, data)| *iface == 0 && is_lrproof(data))
            .count();
        wire_dump = describe_mesh_wires(&hs.wires, &NAMES, &IFACE_NAMES);
        outcome = Some(hs);
    });

    let hs = outcome.expect("handshake pump ran");

    // Contract 3: a storm never lets the air go quiet — the cap trips.
    assert!(
        hs.quiet,
        "the medium must go quiet within the round cap — a still-busy \
         medium is a forwarding loop (the LRPROOF echo storm). \
         air proof TX total={air_proof_tx}\n\
         --- wires ---\n{wire_dump}\n--- logs ---\n{logs}"
    );

    // Contract 1: the handshake completes at the initiator endpoint.
    assert!(
        hs.established[4],
        "the initiator lncp endpoint I must establish the link.\n\
         --- wires ---\n{wire_dump}\n--- logs ---\n{logs}"
    );

    // Contract 2: the relay forwards the proof on the air exactly once.
    assert_eq!(
        beta_proof_tx, 1,
        "beta must transmit the LRPROOF on the air exactly once — every \
         further copy is an echo Python would silently drop \
         (Transport.py:2176).\n\
         --- wires ---\n{wire_dump}\n--- logs ---\n{logs}"
    );
}

/// The bug's minimal repro: a real C->A link handshake through B on one
/// shared medium. RED while the LRPROOF relay echoes: B re-forwards proof
/// copies unboundedly, the medium never goes quiet, the round cap trips.
#[test]
fn test_shared_medium_multihop_link_handshake() {
    let mut c_hops_to_a = None;
    let mut outcome = None;
    let mut beta_proof_tx = 0usize;
    let mut wire_dump = String::new();

    let ((), logs) = with_captured_logs(|| {
        // A = 0 (owns the destination), B = 1 (relay), C = 2 (initiator).
        let mut nodes = [
            make_air_node("airA"),
            make_air_node("airB"),
            make_air_node("airC"),
        ];
        let (dest_hash, signing_key, announce) = attach_link_destination(&mut nodes[0]);

        // 1. A's announce floods the shared medium; C learns A via B (hops 2).
        //    Generous per-round clock advance fires B's rebroadcast timer.
        let ann_pump = pump(&mut nodes, std::vec![(0, announce)], 16, 100_000);
        assert!(ann_pump.quiet, "announce flood must settle");
        c_hops_to_a = nodes[2].hops_to(&dest_hash);
        assert_eq!(c_hops_to_a, Some(2), "C must learn A's destination via B");

        // 2. C opens a REAL link to A. Seed the air with C's LinkRequest and
        //    pump with a small advance: no timers fire, so every subsequent
        //    wire is a direct reaction to a received packet.
        let (_link_id, routed, out) = nodes[2].connect(dest_hash, &signing_key);
        assert!(
            routed,
            "C must route the LinkRequest via its path through B"
        );
        let seed: Vec<(usize, Vec<u8>)> = out
            .actions
            .iter()
            .map(|a| match a {
                Action::SendPacket { data, .. } | Action::Broadcast { data, .. } => {
                    (2usize, data.clone())
                }
            })
            .collect();
        assert!(!seed.is_empty(), "connect() must emit the LinkRequest");

        let hs = pump(&mut nodes, seed, 32, 100);
        beta_proof_tx = hs
            .wires
            .iter()
            .filter(|(src, data)| *src == 1 && is_lrproof(data))
            .count();
        wire_dump = describe_wires(&hs.wires);
        outcome = Some(hs);
    });

    let hs = outcome.expect("handshake pump ran");

    // Contract 3: a storm never lets the medium go quiet — the cap trips.
    assert!(
        hs.quiet,
        "the medium must go quiet within the round cap — a still-busy \
         medium is a forwarding loop (the LRPROOF echo storm).\n\
         --- wires ---\n{wire_dump}\n--- logs ---\n{logs}"
    );

    // Contract 1: the handshake completes at the initiator.
    assert!(
        hs.established[2],
        "the initiator C must establish the link.\n\
         --- wires ---\n{wire_dump}\n--- logs ---\n{logs}"
    );

    // Contract 2: the relay forwards the proof exactly once (no loss here).
    assert_eq!(
        beta_proof_tx, 1,
        "B must transmit the LRPROOF exactly once — every further copy is \
         an echo Python would silently drop (Transport.py:2176).\n\
         --- wires ---\n{wire_dump}\n--- logs ---\n{logs}"
    );
}
