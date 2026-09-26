//! Tests for the `lnstatus` renderer.
//!
//! The centrepiece is a GOLDEN-OUTPUT suite: `tests_data/lnstatus_golden.json`
//! is produced by `tests_gen/gen_lnstatus_golden.py`, which drives the REAL
//! vendored Python `rnstatus.program_setup` over fixed `interface_stats` dicts
//! and captures its exact stdout. Each case carries the rpc-style stats JSON
//! (bytes -> lowercase hex, as `rpc_query` yields) and the Python ground-truth
//! string; `render_status` must reproduce it byte-for-byte. That is the drop-in
//! parity the issue requires, pinned without a live daemon.
//!
//! The remaining tests exercise the pretty-formatters against known Python
//! outputs, plus sorting / filtering / `-j` structure directly.

use super::*;
use serde_json::Value;

const GOLDEN: &str = include_str!("../../tests_data/lnstatus_golden.json");

fn opts_from_json(o: &Value) -> StatusOptions {
    StatusOptions {
        dispall: o["dispall"].as_bool().unwrap(),
        astats: o["astats"].as_bool().unwrap(),
        pstats: o["pstats"].as_bool().unwrap(),
        lstats: o["lstats"].as_bool().unwrap(),
        burst_filter: o["burst_filter"].as_bool().unwrap(),
        totals: o["totals"].as_bool().unwrap(),
        sort: o["sort"].as_str().map(String::from),
        reverse: o["reverse"].as_bool().unwrap(),
        name_filter: o["name_filter"].as_str().map(String::from),
    }
}

#[test]
fn golden_output_matches_rnstatus() {
    let cases: Vec<Value> = serde_json::from_str(GOLDEN).expect("parse golden json");
    assert!(cases.len() >= 20, "expected a broad golden suite");
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let opts = opts_from_json(&case["opts"]);
        let stats = &case["stats"];
        let link_count = case["link_count"].as_i64();
        let expected = case["expected"].as_str().unwrap();
        let got = render_status(stats, link_count, &opts);
        if got != expected {
            failures.push(format!(
                "case `{name}` mismatch:\n--- expected ---\n{expected:?}\n--- got ---\n{got:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn golden_json_structure_roundtrips() {
    // `-j` must emit valid JSON that parses back to the same structure the
    // daemon returned (byte-exact JSON parity is out of scope; structure is).
    let cases: Vec<Value> = serde_json::from_str(GOLDEN).unwrap();
    for case in &cases {
        let stats = &case["stats"];
        let emitted = render_json(stats);
        let reparsed: Value = serde_json::from_str(&emitted).expect("emitted json parses");
        assert_eq!(&reparsed, stats, "case `{}` json roundtrip", case["name"]);
    }
}

// ---------------------------------------------------------------------------
// Pretty-formatter unit tests (ground truth = Python RNS)
// ---------------------------------------------------------------------------

#[test]
fn prettysize_bytes() {
    // RNS.prettysize: no-unit uses %.0f, scaled uses %.2f.
    assert_eq!(prettysize(0.0, false), "0 B");
    assert_eq!(prettysize(999.0, false), "999 B");
    assert_eq!(prettysize(1024.0, false), "1.02 KB");
    assert_eq!(prettysize(123456.0, false), "123.46 KB");
    assert_eq!(prettysize(1_000_000.0, false), "1.00 MB");
}

#[test]
fn prettyspeed_bits() {
    // RNS.prettyspeed(num) = prettysize(num/8, suffix="b")+"ps"; input is bytes/s.
    assert_eq!(prettyspeed(0.0), "0 bps");
    assert_eq!(prettyspeed(42.0), "42 bps");
    assert_eq!(prettyspeed(1500.0), "1.50 Kbps");
    assert_eq!(prettyspeed(125.0), "125 bps");
    assert_eq!(prettyspeed(1000.0), "1.00 Kbps");
}

#[test]
fn speed_str_bitrate() {
    // rnstatus.speed_str default suffix "bps", lowercase k.
    assert_eq!(speed_str(10_000_000.0), "10.00 Mbps");
    assert_eq!(speed_str(1_000_000_000.0), "1.00 Gbps");
    assert_eq!(speed_str(9600.0), "9.60 kbps");
}

#[test]
fn prettyfrequency_d1_lpf_cases() {
    assert_eq!(prettyfrequency_d1_lpf(0.0), "0 Hz");
    assert_eq!(prettyfrequency_d1_lpf(0.5), "0.5 Hz");
    assert_eq!(prettyfrequency_d1_lpf(1500.0), "1.5 KHz");
    assert_eq!(prettyfrequency_d1_lpf(375.0), "375.0 Hz");
}

#[test]
fn prettytime_cases() {
    assert_eq!(prettytime(0.0), "0s");
    assert_eq!(prettytime(3600.0), "1h");
    assert_eq!(prettytime(3661.0), "1h, 1m and 1.0s");
    assert_eq!(prettytime(90.0), "1m and 30.0s");
    assert_eq!(prettytime(86400.0 + 3600.0), "1d and 1h");
}

#[test]
fn py_round2_trims_like_python() {
    assert_eq!(py_round2_str(5.0), "5.0");
    assert_eq!(py_round2_str(5.2), "5.2");
    assert_eq!(py_round2_str(5.25), "5.25");
    assert_eq!(py_round2_str(1.0), "1.0");
}

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

fn iface(name: &str, bitrate: i64, rxb: i64, txb: i64) -> Value {
    serde_json::json!({
        "name": name, "type": "AutoInterface", "status": true, "mode": 1,
        "bitrate": bitrate, "rxb": rxb, "txb": txb, "rxs": 0.0, "txs": 0.0,
        "clients": null, "peers": null,
        "incoming_announce_frequency": 0.0, "outgoing_announce_frequency": 0.0,
        "incoming_pr_frequency": 0.0, "outgoing_pr_frequency": 0.0,
        "held_announces": 0, "announce_queue": null,
        "burst_active": false, "pr_burst_active": false,
        "ifac_signature": null, "ifac_size": null, "ifac_netname": null
    })
}

fn names_after_sort(mut ifaces: Vec<Value>, sort: &str, reverse: bool) -> Vec<String> {
    sort_interfaces(&mut ifaces, sort, reverse);
    ifaces
        .iter()
        .map(|i| i["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn sort_rate_descending_by_default() {
    let v = vec![
        iface("a", 10, 0, 0),
        iface("b", 1000, 0, 0),
        iface("c", 100, 0, 0),
    ];
    // reverse = not sort_reverse: default descending.
    assert_eq!(
        names_after_sort(v.clone(), "rate", false),
        vec!["b", "c", "a"]
    );
    // -r => ascending.
    assert_eq!(names_after_sort(v, "rate", true), vec!["a", "c", "b"]);
}

#[test]
fn sort_traffic_sums_rx_tx() {
    let v = vec![
        iface("a", 0, 1, 1),   // 2
        iface("b", 0, 10, 10), // 20
        iface("c", 0, 5, 0),   // 5
    ];
    assert_eq!(names_after_sort(v, "traffic", false), vec!["b", "c", "a"]);
}

#[test]
fn sort_unknown_field_is_noop() {
    let v = vec![iface("a", 10, 0, 0), iface("b", 1000, 0, 0)];
    assert_eq!(names_after_sort(v, "bogus", false), vec!["a", "b"]);
}

#[test]
fn sort_is_stable_on_equal_keys() {
    let v = vec![
        iface("first", 5, 0, 0),
        iface("second", 5, 0, 0),
        iface("third", 5, 0, 0),
    ];
    assert_eq!(
        names_after_sort(v, "rate", false),
        vec!["first", "second", "third"]
    );
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

#[test]
fn positional_filter_is_case_insensitive_substring() {
    let stats = serde_json::json!({
        "interfaces": [ iface("AutoInterface[Alpha]", 10, 0, 0),
                        iface("AutoInterface[Beta]", 10, 0, 0) ],
        "rxb": 0, "txb": 0, "rxs": 0.0, "txs": 0.0, "rss": null
    });
    let opts = StatusOptions {
        name_filter: Some("beta".to_string()),
        ..Default::default()
    };
    let out = render_status(&stats, None, &opts);
    assert!(out.contains("AutoInterface[Beta]"));
    assert!(!out.contains("AutoInterface[Alpha]"));
}

#[test]
fn burst_filter_shows_only_active_burst_interfaces() {
    // -B: keep only interfaces with an active burst (both burst flags present)
    // or a name match. This is the filter the golden suite pins deterministically
    // (the "burst for <elapsed>" duration is wall-clock dependent and only
    // renders under -A/-P, so it is not asserted here).
    let mut active = iface("AutoInterface[Hot]", 10, 0, 0);
    active["burst_active"] = Value::Bool(true);
    let stats = serde_json::json!({
        "interfaces": [ active, iface("AutoInterface[Cold]", 10, 0, 0) ],
        "rxb": 0, "txb": 0, "rxs": 0.0, "txs": 0.0, "rss": null
    });
    let out = render_status(
        &stats,
        None,
        &StatusOptions {
            burst_filter: true,
            ..Default::default()
        },
    );
    assert!(out.contains("AutoInterface[Hot]"));
    assert!(!out.contains("AutoInterface[Cold]"));
}

#[test]
fn active_burst_renders_duration_line_under_astats() {
    // The burst suffix ("burst for ...") appears on the announce line only with
    // -A. We can't pin the exact elapsed (wall-clock) so we assert the marker.
    let mut active = iface("AutoInterface[Hot]", 10, 0, 0);
    active["burst_active"] = Value::Bool(true);
    active["pr_burst_active"] = Value::Bool(true);
    let stats = serde_json::json!({
        "interfaces": [ active ],
        "rxb": 0, "txb": 0, "rxs": 0.0, "txs": 0.0, "rss": null
    });
    let out = render_status(
        &stats,
        None,
        &StatusOptions {
            astats: true,
            pstats: true,
            ..Default::default()
        },
    );
    assert!(
        out.contains("burst for"),
        "expected burst suffix, got:\n{out}"
    );
}

#[test]
fn default_hides_local_and_client_interfaces() {
    let stats = serde_json::json!({
        "interfaces": [ iface("LocalInterface[shared]", 10, 0, 0),
                        iface("AutoInterface[X]", 10, 0, 0) ],
        "rxb": 0, "txb": 0, "rxs": 0.0, "txs": 0.0, "rss": null
    });
    let hidden = render_status(&stats, None, &StatusOptions::default());
    assert!(!hidden.contains("LocalInterface[shared]"));
    let shown = render_status(
        &stats,
        None,
        &StatusOptions {
            dispall: true,
            ..Default::default()
        },
    );
    assert!(shown.contains("LocalInterface[shared]"));
}

#[test]
fn last_rssi_snr_render_when_present_and_omit_when_absent() {
    // The daemon emits `last_rssi`/`last_snr` only once the radio has
    // reported a packet (radio_stat_fields gates on `Some`), so the render
    // contract is presence-keyed: keys present -> the two labelled lines,
    // keys absent -> neither line, and `-j` passes the dict through as-is.
    let mut with = iface("RNodeInterface[/dev/ttyUSB0]", 292, 0, 0);
    with["last_rssi"] = serde_json::json!(-42);
    with["last_snr"] = serde_json::json!(9.5);
    let without = iface("RNodeInterface[/dev/ttyUSB1]", 292, 0, 0);
    let stats = serde_json::json!({
        "interfaces": [with, without],
        "rxb": 0, "txb": 0, "rxs": 0.0, "txs": 0.0, "rss": null
    });

    let text = render_status(&stats, None, &StatusOptions::default());
    let blocks: Vec<&str> = text.split("RNodeInterface[").collect();
    assert_eq!(blocks.len(), 3, "both interfaces render:\n{text}");
    assert!(blocks[1].contains("    Last RSSI : -42 dBm\n"), "{text}");
    assert!(blocks[1].contains("    Last SNR  : 9.5 dB\n"), "{text}");
    assert!(!blocks[2].contains("Last RSSI"), "{text}");
    assert!(!blocks[2].contains("Last SNR"), "{text}");

    let json: Value = serde_json::from_str(&render_json(&stats)).unwrap();
    let ifs = json["interfaces"].as_array().unwrap();
    assert_eq!(ifs[0]["last_rssi"], serde_json::json!(-42));
    assert_eq!(ifs[0]["last_snr"], serde_json::json!(9.5));
    assert!(ifs[1].get("last_rssi").is_none());
    assert!(ifs[1].get("last_snr").is_none());
}

#[test]
fn tx_queue_drops_render_when_positive_and_stay_silent_at_zero() {
    // Codeberg #318: `tx_queue_drops` is served unconditionally by lnsd,
    // but the labelled line renders only for a non-zero count — the same
    // stay-silent-at-zero contract as the Queued/Held announce counters,
    // so a healthy plain run stays byte-identical to rnstatus (and rnsd,
    // which never emits the key, renders no line either way). `-j`
    // passes the key through untouched.
    let mut dropping = iface("RNodeInterface[/dev/ttyUSB0]", 292, 0, 0);
    dropping["tx_queue_drops"] = serde_json::json!(7);
    let mut clean = iface("RNodeInterface[/dev/ttyUSB1]", 292, 0, 0);
    clean["tx_queue_drops"] = serde_json::json!(0);
    let rnsd_shaped = iface("RNodeInterface[/dev/ttyUSB2]", 292, 0, 0);
    let stats = serde_json::json!({
        "interfaces": [dropping, clean, rnsd_shaped],
        "rxb": 0, "txb": 0, "rxs": 0.0, "txs": 0.0, "rss": null
    });

    let text = render_status(&stats, None, &StatusOptions::default());
    let blocks: Vec<&str> = text.split("RNodeInterface[").collect();
    assert_eq!(blocks.len(), 4, "all three interfaces render:\n{text}");
    assert!(
        blocks[1].contains("    TX drops  : 7 frames\n"),
        "a shedding interface names its loss:\n{text}"
    );
    assert!(
        !blocks[2].contains("TX drops"),
        "zero stays silent:\n{text}"
    );
    assert!(
        !blocks[3].contains("TX drops"),
        "an rnsd entry without the key renders no line:\n{text}"
    );

    let json: Value = serde_json::from_str(&render_json(&stats)).unwrap();
    let ifs = json["interfaces"].as_array().unwrap();
    assert_eq!(ifs[0]["tx_queue_drops"], serde_json::json!(7));
    assert_eq!(ifs[1]["tx_queue_drops"], serde_json::json!(0));
    assert!(ifs[2].get("tx_queue_drops").is_none());
}

// ---------------------------------------------------------------------------
// -d / -D discovered interfaces (Codeberg #32)
// ---------------------------------------------------------------------------

/// One RNode discovered-interface record as `rpc_query` yields it (bytes are
/// already hex strings; ids as hex; timestamps as epoch seconds).
fn rnode_record() -> Value {
    serde_json::json!({
        "type": "RNodeInterface",
        "transport": true,
        "name": "Node A",
        "received": 1000.0,
        "value": 15,
        "transport_id": "abababababababababababababababab",
        "network_id": "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
        "hops": 2,
        "latitude": 52.5,
        "longitude": 13.4,
        "height": null,
        "frequency": 867200000,
        "bandwidth": 125000,
        "sf": 8,
        "cr": 5,
        "config_entry": "[[Node A]]\n  type = RNodeInterface\n  enabled = yes\n  port = \n  frequency = 867200000\n  bandwidth = 125000\n  spreadingfactor = 8\n  codingrate = 5\n  txpower = ",
        "discovered": 1000.0,
        "last_heard": 1000.0,
        "heard_count": 0,
        "status": "available",
        "status_code": 1000
    })
}

#[test]
fn discovered_detail_renders_python_lines() {
    let list = Value::Array(vec![rnode_record()]);
    let out = render_discovered(&list, true, None, 1000.0);
    // transport_id != network_id -> both shown.
    assert!(out.contains("Network   ID : cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd\n"));
    assert!(out.contains("Transport ID : abababababababababababababababab\n"));
    assert!(out.contains("Name         : Node A\n"));
    assert!(out.contains("Type         : RNodeInterface\n"));
    assert!(out.contains("Status       : Available\n"));
    assert!(out.contains("Transport    : Enabled\n"));
    assert!(out.contains("Distance     : 2 hops\n"));
    assert!(out.contains("Discovered   : 0s ago\n"));
    assert!(out.contains("Last Heard   : 0s ago\n"));
    assert!(out.contains("Location     : 52.5, 13.4\n"));
    // {:,} thousands grouping.
    assert!(out.contains("Frequency    : 867,200,000 Hz\n"));
    assert!(out.contains("Bandwidth    : 125,000 Hz\n"));
    assert!(out.contains("Sprd. Factor : 8\n"));
    assert!(out.contains("Coding Rate  : 5\n"));
    assert!(out.contains("Stamp Value  : 15\n"));
    // Config entry indented by two spaces per line.
    assert!(out.contains("\nConfiguration Entry:\n"));
    assert!(out.contains("  [[Node A]]\n"));
    assert!(out.contains("  spreadingfactor = 8\n"));
}

#[test]
fn discovered_list_renders_python_columns() {
    let list = Value::Array(vec![rnode_record()]);
    let out = render_discovered(&list, false, None, 1000.0);
    assert!(out.contains(
        "Name                      Type         Status       Last Heard   Value    Location       "
    ));
    assert!(out.contains(&"-".repeat(89)));
    // Type drops the "Interface" suffix; status carries the ✓ glyph; last heard
    // "Just now" for a sub-minute delta.
    assert!(out.contains("Node A"));
    assert!(out.contains("RNode"));
    assert!(out.contains("✓ Available"));
    assert!(out.contains("Just now"));
}

#[test]
fn discovered_list_last_heard_buckets() {
    let mut r = rnode_record();
    r["last_heard"] = serde_json::json!(0.0);
    let list = Value::Array(vec![r]);
    // 2h since last_heard -> "2h ago".
    let out = render_discovered(&list, false, None, 7200.0);
    assert!(out.contains("2h ago"), "got: {out}");
    // 3 days -> "3d ago".
    let mut r2 = rnode_record();
    r2["last_heard"] = serde_json::json!(0.0);
    let out2 = render_discovered(&Value::Array(vec![r2]), false, None, 3.0 * 86400.0);
    assert!(out2.contains("3d ago"));
}

#[test]
fn discovered_name_filter_applies() {
    let mut other = rnode_record();
    other["name"] = serde_json::json!("Other");
    let list = Value::Array(vec![rnode_record(), other]);
    let out = render_discovered(&list, true, Some("node"), 1000.0);
    assert!(out.contains("Name         : Node A\n"));
    assert!(!out.contains("Name         : Other\n"));
}

#[test]
fn discovered_json_passthrough() {
    let list = Value::Array(vec![rnode_record()]);
    let s = render_json(&list);
    let back: Value = serde_json::from_str(&s).unwrap();
    assert_eq!(back, list);
}

#[test]
fn thousands_and_compact_time_match_python() {
    assert_eq!(fmt_thousands(867200000), "867,200,000");
    assert_eq!(fmt_thousands(125000), "125,000");
    assert_eq!(fmt_thousands(5), "5");
    assert_eq!(prettytime_compact(0.0), "0s");
    assert_eq!(prettytime_compact(90.0), "1m and 30s");
    assert_eq!(prettytime_compact(3661.0), "1h and 1m");
    // Compact keeps only the two most-significant components.
    assert_eq!(prettytime_compact(90061.0), "1d and 1h");
}

// --- --tables (Codeberg #174) --------------------------------------------

/// The stats object a daemon returns, trimmed to the parts a reader keys on.
fn stats_object() -> Value {
    serde_json::json!({
        "transport_id": "aabb",
        "transport_uptime": 12.5,
        "interfaces": [{"name": "TCPInterface[peer]", "rxb": 1, "txb": 2}],
    })
}

/// Present: the tables land under one additive key and nothing else moves.
/// The `interfaces` array stays at the top level, which is what every existing
/// `-j` consumer keys on (Periculum's `parse_status` scans for exactly that).
#[test]
fn merge_transport_tables_adds_one_key_and_leaves_the_stats_dict_alone() {
    let before = stats_object();
    let mut stats = stats_object();
    let tables = serde_json::json!({
        "path_table": [{"hash": "11", "hops": 3}],
        "reverse_table": [],
        "link_table": [],
        "announce_table": [],
        "announce_cache": [],
        "tunnels": [],
        "local_links": [],
    });
    merge_transport_tables(&mut stats, Some(tables.clone()));

    assert_eq!(stats[TRANSPORT_TABLES_KEY], tables);
    assert!(
        stats["interfaces"].is_array(),
        "-j consumers key on interfaces"
    );
    let obj = stats.as_object().unwrap();
    for (k, v) in before.as_object().unwrap() {
        assert_eq!(obj.get(k), Some(v), "--tables must not disturb {k}");
    }
    assert_eq!(
        obj.len(),
        before.as_object().unwrap().len() + 1,
        "exactly one key is added"
    );
}

/// Absent: an `rnsd`, or an `lnsd` older than #174, answers no
/// `transport_tables` at all. The merge then leaves the stats dict byte-for-byte
/// as it was — the status question the user asked was answered, and the missing
/// capability must not cost them the answer.
#[test]
fn merge_transport_tables_tolerates_a_daemon_that_cannot_answer() {
    let mut stats = stats_object();
    merge_transport_tables(&mut stats, None);
    assert_eq!(stats, stats_object());
    assert_eq!(render_json(&stats), render_json(&stats_object()));
}

/// Absence and emptiness must not look alike. A daemon that knows the command
/// but holds nothing answers with the key present and every table empty; one
/// that does not know it leaves the key out. A reader distinguishes the two by
/// presence, which is the whole reason the key is omitted rather than nulled.
#[test]
fn an_empty_table_is_distinguishable_from_a_daemon_that_cannot_answer() {
    let empty = serde_json::json!({
        "path_table": [], "reverse_table": [], "link_table": [],
        "announce_table": [], "announce_cache": [], "tunnels": [], "local_links": [],
    });

    let mut knows = stats_object();
    merge_transport_tables(&mut knows, Some(empty));
    let mut cannot = stats_object();
    merge_transport_tables(&mut cannot, None);

    assert!(knows.get(TRANSPORT_TABLES_KEY).is_some());
    assert!(cannot.get(TRANSPORT_TABLES_KEY).is_none());
    assert_ne!(knows, cannot);
    assert_eq!(
        knows[TRANSPORT_TABLES_KEY]["path_table"],
        serde_json::json!([]),
        "an empty path table is an empty list, never an absent key"
    );
}

/// A response that is not an object (an unexpected answer from a foreign
/// daemon) is passed through untouched rather than reshaped.
#[test]
fn merge_transport_tables_leaves_a_non_object_response_alone() {
    let mut stats = Value::Array(vec![Value::from(1)]);
    merge_transport_tables(&mut stats, Some(serde_json::json!({"path_table": []})));
    assert_eq!(stats, Value::Array(vec![Value::from(1)]));
}

// ---------------------------------------------------------------------------
// --identities: derivation and rendering
// ---------------------------------------------------------------------------

/// The derivation pinned against the two identity/destination pairs measured
/// on the rig (bug ledger #365 / field finding 2026-09-04): the T114 and the
/// Pocket, each verified with rnprobe against the live board.
#[test]
fn derive_destination_matches_the_two_known_rig_pairs() {
    let pairs = [
        // (identity, rnstransport.probe, lxmf.delivery)
        (
            "b2a8bea123f668be63e85be2374e26e5",
            "6a1ab9ea64747f298c1f205dfcf0f5a3",
            "be26233976540d7d9e10faf5c396558a",
        ),
        (
            "1d48253ff2dddd5f95e6ef6ce8302a62",
            "d8dd8f1d749a36f71c74a662eb7d531e",
            "2f9a770aa734a6ab02c7e845583cf206",
        ),
    ];
    for (identity_hex, probe_hex, lxmf_hex) in pairs {
        let identity: Vec<u8> = (0..identity_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&identity_hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(
            hex_lower16(&derive_destination("rnstransport.probe", &identity)),
            probe_hex,
            "probe destination for identity {identity_hex}"
        );
        assert_eq!(
            hex_lower16(&derive_destination("lxmf.delivery", &identity)),
            lxmf_hex,
            "lxmf delivery destination for identity {identity_hex}"
        );
    }
}

/// One full row plus its derived line; the honest placeholders for the
/// pathless/unnamed case (`?` name, `-` columns); the empty listing.
#[test]
fn render_identities_rows_and_placeholders() {
    let list = serde_json::json!([
        {
            "identity_hash": "1d48253ff2dddd5f95e6ef6ce8302a62",
            "destination_hash": "2f9a770aa734a6ab02c7e845583cf206",
            "name": null,
            "hops": 2,
            "interface": "lora0",
            "via": "b2a8bea123f668be63e85be2374e26e5",
            "last_seen": 820.0,
        },
        {
            "identity_hash": "b2a8bea123f668be63e85be2374e26e5",
            "destination_hash": "6a1ab9ea64747f298c1f205dfcf0f5a3",
            "name": "rnstransport.probe",
            "hops": null,
            "interface": null,
            "via": null,
            "last_seen": null,
        },
    ]);
    let out = render_identities(&list, 1000.0);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("identity"), "header first: {out}");

    // Row 1: relayed path, unknown name.
    assert!(lines[1].starts_with("1d48253ff2dddd5f95e6ef6ce8302a62"));
    assert!(lines[1].contains("2f9a770aa734a6ab02c7e845583cf206"));
    assert!(lines[1].contains(" ? "), "unknown name shows ?: {out}");
    assert!(
        lines[1].contains("lora0/b2a8bea1"),
        "via is iface/hop: {out}"
    );
    assert!(
        lines[1].contains("3m ago"),
        "180s ago renders compact: {out}"
    );
    assert!(
        lines[2].contains("derived: lxmf.delivery=2f9a770aa734a6ab02c7e845583cf206"),
        "derived lxmf line: {out}"
    );
    assert!(
        lines[2].contains("rnstransport.probe=d8dd8f1d749a36f71c74a662eb7d531e"),
        "derived probe line: {out}"
    );

    // Row 2: no live path -> `-` columns, known name printed.
    assert!(lines[3].contains("rnstransport.probe"));
    assert!(lines[3].contains(" - "), "pathless columns show -: {out}");
    assert!(
        lines[4].contains("rnstransport.probe=6a1ab9ea64747f298c1f205dfcf0f5a3"),
        "derived line under the second row too: {out}"
    );

    let empty = render_identities(&serde_json::json!([]), 1000.0);
    assert!(
        empty.contains("No identities"),
        "empty listing says so: {empty}"
    );
}
