//! The airtime arithmetic, held to the numbers periculum publishes.
//!
//! # Why this file exists
//!
//! periculum used to call straight into `leviculum_core::rnode` for LoRa
//! airtime, the derived preamble and the ETSI duty-cycle table. It no longer
//! does: those ~1000 lines now also live in `periculum-wire`, so that
//! repository builds from a solo clone instead of demanding this checkout
//! (and, through it, the whole dalek stack) for arithmetic that uses no
//! crypto at all.
//!
//! Two copies can drift, and a drifting duty model is a regulatory question,
//! not a style question — the numbers below decide whether a bench run is
//! lawful. So both copies are nailed to the same published figures from both
//! sides:
//!
//! * here: this file, against `leviculum_core::rnode`;
//! * there: `periculum/tests/doc_claims.rs` and `periculum/tests/duty_budget.rs`,
//!   against `periculum_wire::rnode`.
//!
//! The figures are the ones printed in periculum's `hardware/README.md`
//! (the airtime table and the bench duty-cycle budget) and quoted in the
//! `lora_path_discovery_*` scenario files. **If a change here makes this file
//! fail, the same change is owed to `periculum-wire`, and vice versa** —
//! that is the whole point of the pair. Never edit one side's expectations
//! to make it pass.

use leviculum_core::rnode::{airtime_ms, airtime_ms_with_preamble, derive_preamble_symbols};

/// The bench PHY: everything below is BW125 with CR4:8.
const BW: u32 = 125_000;
const CR: u8 = 8;

/// The preamble the firmware derives, and the one every on-air figure below
/// is charged at. `hardware/README.md` says the table charges 18 symbols;
/// periculum's `doc_claims.rs` asserts the same derivation returns 18.
#[test]
fn the_derived_preamble_is_18_symbols_at_the_bench_phy() {
    assert_eq!(derive_preamble_symbols(10, CR, BW), 18);
    assert_eq!(derive_preamble_symbols(12, CR, BW), 18);
}

/// The SF10 airtime table of `hardware/README.md`, to the millisecond.
///
/// The seconds in brackets are the cells the document prints; periculum
/// asserts the printed strings, this asserts the milliseconds they round
/// from, so a sub-10-ms drift that the 2-decimal cell would hide still
/// fails here.
#[test]
fn the_sf10_airtime_figures_are_the_ones_periculum_publishes() {
    let air = |bytes: u32| airtime_ms_with_preamble(bytes, BW, 10, CR, 18);
    assert_eq!(air(131), 2_018, "probe, `PKT_TX len=131` (2.02 s)");
    assert_eq!(air(115), 1_821, "its proof, `PKT_TX len=115` (1.82 s)");
    assert_eq!(air(167), 2_477, "announce (2.48 s)");
    assert_eq!(air(51), 969, "path request (0.97 s)");
}

/// The 500 B pacing basis, which is charged at the modem-default 8-symbol
/// preamble because that is what the executor's model uses — the one figure
/// in the table that is not an on-air number. The tracked benchmark
/// documents recorded `min_interval_ms = 4 * this`.
#[test]
fn the_500_byte_pacing_basis_is_the_preamble_8_figure() {
    assert_eq!(airtime_ms(500, BW, 10, CR), 6_786, "6.79 s");
    assert_eq!(
        airtime_ms(500, BW, 10, CR),
        airtime_ms_with_preamble(500, BW, 10, CR, 8),
        "airtime_ms is the preamble-8 case; periculum relies on that identity"
    );
}

/// The SF12 cell that `lora_path_discovery_slowest_mixed.toml` carries its
/// duty arithmetic in, and that `periculum/tests/duty_budget.rs` recomputes:
/// 3.875 s per path request, 9.905 s per announce, 21 worst-case round trips
/// = 289.4 s of channel time.
#[test]
fn the_sf12_duty_budget_cell_is_the_one_the_corpus_quotes() {
    let air = |bytes: u32| airtime_ms_with_preamble(bytes, BW, 12, CR, 18);
    assert_eq!(air(51), 3_875, "path request");
    assert_eq!(air(167), 9_905, "announce");
    assert_eq!(
        21 * air(51) + 21 * air(167),
        289_380,
        "the 289.4 s of channel time the scenario file's comment quotes"
    );
}

/// The regulatory limit the budget is measured against. 869.525 MHz is ETSI
/// sub-band P (h1.7), 10% of the rolling hour; `duty_budget.rs` asserts
/// `limit_pct == 10.0` and a 360 s budget from exactly this row.
#[test]
fn the_bench_frequency_is_the_ten_percent_sub_band() {
    let fraction =
        leviculum_core::rnode::etsi_eu868_duty_cycle(869_525_000).expect("869.525 MHz is listed");
    assert_eq!(fraction, 0.10);
    assert_eq!((fraction * 3_600_000.0) as u64, 360_000, "360 s per hour");
}
