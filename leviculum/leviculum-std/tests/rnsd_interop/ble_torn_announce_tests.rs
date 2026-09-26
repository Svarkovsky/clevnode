//! Telemetry-cycle announces rejected by a Python peer as "Invalid
//! signature" (#255, Leitstern component 1).
//!
//! Evidence (Columba logcat, Fairphone 5, 2026-08-29): the 186 B
//! `lxmf.delivery` announces from a RAK4631 validate all evening, but at
//! 19:11:49 a 211 B and at 19:56:49 a 259 B "announce" is rejected with
//! `Received invalid announce for <2f9a770a…>: Invalid signature.`
//!
//! These tests discriminate the three hypotheses from the batch
//! instruction and pin the actual mechanism:
//!
//! 1. **Signer exonerated** (`firmware_announce_variants_validate_at_python`):
//!    every announce variant the firmware could emit — the real 19 B
//!    `DeliveryAnnounce` app_data, oversized app_data at exactly the
//!    invalid wire sizes, and the ratcheted flags variant — validates
//!    against the vendored Python `RNS.Identity.validate_announce`, the
//!    same code the phone runs. Hypotheses 2 (signed-data assembly
//!    mismatch) and 3 (ratchet/flags span) are refuted; there is also no
//!    "telemetry app_data" announce variant in the firmware at all —
//!    `leviculum-nrf/src/telemetry.rs:709` emits a fixed display-name
//!    app_data, so the announce is always 186 B on the wire.
//!
//! 2. **Mechanism pinned** (`torn_ble_fragment_stream_glues_report_tail_onto_announce`):
//!    the numbers 211 and 259 are not announce sizes; they are
//!    `START(announce frag 0) + END(report frag 1)`. At the BLE MTU the
//!    firmware fragments with (185 → 177 B payload per fragment):
//!    186 = 177 + 9, 211 = 177 + 34, 259 = 177 + 82 — and the telemetry
//!    reports of the two log windows were 211 B (no GNSS fix) and 259 B
//!    (with position), sent immediately after the announce in the same
//!    tick (`leviculum-nrf/src/telemetry.rs:553` then `:583`). When
//!    `PacketTx` (leviculum-nrf/ble-tx) aborts the announce on a
//!    transient HVN drain stall after fragment 0 was already accepted,
//!    the peer's reassembler is left holding a torn head; a reassembler
//!    that keeps the first fragment for a given sequence number (the
//!    phone's observed behaviour — the glued sizes prove it kept
//!    announce frag 0 and ignored the report's START) completes it with
//!    the report's END into a packet that parses as an ANNOUNCE of
//!    exactly the report's length and fails signature validation.
//!    Deterministically: the same bytes are always invalid.
//!
//! 3. **Characterization** (`glued_announce_head_report_tail_is_pythons_invalid_signature`):
//!    the glue artifact reproduces the phone's exact verdict against
//!    vendored Python. Stays green forever; it documents the failure
//!    artifact, not the fix.
//!
//! Red-then-green: test 2 is red while `PacketTx` tears packets on a
//! single stall timeout, and green once a transient stall is retried
//! instead of torn (the fix in leviculum-nrf/ble-tx).

use std::process::Command;

use leviculum_ble_tx::{Action, Event, NotifyOutcome, PacketTx};
use leviculum_core::framing::ble::{
    fragment_packet, payload_per_fragment, DefragResult, DEFAULT_MTU, FRAGMENT_HEADER_SIZE,
    FRAGMENT_TYPE_END, FRAGMENT_TYPE_START,
};
use leviculum_core::identity::Identity;
use leviculum_core::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use leviculum_core::{Destination, DestinationType};
use leviculum_lxmf::announce::DeliveryAnnounce;

/// Path to the vendored Python RNS package (for PYTHONPATH), the same
/// code the phone's validator runs.
const VENDOR_RNS_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../reference/Reticulum");

/// Wire sizes from the phone-side evidence.
const VALID_ANNOUNCE_WIRE: usize = 186;
const INVALID_WIRE_BEFORE_FIX: usize = 211;
const INVALID_WIRE_AFTER_FIX: usize = 259;

/// Feed raw packet bytes to the vendored Python
/// `RNS.Identity.validate_announce` — the exact validator behind the
/// phone's `Invalid signature` log line (Transport.py rejects when it
/// returns False). `only_validate_signature=True` isolates the signature
/// check, which is the check the phone failed.
fn python_validate_announce(raw: &[u8]) -> bool {
    let script = r#"
import sys
import RNS
raw = bytes.fromhex(sys.argv[1])
p = RNS.Packet(None, raw)
p.unpack()
ok = RNS.Identity.validate_announce(p, only_validate_signature=True)
print("VERDICT=VALID" if ok else "VERDICT=INVALID")
"#;
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(hex::encode(raw))
        .env("PYTHONPATH", VENDOR_RNS_ROOT)
        .output()
        .expect("spawn python3 with vendored RNS");
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("VERDICT=VALID") {
        return true;
    }
    assert!(
        stdout.contains("VERDICT=INVALID"),
        "python validator produced no verdict: stdout={stdout:?} stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

/// The firmware's announce app_data, byte-for-byte
/// (`leviculum-nrf/src/telemetry.rs:709`): a `DeliveryAnnounce` with the
/// derived `LNode-<8 hex>` display name, no stamp cost, no compression.
fn firmware_app_data(identity: &Identity) -> Vec<u8> {
    let hash = identity.hash();
    let mut name = Vec::with_capacity(14);
    name.extend_from_slice(b"LNode-");
    for byte in &hash[..4] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        name.push(HEX[(byte >> 4) as usize]);
        name.push(HEX[(byte & 0x0F) as usize]);
    }
    DeliveryAnnounce {
        display_name: Some(name),
        stamp_cost: None,
        compression_supported: false,
    }
    .encode()
}

/// The firmware's `lxmf.delivery` destination
/// (`leviculum-lxmf/src/node.rs:370`, no ratchets), announced through the
/// same core path the firmware uses.
fn delivery_announce_wire(identity: Identity, app_data: &[u8]) -> Vec<u8> {
    let mut dest =
        leviculum_lxmf::LxmfNode::delivery_destination(identity).expect("delivery destination");
    announce_wire(&mut dest, app_data)
}

fn announce_wire(dest: &mut Destination, app_data: &[u8]) -> Vec<u8> {
    let packet = dest
        .announce(Some(app_data), &mut rand_core::OsRng, 12_000, 1_756_400_000)
        .expect("announce");
    let mut buf = [0u8; leviculum_core::constants::MTU];
    let len = packet.pack(&mut buf).expect("pack");
    buf[..len].to_vec()
}

/// An opaque single-destination DATA packet of exactly `wire_len` bytes,
/// standing in for the LXMF telemetry report. Only its length and its
/// position in the fragment stream matter to the mechanism under test.
fn report_stand_in(wire_len: usize) -> Vec<u8> {
    let header = 19; // Type 1 header: flags + hops + dest(16) + context
    let payload: Vec<u8> = (0..wire_len - header).map(|i| (i % 251) as u8).collect();
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: [0x42; 16],
        context: PacketContext::None,
        data: PacketData::Owned(payload),
    };
    let mut buf = [0u8; leviculum_core::constants::MTU];
    let len = packet.pack(&mut buf).expect("pack report stand-in");
    assert_eq!(len, wire_len, "stand-in wire size");
    buf[..len].to_vec()
}

/// Every announce variant the firmware could put on the air validates at
/// the Python peer. Refutes hypotheses 2 and 3: the signer's byte order
/// and the ratcheted flags variant both match `Identity.validate_announce`
/// (reference/Reticulum/RNS/Identity.py:532-579, signed_data =
/// `destination_hash+public_key+name_hash+random_hash+ratchet+app_data`,
/// Identity.py:566), independent of app_data size.
#[test]
fn firmware_announce_variants_validate_at_python() {
    // (a) The real firmware announce: DeliveryAnnounce app_data, 186 B.
    let identity = Identity::generate(&mut rand_core::OsRng);
    let app_data = firmware_app_data(&identity);
    let wire = delivery_announce_wire(identity, &app_data);
    assert_eq!(
        wire.len(),
        VALID_ANNOUNCE_WIRE,
        "the firmware announce is the 186 B packet from the log"
    );
    assert!(
        python_validate_announce(&wire),
        "the 186 B firmware announce must validate (it did all evening)"
    );

    // (b, c) App_data grown so the wire size is exactly the two invalid
    // sizes from the log. If the signer mishandled large app_data, these
    // would fail; they pass, so announce size cannot be the trigger.
    for target in [INVALID_WIRE_BEFORE_FIX, INVALID_WIRE_AFTER_FIX] {
        let identity = Identity::generate(&mut rand_core::OsRng);
        let mut app_data = firmware_app_data(&identity);
        app_data.resize(app_data.len() + (target - VALID_ANNOUNCE_WIRE), 0xA5);
        let wire = delivery_announce_wire(identity, &app_data);
        assert_eq!(wire.len(), target);
        assert!(
            python_validate_announce(&wire),
            "a genuine {target} B announce validates — size is not the trigger"
        );
    }

    // (d) The ratcheted variant (context_flag set, ratchet inside the
    // signed span). The firmware's delivery destination does not enable
    // ratchets, but hypothesis 3 named the flags byte, so pin it too.
    let identity = Identity::generate(&mut rand_core::OsRng);
    let app_data = firmware_app_data(&identity);
    let mut dest =
        leviculum_lxmf::LxmfNode::delivery_destination(identity).expect("delivery destination");
    dest.enable_ratchets(&mut rand_core::OsRng, 1_000)
        .expect("ratchets");
    let wire = announce_wire(&mut dest, &app_data);
    assert!(
        python_validate_announce(&wire),
        "the ratcheted announce variant validates too"
    );

    // Positive control: the validator must be able to say no. A flipped
    // signature byte (last byte before app_data) must fail.
    let identity = Identity::generate(&mut rand_core::OsRng);
    let app_data = firmware_app_data(&identity);
    let mut wire = delivery_announce_wire(identity, &app_data);
    let sig_end = wire.len() - app_data.len() - 1;
    wire[sig_end] ^= 0x01;
    assert!(
        !python_validate_announce(&wire),
        "positive control: a tampered signature must be rejected"
    );
}

/// A reassembler that keeps the first fragment seen for a sequence
/// number, modeling the phone's observed behaviour. The glued sizes in
/// the logcat prove the phone kept announce frag 0 across the report's
/// START (a reset-on-START reassembler would have delivered the report
/// intact and logged no invalid announce; ours,
/// `leviculum_core::framing::ble::BleDefragmenter`, resets). Completion
/// on fragment count, like every implementation of this protocol.
struct KeepFirstReassembler {
    fragments: std::collections::BTreeMap<u16, Vec<u8>>,
    expected_total: u16,
}

impl KeepFirstReassembler {
    fn new() -> Self {
        Self {
            fragments: std::collections::BTreeMap::new(),
            expected_total: 0,
        }
    }

    fn process(&mut self, fragment: &[u8]) -> Option<Vec<u8>> {
        let seq = u16::from_be_bytes([fragment[1], fragment[2]]);
        let total = u16::from_be_bytes([fragment[3], fragment[4]]);
        let payload = fragment[FRAGMENT_HEADER_SIZE..].to_vec();
        if self.fragments.is_empty() {
            self.expected_total = total;
        }
        self.fragments.entry(seq).or_insert(payload);
        if self.fragments.len() == usize::from(self.expected_total) {
            let packet = core::mem::take(&mut self.fragments)
                .into_values()
                .flatten()
                .collect();
            self.expected_total = 0;
            return Some(packet);
        }
        None
    }
}

/// Drive one packet's fragments through `PacketTx` against a scripted
/// HVN queue, exactly as `leviculum-nrf/src/ble.rs::notify_fragments`
/// does. `stall_timeouts` injects that many `WaitTimedOut` events (a
/// transient drain stall: the drain arrives, but later than
/// `DRAIN_WAIT_MS`) before the queue behaves again. Returns the
/// fragments the queue accepted, in order.
fn deliver(fragments: &[Vec<u8>], stall_timeouts: &mut usize) -> Vec<Vec<u8>> {
    // One-deep queue, S140 default: a fragment occupies the slot until
    // the drain event for it fires.
    let mut in_flight = 0usize;
    let mut accepted = Vec::new();
    let (mut tx, mut action) = PacketTx::start(fragments.len());
    for _ in 0..10_000 {
        match action {
            Action::Send { index } => {
                let outcome = if in_flight >= 1 {
                    NotifyOutcome::QueueFull
                } else {
                    in_flight += 1;
                    accepted.push(fragments[index].clone());
                    NotifyOutcome::Sent
                };
                action = tx.step(Event::Notify(outcome));
            }
            Action::AwaitDrain { .. } => {
                let event = if *stall_timeouts > 0 {
                    *stall_timeouts -= 1;
                    Event::WaitTimedOut
                } else {
                    in_flight = 0;
                    Event::Drained
                };
                action = tx.step(event);
            }
            Action::Done | Action::Abort { .. } | Action::Nothing => return accepted,
        }
    }
    panic!("PacketTx did not terminate");
}

/// The mechanism, red-then-green. A single transient stall while sending
/// announce fragment 1 must not tear the packet: the peer must end up
/// with the valid announce AND the intact report. While `PacketTx`
/// aborts on the first `WaitTimedOut`, the peer instead reassembles one
/// 259 B pseudo-announce — the phone's log line.
#[test]
fn torn_ble_fragment_stream_glues_report_tail_onto_announce() {
    let identity = Identity::generate(&mut rand_core::OsRng);
    let app_data = firmware_app_data(&identity);
    let announce = delivery_announce_wire(identity, &app_data);
    let report = report_stand_in(INVALID_WIRE_AFTER_FIX);

    let announce_frags = fragment_packet(&announce, DEFAULT_MTU);
    let report_frags = fragment_packet(&report, DEFAULT_MTU);
    assert_eq!(announce_frags.len(), 2, "186 B is 2 fragments at MTU 185");
    assert_eq!(report_frags.len(), 2, "259 B is 2 fragments at MTU 185");

    // The telemetry tick: announce first (telemetry.rs:553), report
    // right behind it (telemetry.rs:583). One transient stall while the
    // announce's second fragment waits for the queue to drain.
    let mut stalls = 1usize;
    let mut on_air = deliver(&announce_frags, &mut stalls);
    on_air.extend(deliver(&report_frags, &mut stalls));

    let mut phone = KeepFirstReassembler::new();
    let packets: Vec<Vec<u8>> = on_air.iter().filter_map(|f| phone.process(f)).collect();

    // Desired end state: both packets arrive intact. Today this fails:
    // PacketTx aborts the announce at fragment 1, the phone glues
    // announce frag 0 + report frag 1 into one 259 B pseudo-announce.
    assert_eq!(
        packets.len(),
        2,
        "the peer must reassemble two packets, got {}: sizes {:?}",
        packets.len(),
        packets.iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert_eq!(packets[0], announce, "first packet is the intact announce");
    assert_eq!(packets[1], report, "second packet is the intact report");
    assert!(
        python_validate_announce(&packets[0]),
        "the reassembled announce validates at the Python peer"
    );
}

/// Characterization of the failure artifact (stays green): the glue of
/// announce frag 0 and report frag 1 is a packet that parses as an
/// ANNOUNCE of exactly the report's wire size and draws exactly the
/// phone's verdict from the Python validator. This is the discriminating
/// experiment for the logcat numbers 211 and 259.
#[test]
fn glued_announce_head_report_tail_is_pythons_invalid_signature() {
    for report_len in [INVALID_WIRE_BEFORE_FIX, INVALID_WIRE_AFTER_FIX] {
        let identity = Identity::generate(&mut rand_core::OsRng);
        let app_data = firmware_app_data(&identity);
        let announce = delivery_announce_wire(identity, &app_data);
        let report = report_stand_in(report_len);

        let announce_frags = fragment_packet(&announce, DEFAULT_MTU);
        let report_frags = fragment_packet(&report, DEFAULT_MTU);
        assert_eq!(announce_frags[0][0], FRAGMENT_TYPE_START);
        assert_eq!(report_frags[1][0], FRAGMENT_TYPE_END);

        // What the phone reassembled: the announce's START head, the
        // report's END tail (announce frag 1 torn away by the sender,
        // report START ignored by keep-first).
        let ppf = payload_per_fragment(DEFAULT_MTU);
        let mut glued = announce[..ppf].to_vec();
        glued.extend_from_slice(&report[ppf..]);
        assert_eq!(
            glued.len(),
            report_len,
            "the glue has exactly the report's wire size — the logcat sizes"
        );

        // It parses as an announce for the delivery destination…
        let parsed = Packet::unpack(&glued).expect("the glue parses as a packet");
        assert_eq!(parsed.flags.packet_type, PacketType::Announce);
        assert_eq!(
            parsed.destination_hash,
            Packet::unpack(&announce).unwrap().destination_hash
        );

        // …and the Python peer rejects it: "Invalid signature."
        assert!(
            !python_validate_announce(&glued),
            "the glued packet is the phone's invalid announce"
        );

        // Cross-check with our own defragmenter: the same three
        // fragments produce no invalid announce here — reset-on-START
        // yields the intact report instead. The corruption is created by
        // the sender-side tear; the receiver policy only picks which
        // packet survives.
        let mut ours = leviculum_core::framing::ble::BleDefragmenter::new();
        let mut recovered = Vec::new();
        for frag in [&announce_frags[0], &report_frags[0], &report_frags[1]] {
            if let DefragResult::Complete(p) = ours.process(frag, 1_000) {
                recovered.push(p);
            }
        }
        assert_eq!(recovered, vec![report.clone()]);
    }
}
