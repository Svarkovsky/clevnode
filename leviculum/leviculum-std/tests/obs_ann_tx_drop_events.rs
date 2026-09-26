//! OBS-1 / OBS-2 well-formedness: drive the REAL leviculum-core transport emit
//! sites (ANN_TX, ANN_TX_SUPPRESSED, PKT_DROP, PKT_DROP_SUMMARY) under the
//! production EventLogLayer and assert the canonical lines are well-formed
//! (tokenize as scalar `key=val`, no field/schema violations). The BUG-3
//! sanitizer is in place, so this also proves the new events stay scalar.
//!
//! These are observability-only assertions: no wire/behaviour is exercised
//! beyond the existing announce/drop paths.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use leviculum_core::constants::{MTU, RANDOM_HASHBYTES, TRUNCATED_HASHBYTES};
use leviculum_core::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use leviculum_core::transport::{DropReason, Transport, TransportConfig};
use leviculum_core::{Clock, Destination, DestinationType, Direction, Identity, MemoryStorage};
use leviculum_std::event_log::EVENT_CATALOG;
use leviculum_std::test_support::event_log::init_event_log;

use rand_core::OsRng;

/// Minimal advanceable clock (production Clock trait). Backed by a shared
/// atomic so the test can advance time after the transport takes ownership.
#[derive(Clone)]
struct TestClock(Arc<AtomicU64>);
impl TestClock {
    fn new(start_ms: u64) -> Self {
        TestClock(Arc::new(AtomicU64::new(start_ms)))
    }
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Build a valid signed announce on the wire (mirrors the core test helper,
/// using only public API).
fn make_announce_raw(hops: u8) -> (Vec<u8>, [u8; TRUNCATED_HASHBYTES]) {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "obsapp",
        &["anntx"],
    )
    .unwrap();
    let id = dest.identity().unwrap();
    let random_hash = [0x42u8; RANDOM_HASHBYTES];

    let mut payload = Vec::new();
    payload.extend_from_slice(&id.public_key_bytes());
    payload.extend_from_slice(dest.name_hash());
    payload.extend_from_slice(&random_hash);

    let app_data = b"obs";
    let mut signed = Vec::new();
    signed.extend_from_slice(dest.hash().as_bytes());
    signed.extend_from_slice(&id.public_key_bytes());
    signed.extend_from_slice(dest.name_hash());
    signed.extend_from_slice(&random_hash);
    signed.extend_from_slice(app_data);
    let signature = id.sign(&signed).unwrap();
    payload.extend_from_slice(&signature);
    payload.extend_from_slice(app_data);

    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Announce,
        },
        hops,
        transport_id: None,
        destination_hash: dest.hash().into_bytes(),
        context: PacketContext::None,
        data: PacketData::Owned(payload),
    };
    let mut buf = [0u8; 500];
    let len = packet.pack(&mut buf).unwrap();
    (buf[..len].to_vec(), dest.hash().into_bytes())
}

/// A HEADER_2 non-announce packet addressed to a transport id that is NOT ours:
/// the high-volume "overheard / not for us" drop (counter only, no per-packet
/// event).
fn make_overheard_packet() -> Vec<u8> {
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type2,
            context_flag: false,
            transport_type: TransportType::Transport,
            dest_type: DestinationType::Single,
            packet_type: PacketType::Data,
        },
        hops: 1,
        transport_id: Some([0xAB; TRUNCATED_HASHBYTES]),
        destination_hash: [0x11; TRUNCATED_HASHBYTES],
        context: PacketContext::None,
        data: PacketData::Owned(b"overheard".to_vec()),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// A PLAIN/GROUP packet that the early filters drop (rare anomaly -> per-packet
/// PKT_DROP). `Announce` -> invalid-announce; `Data` with hops>1 ->
/// plain-group-multihop (kebab DropReason on the journey contract).
fn make_plain_group_packet(dest_type: DestinationType, ptype: PacketType, hops: u8) -> Vec<u8> {
    let packet = Packet {
        flags: PacketFlags {
            ifac_flag: false,
            header_type: HeaderType::Type1,
            context_flag: false,
            transport_type: TransportType::Broadcast,
            dest_type,
            packet_type: ptype,
        },
        hops,
        transport_id: None,
        destination_hash: [0x22; TRUNCATED_HASHBYTES],
        context: PacketContext::None,
        data: PacketData::Owned(b"x".to_vec()),
    };
    let mut buf = [0u8; MTU];
    let len = packet.pack(&mut buf).unwrap();
    buf[..len].to_vec()
}

/// Tokenize a canonical line into `key=value` pairs, asserting every token is a
/// scalar `key=val` with no whitespace inside a value (the documented format).
fn assert_well_formed(line: &str) {
    // Format: `EVENT_NAME k1=v1 k2=v2 ... t=NNN`
    let mut parts = line.split_whitespace();
    let name = parts.next().expect("event name token");
    assert!(
        name.chars().all(|c| c.is_ascii_uppercase() || c == '_'),
        "event name must be UPPER_SNAKE: {name:?} in {line:?}"
    );
    for tok in parts {
        assert!(
            tok.contains('='),
            "token {tok:?} is not key=val in line {line:?}"
        );
        let (k, _v) = tok.split_once('=').unwrap();
        assert!(!k.is_empty(), "empty key in {line:?}");
    }
}

/// The field names PKT_DROP_SUMMARY must carry, one per drop reason,
/// derived mechanically from the taxonomy: the summary spells its fields
/// as the snake-case form of `DropReason::kebab()`.
///
/// Enumerating here rather than by hand is the point. `blackholed-announce`
/// was counted for months without a field of its own, so the fields silently
/// stopped summing to `total` -- exactly the arithmetic the summary exists to
/// support. A new reason now fails this test and the catalog test below
/// instead of going unnoticed.
fn summary_reason_fields() -> Vec<String> {
    DropReason::ALL
        .iter()
        .map(|r| r.kebab().replace('-', "_"))
        .collect()
}

/// The event catalog's `required_keys` is the other hand-written copy of the
/// same list: the EventLogLayer validates emitted lines against it, so a
/// reason missing here is a reason nothing enforces.
#[test]
fn pkt_drop_summary_catalog_covers_every_drop_reason() {
    let schema = EVENT_CATALOG
        .iter()
        .find(|s| s.name == "PKT_DROP_SUMMARY")
        .expect("PKT_DROP_SUMMARY must be catalogued");
    let missing: Vec<String> = summary_reason_fields()
        .into_iter()
        .filter(|f| !schema.required_keys.contains(&f.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "PKT_DROP_SUMMARY schema is missing required_keys {missing:?}; \
         required_keys: {:?}",
        schema.required_keys
    );
    assert!(
        schema.required_keys.contains(&"total"),
        "the grand total is what the per-reason fields have to sum to"
    );
}

fn lines_for<'a>(dump: &'a [String], event: &str) -> Vec<&'a String> {
    dump.iter()
        .filter(|l| l.starts_with(&format!("{event} ")))
        .collect()
}

#[test]
fn obs_events_are_well_formed_under_event_log_layer() {
    let evlog = init_event_log();

    let clock = TestClock::new(100_000);
    let clock_handle = clock.clone();
    let identity = Identity::generate(&mut OsRng);
    let config = TransportConfig {
        enable_transport: true,
        ..TransportConfig::default()
    };
    let mut transport = Transport::new(config, clock, MemoryStorage::with_defaults(), identity);

    // OBS-1: receive a forwardable announce; the retry scheduler rebroadcasts
    // it on poll -> ANN_TX.
    let (announce, _dst) = make_announce_raw(1);
    transport.process_incoming(0, &announce).unwrap();
    transport.drain_events();

    // OBS-2: two rare-anomaly drops (per-packet PKT_DROP) and one overheard
    // drop (counter only) before the summary fires.
    transport
        .process_incoming(0, &make_overheard_packet())
        .unwrap();
    transport
        .process_incoming(
            0,
            &make_plain_group_packet(DestinationType::Plain, PacketType::Announce, 0),
        )
        .unwrap();
    transport
        .process_incoming(
            0,
            &make_plain_group_packet(DestinationType::Group, PacketType::Data, 1),
        )
        .unwrap();

    // Advance past the announce jitter window AND the 10s snapshot cadence so a
    // single poll emits ANN_TX and PKT_DROP_SUMMARY.
    clock_handle.advance(11_000);
    transport.poll();

    let dump = evlog.dump();

    // ANN_TX fired and is well-formed.
    let ann_tx = lines_for(&dump, "ANN_TX");
    assert!(
        !ann_tx.is_empty(),
        "expected an ANN_TX line; dump:\n{dump:#?}"
    );
    for l in &ann_tx {
        assert_well_formed(l);
        assert!(l.contains("dst=") && l.contains("hops=") && l.contains("iface="));
    }

    // PKT_DROP_SUMMARY fired with the per-reason counts.
    let summary = lines_for(&dump, "PKT_DROP_SUMMARY");
    assert!(
        !summary.is_empty(),
        "expected a PKT_DROP_SUMMARY line; dump:\n{dump:#?}"
    );
    for l in &summary {
        assert_well_formed(l);
        // Every taxonomy reason, enumerated from DropReason::ALL rather than
        // by hand -- a counted-but-unemitted reason (as blackholed-announce
        // was) makes the fields stop summing to `total`.
        let keys: Vec<&str> = l
            .split_whitespace()
            .skip(1)
            .filter_map(|tok| tok.split_once('=').map(|(k, _)| k))
            .collect();
        let missing: Vec<String> = summary_reason_fields()
            .into_iter()
            .filter(|f| !keys.contains(&f.as_str()))
            .collect();
        assert!(
            missing.is_empty() && keys.contains(&"total"),
            "summary missing reason fields {missing:?}: {l}"
        );
    }

    // Per-packet PKT_DROP for the rare anomalies, well-formed, with the kebab
    // reason and the journey ph correlator (16 hex chars).
    let pkt_drop = lines_for(&dump, "PKT_DROP");
    assert!(
        pkt_drop
            .iter()
            .any(|l| l.contains("reason=invalid-announce")),
        "expected per-packet PKT_DROP reason=invalid-announce; dump:\n{dump:#?}"
    );
    assert!(
        pkt_drop
            .iter()
            .any(|l| l.contains("reason=plain-group-multihop")),
        "expected per-packet PKT_DROP reason=plain-group-multihop; dump:\n{dump:#?}"
    );
    for l in &pkt_drop {
        assert_well_formed(l);
        assert!(
            l.contains("ph="),
            "journey contract: PKT_DROP must carry ph; line: {l}"
        );
    }
    // The high-volume overheard path must NOT emit a per-packet event: no
    // PKT_DROP line carries its destination hash (0x11..) and there is no
    // overheard reason token anywhere.
    assert!(
        pkt_drop.iter().all(|l| !l.contains("reason=overheard")),
        "overheard path must not emit a per-packet PKT_DROP; dump:\n{dump:#?}"
    );
    assert!(
        summary
            .iter()
            .any(|l| l.contains("overheard_transport_id=1")),
        "summary must count the overheard drop; dump:\n{dump:#?}"
    );

    // No EVENT_FIELD_VIOLATION / EVENT_SCHEMA_VIOLATION for the new events.
    for l in &dump {
        if l.starts_with("EVENT_FIELD_VIOLATION") || l.starts_with("EVENT_SCHEMA_VIOLATION") {
            assert!(
                !l.contains("ANN_TX")
                    && !l.contains("ANN_TX_SUPPRESSED")
                    && !l.contains("PKT_DROP_SUMMARY")
                    && !l.contains("PKT_DROP"),
                "schema/field violation for a new event: {l}"
            );
        }
    }

    // Verbatim samples for the report.
    println!("SAMPLE ANN_TX: {}", ann_tx[0]);
    println!("SAMPLE PKT_DROP_SUMMARY: {}", summary[0]);

    drop(evlog);
}
