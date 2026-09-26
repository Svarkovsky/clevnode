//! ble_lora_transport mvr: three daemons start together — do their probe
//! announces enter the shared channel in the same instant?
//!
//! ## The observation this answers
//!
//! Rig scenario ble_lora_transport (#255), runs 2026-08-31T21-44-18Z and
//! the 22:44 firmware-bisect run: step 1 (`wait_for_path host ->
//! t114.probe`) red because the t114 daemon's probe announce never reached
//! the pocket side. The daemon DID emit — `serial_0 TX 167 bytes` at
//! +15.002 s after startup in every red run, the board's `SER RX 167`
//! confirms ingest, and the board's rebroadcast is visible as the
//! daemon-silent own-echo serial frame. It died ON THE AIR: periculum
//! starts all three lnsd daemons within ~10 ms of each other, each
//! schedules its first management announce at exactly
//! `now + MGMT_ANNOUNCE_INITIAL_DELAY_MS` (15 000 ms sharp), so three
//! announces plus their per-hop rebroadcast ladders tile the same
//! ~1 s window of a half-duplex LoRa channel without carrier sense.
//! Measured overlaps of the t114 probe announce's LoRa transmissions with
//! pocket-board transmissions: 21:37:56.66–57.04 vs pocket TX to 56.97 and
//! 21:38:02.25–02.56 vs pocket TX 02.25–02.57 (run 21-44); 22:44:45.84–46.16
//! vs 45.83–46.15 and 22:44:50.89–51.21 vs 50.89–51.21 (run 22-44, on
//! green-era board firmware — the collision is daemon-phase-driven, not a
//! firmware regression). After the retries the destination is silent until
//! the 2-hour management interval; the cell's 300 s wait cannot succeed.
//!
//! ## What is pinned
//!
//! 1. Co-started nodes must NOT share one first-announce instant: the
//!    schedule carries per-node jitter drawn from the node's own RNG
//!    (red before the fix: every node's deadline was exactly +15 000).
//! 2. The jitter stays inside a bounded window, and the announce actually
//!    egresses on network interfaces when the deadline fires — the
//!    daemon-side emission path that run evidence proved was never broken
//!    (the reviewer's trace saw nothing only because the scenario's
//!    RUST_LOG kept `leviculum_core::node` at INFO while the emission
//!    line logged at DEBUG).
//!
//! Python comparison: Transport.py:283 seeds `last_mgmt_announce` to
//! `start − interval + 15` and the :963 job loop fires it, i.e. Python is
//! just as sharp — it survives on RNode-firmware CSMA, which LNode boards
//! do not run. Timing-only deviation, wire format and semantics untouched.
//!
//! Sans-I/O: no LoRa, no Docker, deterministic seeded RNGs, sub-second.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::vec::Vec;

use rand_core::{CryptoRng, RngCore};

use crate::memory_storage::MemoryStorage;
use crate::node::{NodeCore, NodeCoreBuilder};
use crate::packet::{Packet, PacketType};
use crate::test_utils::{MockClock, MockInterface, TEST_TIME_MS};
use crate::transport::Action;

/// Deterministic xorshift64* RNG so each "daemon" is a fixed, replayable
/// node. Not cryptographically strong — test-only, the `CryptoRng` marker
/// is required by `NodeCore`'s bound.
struct SeededRng(u64);

impl RngCore for SeededRng {
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for SeededRng {}

type SeededNode = NodeCore<SeededRng, MockClock, MemoryStorage>;

/// One lnsd as the scenario runs it: transport enabled, probe responder on,
/// one network interface, clock starting at the shared orchestrator instant.
fn make_daemon(seed: u64) -> SeededNode {
    let mut node = NodeCoreBuilder::new()
        .enable_transport(true)
        .respond_to_probes(true)
        .build(
            SeededRng(seed),
            MockClock::new(TEST_TIME_MS),
            MemoryStorage::with_defaults(),
        );
    let idx = node
        .transport
        .register_interface(Box::new(MockInterface::new("net", 1)));
    node.set_interface_name(idx, String::from("net"));
    node
}

const BASE_MS: u64 = 15_000;
const JITTER_MS: u64 = 5_000;

/// Direction 1 (red before the fix): three co-started daemons must not all
/// schedule their first probe announce for the identical millisecond.
#[test]
fn mvr_costarted_probe_announces_are_dephased() {
    let deadlines: Vec<u64> = [1u64, 2, 3]
        .iter()
        .map(|&seed| {
            make_daemon(seed)
                .next_deadline()
                .expect("probe responder schedules a mgmt announce deadline")
        })
        .collect();

    for &d in &deadlines {
        assert!(
            (TEST_TIME_MS + BASE_MS..TEST_TIME_MS + BASE_MS + JITTER_MS).contains(&d),
            "deadline {d} outside the 15s+jitter window"
        );
    }

    assert!(
        !(deadlines[0] == deadlines[1] && deadlines[1] == deadlines[2]),
        "all three daemons would announce in the same instant ({}): the \
         ble_lora_transport phase lock — rebroadcasts of simultaneous \
         announces collide burst-for-burst on a half-duplex channel",
        deadlines[0]
    );
}

/// Direction 2 (green before and after — pins the refuted hypothesis): a
/// daemon that registers a probe responder DOES emit its announce on the
/// network interfaces when the first management deadline fires. The #255
/// misdiagnosis claimed this path went silent; the run evidence (serial TX
/// at +15.002 s in every red run) says it never did.
#[test]
fn mvr_probe_announce_egresses_at_first_mgmt_deadline() {
    let mut node = make_daemon(7);
    let probe_hash = *node.probe_dest_hash().expect("probe destination");

    let deadline = node
        .next_deadline()
        .expect("probe responder schedules a mgmt announce deadline");
    node.transport().clock().set(deadline);
    let out = node.handle_timeout();

    let announced = out.actions.iter().any(|a| {
        let (Action::Broadcast { data, .. } | Action::SendPacket { data, .. }) = a;
        match Packet::unpack(data) {
            Ok(pkt) => {
                pkt.flags.packet_type == PacketType::Announce
                    && pkt.destination_hash == probe_hash.into_bytes()
                    && pkt.hops == 0
            }
            Err(_) => false,
        }
    });
    assert!(
        announced,
        "the probe destination's announce must egress on network interfaces \
         at the first management deadline"
    );
}
