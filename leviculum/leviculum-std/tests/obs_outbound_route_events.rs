//! Codeberg #365 observability: the outbound routing decision is stated
//! once per originated packet, so a log can distinguish "sent to a live
//! carrier", "withheld because the path's interface is offline" and "no
//! path at all". Drives the REAL `Transport::send_to_destination` emit
//! sites (OUTBOUND_ROUTE, OUTBOUND_WITHHELD) under the production
//! EventLogLayer and asserts the canonical lines are well-formed. The
//! firmware's `[TELEMETRY] send` / `report withheld` lines are the same
//! statement on the debug port (docs/src/structured-event-logs.md).

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use leviculum_core::constants::{RANDOM_HASHBYTES, TRUNCATED_HASHBYTES};
use leviculum_core::packet::{
    HeaderType, Packet, PacketContext, PacketData, PacketFlags, PacketType, TransportType,
};
use leviculum_core::transport::{Transport, TransportConfig};
use leviculum_core::{Clock, Destination, DestinationType, Direction, Identity, MemoryStorage};
use leviculum_std::test_support::event_log::init_event_log;

use rand_core::OsRng;

/// Minimal fixed clock (production Clock trait).
#[derive(Clone)]
struct TestClock(Arc<AtomicU64>);
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Build a valid signed direct announce on the wire (same helper shape
/// as obs_ann_tx_drop_events).
fn make_announce_raw() -> (Vec<u8>, [u8; TRUNCATED_HASHBYTES]) {
    let identity = Identity::generate(&mut OsRng);
    let dest = Destination::new(
        Some(identity),
        Direction::In,
        DestinationType::Single,
        "obsapp",
        &["outroute"],
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
        hops: 0,
        transport_id: None,
        destination_hash: dest.hash().into_bytes(),
        context: PacketContext::None,
        data: PacketData::Owned(payload),
    };
    let mut buf = [0u8; 500];
    let len = packet.pack(&mut buf).unwrap();
    (buf[..len].to_vec(), dest.hash().into_bytes())
}

fn lines_for<'a>(dump: &'a [String], event: &str) -> Vec<&'a String> {
    dump.iter()
        .filter(|l| l.starts_with(&format!("{event} ")))
        .collect()
}

fn keys_of(line: &str) -> Vec<&str> {
    line.split_whitespace()
        .skip(1)
        .filter_map(|tok| tok.split_once('=').map(|(k, _)| k))
        .collect()
}

#[test]
fn outbound_route_and_withheld_lines_are_emitted_and_well_formed() {
    let evlog = init_event_log();

    let clock = TestClock(Arc::new(AtomicU64::new(100_000)));
    let identity = Identity::generate(&mut OsRng);
    let config = TransportConfig {
        enable_transport: true,
        ..TransportConfig::default()
    };
    let mut transport = Transport::new(config, clock, MemoryStorage::with_defaults(), identity);
    transport.set_interface_name(0, "ble0".into());

    // Learn a direct path on interface 0, then originate a packet to it:
    // the routing decision must be stated as OUTBOUND_ROUTE online=y.
    let (announce, dst) = make_announce_raw();
    transport.process_incoming(0, &announce).unwrap();
    transport.drain_events();
    transport
        .send_to_destination(&dst, b"\x00\x00report")
        .expect("send with a live path");

    // The interface goes offline: the same send must be refused and the
    // refusal stated as OUTBOUND_WITHHELD reason=iface-offline.
    transport.set_interface_online(0, false);
    assert!(
        transport
            .send_to_destination(&dst, b"\x00\x00report")
            .is_err(),
        "a path over an offline interface must not route (Codeberg #365)"
    );

    let dump = evlog.dump();

    let route = lines_for(&dump, "OUTBOUND_ROUTE");
    assert_eq!(
        route.len(),
        1,
        "exactly one routing line for one originated packet; dump:\n{dump:#?}"
    );
    let keys = keys_of(route[0]);
    for k in ["dst", "iface", "next_hop", "online"] {
        assert!(
            keys.contains(&k),
            "OUTBOUND_ROUTE missing {k}: {}",
            route[0]
        );
    }
    assert!(
        route[0].contains("iface=ble0") && route[0].contains("online=y"),
        "the line names the carrier and its state: {}",
        route[0]
    );

    let withheld = lines_for(&dump, "OUTBOUND_WITHHELD");
    assert_eq!(
        withheld.len(),
        1,
        "exactly one withheld line for the refused send; dump:\n{dump:#?}"
    );
    let keys = keys_of(withheld[0]);
    for k in ["dst", "iface", "next_hop", "reason"] {
        assert!(
            keys.contains(&k),
            "OUTBOUND_WITHHELD missing {k}: {}",
            withheld[0]
        );
    }
    assert!(
        withheld[0].contains("reason=iface-offline"),
        "the refusal names its reason: {}",
        withheld[0]
    );

    leviculum_std::assert_no_schema_violations!(evlog);
}
