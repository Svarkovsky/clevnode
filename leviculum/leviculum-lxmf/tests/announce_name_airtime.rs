//! The airtime the node name costs, pinned (Codeberg #235).
//!
//! `leviculum_core::node_name::NODE_NAME_MAX_LEN` is 32 bytes, and its
//! module docs derive that number from a table of announce sizes and
//! airtimes. This file is that table, executed: a name rides in **every**
//! LXMF delivery announce over LoRa, so the bound is airtime policy, and a
//! policy stated only in prose drifts the first time the encoding changes.
//!
//! Here rather than in `leviculum-core` because the numbers need both
//! halves: the LXMF `app_data` encoding
//! ([`leviculum_lxmf::announce::DeliveryAnnounce`]) and core's announce
//! geometry and airtime formula. `leviculum-nrf`'s `announce_app_data`,
//! which builds the real thing, cross-compiles and runs no host tests, so
//! this is the closest a host test gets to the frame that goes on the air.
//!
//! If one of these numbers moves, the announce got bigger or cheaper and
//! `node_name`'s table has to move with it — that is the point.

use leviculum_core::announce_payload_fixed_len;
use leviculum_core::constants::HEADER_MINSIZE;
use leviculum_core::node_name::NODE_NAME_MAX_LEN;
use leviculum_core::rnode::airtime_ms_with_preamble;
use leviculum_lxmf::announce::DeliveryAnnounce;

/// The `app_data` the firmware builds for a display name of `len` bytes:
/// `DeliveryAnnounce { display_name: Some(..), stamp_cost: None,
/// compression_supported: false }`, byte for byte
/// (`leviculum_nrf::telemetry::announce_app_data`).
fn app_data_len(len: usize) -> usize {
    DeliveryAnnounce {
        display_name: Some(vec![b'x'; len]),
        stamp_cost: None,
        compression_supported: false,
    }
    .encode()
    .len()
}

/// The whole announce on the air for a name of `len` bytes. No ratchet:
/// the LNode's delivery destination announces without one.
fn announce_len(len: usize) -> usize {
    HEADER_MINSIZE + announce_payload_fixed_len(false) + app_data_len(len)
}

/// SF7/BW125/CR4:5 at the modem-default 8-symbol preamble — the fast end
/// of the band, and the configuration a bench measurement runs at.
fn sf7_ms(bytes: usize) -> u64 {
    airtime_ms_with_preamble(bytes as u32, 125_000, 7, 5, 8)
}

/// SF12/BW125/CR4:8 at the RNode-derived 18-symbol preamble — the slow
/// end, and where the bound actually bites. The same modulation
/// `leviculum_nrf::lora` sizes its transmit timeout against.
fn sf12_ms(bytes: usize) -> u64 {
    airtime_ms_with_preamble(bytes as u32, 125_000, 12, 8, 18)
}

#[test]
fn the_app_data_costs_five_bytes_plus_the_name() {
    // One fixarray header, a two-byte bin8 header, the name, a nil stamp
    // cost, an empty capability array. The `+5` the whole derivation
    // rests on.
    for len in [0, 8, 11, 14, NODE_NAME_MAX_LEN] {
        assert_eq!(app_data_len(len), 5 + len, "name of {len} bytes");
    }
}

#[test]
fn the_announce_is_one_hundred_and_seventy_two_bytes_plus_the_name() {
    assert_eq!(HEADER_MINSIZE, 19);
    assert_eq!(announce_payload_fixed_len(false), 148);
    for len in [0, 14, NODE_NAME_MAX_LEN] {
        assert_eq!(announce_len(len), 172 + len, "name of {len} bytes");
    }
}

#[test]
fn the_table_in_the_node_name_docs_is_these_numbers() {
    // `leviculum_core::node_name`, "Why the length is airtime politics".
    // Each row: (name bytes, announce bytes, SF7 ms, SF12/CR4:8 ms).
    //
    // The first row is the management announce, which carries no
    // `app_data` at all — it is the 167 B figure the rig measures and the
    // baseline the name's cost is counted from.
    assert_eq!(
        (
            HEADER_MINSIZE + announce_payload_fixed_len(false),
            sf7_ms(167),
            sf12_ms(167)
        ),
        (167, 272, 9_905)
    );
    for (name, bytes, sf7, sf12) in [
        (14usize, 186usize, 298u64, 10_953u64),
        (NODE_NAME_MAX_LEN, 204, 323, 11_740),
        (64, 236, 369, 13_575),
    ] {
        assert_eq!(announce_len(name), bytes, "name of {name} bytes");
        assert_eq!(sf7_ms(bytes), sf7, "SF7 airtime of a {bytes} B announce");
        assert_eq!(sf12_ms(bytes), sf12, "SF12 airtime of a {bytes} B announce");
    }
}

#[test]
fn the_cap_costs_twenty_five_milliseconds_at_sf7_and_under_a_second_at_sf12() {
    // The headline numbers of the "what it costs" section: the price of
    // an operator using the whole cap, against today's 14-byte derived
    // default.
    let default = announce_len(14);
    let capped = announce_len(NODE_NAME_MAX_LEN);
    assert_eq!(sf7_ms(capped) - sf7_ms(default), 25);
    assert_eq!(sf12_ms(capped) - sf12_ms(default), 787);
}

#[test]
fn the_worst_hourly_bill_stays_under_a_twentieth_of_the_duty_cycle() {
    // The bound the cap was actually chosen against. The EU 1 % duty
    // cycle is 36 s of airtime per hour per band; the busiest cadence a
    // name rides on is the tracker profile's 60 s floor
    // (`leviculum-nrf/telemetry-policy`, `min_interval_ms`), one delivery
    // announce each, i.e. 60 announces per hour — which is only a sane
    // configuration at the fast spreading factors.
    const DUTY_CYCLE_MS_PER_HOUR: u64 = 36_000;
    const TRACKER_ANNOUNCES_PER_HOUR: u64 = 60;
    const STATION_ANNOUNCES_PER_HOUR: u64 = 1;

    let extra_sf7 = sf7_ms(announce_len(NODE_NAME_MAX_LEN)) - sf7_ms(announce_len(14));
    let extra_sf12 = sf12_ms(announce_len(NODE_NAME_MAX_LEN)) - sf12_ms(announce_len(14));

    let tracker_sf7 = extra_sf7 * TRACKER_ANNOUNCES_PER_HOUR;
    let station_sf12 = extra_sf12 * STATION_ANNOUNCES_PER_HOUR;
    assert_eq!(tracker_sf7, 1_500, "SF7 tracker floor, ms/hour");
    assert_eq!(station_sf12, 787, "SF12 station cadence, ms/hour");

    // Both under 5 % of the hourly allowance. Doubling the cap would
    // double this bill, which is the argument the docs make for 32.
    for (label, cost) in [("sf7-tracker", tracker_sf7), ("sf12-station", station_sf12)] {
        assert!(
            cost * 100 / DUTY_CYCLE_MS_PER_HOUR < 5,
            "{label}: {cost} ms/hour is {} % of the duty cycle",
            cost * 100 / DUTY_CYCLE_MS_PER_HOUR
        );
    }
}

#[test]
fn a_name_at_the_cap_is_nowhere_near_the_announce_mtu() {
    // The bound is policy, not a wire limit — stated in the docs and
    // asserted here so nobody defends the number as "it would not fit".
    assert!(
        app_data_len(NODE_NAME_MAX_LEN) < leviculum_core::announce_app_data_budget(true),
        "even with a ratchet the announce has room to spare"
    );
}
