//! Rendering + formatting for the native `lnstatus` binary (Codeberg #86,
//! Stage 1: local mode).
//!
//! This matches the local-mode output format of Python
//! `RNS/Utilities/rnstatus.py` plus the size/speed/frequency/time formatting
//! from `RNS/__init__.py`. The renderer consumes the JSON that
//! `leviculum_std::rpc_query("interface_stats")` returns (msgpack maps decoded
//! to objects, `bin` values to lowercase hex, `nil` to null), which is the
//! same `interface_stats` dict a Python `rnsd` exposes. Feeding an identical
//! stats dict here and into rnstatus yields byte-identical output — that is the
//! drop-in goal, and the golden-output tests pin it.
//!
//! All character-width padding mirrors Python `len()`, which counts code
//! points; we therefore count `chars()`, never bytes, so the arrow glyphs
//! (`↓`/`↑`) and `µ` align exactly as in rnstatus.

use std::fmt::Write as _;

use serde_json::Value;

/// Interface `mode` enum values (RNS/Interfaces/Interface.py:45-50).
const MODE_POINT_TO_POINT: i64 = 0x02;
const MODE_ACCESS_POINT: i64 = 0x03;
const MODE_ROAMING: i64 = 0x04;
const MODE_BOUNDARY: i64 = 0x05;
const MODE_GATEWAY: i64 = 0x06;

/// Options controlling what `render_status` shows. One field per rnstatus flag
/// that affects local-mode rendering.
#[derive(Debug, Default, Clone)]
pub struct StatusOptions {
    /// `-a/--all`: show interfaces normally hidden (local/client/peer entries).
    pub dispall: bool,
    /// `-A/--announce-stats`: show announce frequencies / rate / queued / held.
    pub astats: bool,
    /// `-P/--pr-stats`: show path-request frequencies.
    pub pstats: bool,
    /// `-l/--link-stats`: append the link-table count to the trailer.
    pub lstats: bool,
    /// `-B/--burst`: only show interfaces with an active burst (or name match).
    pub burst_filter: bool,
    /// `-t/--totals`: print the aggregate traffic totals line.
    pub totals: bool,
    /// `-s/--sort <field>`: sort order; `None` leaves daemon order.
    pub sort: Option<String>,
    /// `-r/--reverse`: reverse the sort direction.
    pub reverse: bool,
    /// positional filter: only show interfaces whose name contains this.
    pub name_filter: Option<String>,
}

// ---------------------------------------------------------------------------
// Python-compatible number formatting
// ---------------------------------------------------------------------------

/// Character count (Python `len()` on a `str` counts code points).
fn clen(s: &str) -> usize {
    s.chars().count()
}

/// `str(round(x, 1))` — `round(x, 1)` is always a one-decimal multiple, so its
/// `str()` shows exactly one decimal (`str(2.0) == "2.0"`, `str(1.5) == "1.5"`).
/// Rust's `{:.1}` uses the same round-half-to-even rule as CPython's `round()`.
fn py_round1_str(x: f64) -> String {
    format!("{x:.1}")
}

/// `str(round(x, 2))` — like `{:.2}` but with Python's trailing-zero trimming:
/// `str(5.0) == "5.0"`, `str(5.2) == "5.2"`, `str(5.25) == "5.25"`. `round`
/// yields at most two decimals; `str` drops a single trailing hundredths zero
/// while keeping at least one decimal digit.
fn py_round2_str(x: f64) -> String {
    let s = format!("{x:.2}");
    if let Some(stripped) = s.strip_suffix('0') {
        // "5.20" -> "5.2", "5.00" -> "5.0" (keep the tenths digit).
        stripped.to_string()
    } else {
        s
    }
}

/// `RNS.prettysize` (RNS/__init__.py:191). `bits=true` corresponds to
/// `suffix='b'` (multiplies by 8); otherwise bytes (`suffix='B'`).
fn prettysize(mut num: f64, bits: bool) -> String {
    let suffix = if bits { "b" } else { "B" };
    if bits {
        num *= 8.0;
    }
    for unit in ["", "K", "M", "G", "T", "P", "E", "Z"] {
        if num.abs() < 1000.0 {
            if unit.is_empty() {
                return format!("{num:.0} {suffix}");
            }
            return format!("{num:.2} {unit}{suffix}");
        }
        num /= 1000.0;
    }
    format!("{num:.2}Y{suffix}")
}

/// `RNS.prettyspeed` (RNS/__init__.py:188): bits-per-second string.
fn prettyspeed(num: f64) -> String {
    format!("{}ps", prettysize(num / 8.0, true))
}

/// `RNS.prettyfrequency(hz, d=1, lpf=True)` — the only call shape rnstatus uses
/// for interface announce/path-request frequencies (RNS/__init__.py:208).
fn prettyfrequency_d1_lpf(hz: f64) -> String {
    if hz == 0.0 {
        return "0 Hz".to_string();
    }
    let mut num = hz;
    for unit in ["", "K", "M", "G", "T", "P", "E", "Z"] {
        if num.abs() < 1000.0 {
            return format!("{} {unit}Hz", py_round1_str(num));
        }
        num /= 1000.0;
    }
    format!("{num:.2}YHz")
}

/// `speed_str` from rnstatus.py:756 (default `suffix='bps'`, no /8). Used for
/// the interface bitrate ("Rate" line). Note lowercase `k` for kilo.
fn speed_str(mut num: f64) -> String {
    for unit in ["", "k", "M", "G", "T", "P", "E", "Z"] {
        if num.abs() < 1000.0 {
            return format!("{num:.2} {unit}bps");
        }
        num /= 1000.0;
    }
    format!("{num:.2} Ybps")
}

/// `RNS.prettytime(time)` — non-verbose, non-compact (RNS/__init__.py:239).
/// Days/hours/minutes render as integers with `d/h/m`; seconds as
/// `str(round(x, 2)) + "s"`. Components join with `, ` and a final ` and `.
fn prettytime(mut t: f64) -> String {
    let neg = t < 0.0;
    if neg {
        t = t.abs();
    }
    let days = (t / 86400.0).floor() as i64;
    t %= 86400.0;
    let hours = (t / 3600.0).floor() as i64;
    t %= 3600.0;
    let minutes = (t / 60.0).floor() as i64;
    t %= 60.0;
    let seconds = t; // rounded to 2 decimals at format time

    let mut components: Vec<String> = Vec::new();
    if days > 0 {
        components.push(format!("{days}d"));
    }
    if hours > 0 {
        components.push(format!("{hours}h"));
    }
    if minutes > 0 {
        components.push(format!("{minutes}m"));
    }
    // Python: `if seconds > 0` where seconds == round(t, 2).
    if (seconds * 100.0).round() as i64 > 0 {
        components.push(format!("{}s", py_round2_str(seconds)));
    }

    if components.is_empty() {
        return "0s".to_string();
    }
    let n = components.len();
    let mut tstr = String::new();
    for (i, c) in components.iter().enumerate() {
        if i == 0 {
        } else if i < n - 1 {
            tstr.push_str(", ");
        } else {
            tstr.push_str(" and ");
        }
        tstr.push_str(c);
    }
    if neg {
        format!("-{tstr}")
    } else {
        tstr
    }
}

/// `RNS.prettyhexrep` over an already-hex string (rpc_query decodes `bin` to
/// lowercase hex): `<hex>`.
fn prettyhexrep_from_hex(hex: &str) -> String {
    format!("<{hex}>")
}

// ---------------------------------------------------------------------------
// JSON field accessors
// ---------------------------------------------------------------------------

/// Key present at all (Python `"key" in ifstat`, true even if the value is
/// null, matching a decoded `None`).
fn has(v: &Value, k: &str) -> bool {
    v.get(k).is_some()
}

/// Value present and not null (Python `ifstat["key"] != None`).
fn not_null(v: &Value, k: &str) -> bool {
    matches!(v.get(k), Some(x) if !x.is_null())
}

/// Read a numeric field as f64. rpc_query renders big ints as decimal strings,
/// so accept a JSON number or a numeric string.
fn jf(v: &Value, k: &str) -> Option<f64> {
    match v.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

/// Read a numeric field as i64 (number or decimal string).
fn ji(v: &Value, k: &str) -> Option<i64> {
    match v.get(k)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

fn jb(v: &Value, k: &str) -> Option<bool> {
    v.get(k)?.as_bool()
}

fn js<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k)?.as_str()
}

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

/// Sort key for a `-s/--sort <field>` value, or `None` for an unrecognised
/// field (rnstatus then leaves the order untouched).
fn sort_key(iface: &Value, field: &str) -> Option<f64> {
    let g = |k: &str| jf(iface, k).unwrap_or(0.0);
    Some(match field {
        "rate" | "bitrate" => g("bitrate"),
        "rx" => g("rxb"),
        "tx" => g("txb"),
        "rxs" => g("rxs"),
        "txs" => g("txs"),
        "traffic" => g("rxb") + g("txb"),
        "announces" | "announce" => {
            g("incoming_announce_frequency") + g("outgoing_announce_frequency")
        }
        "arx" => g("incoming_announce_frequency"),
        "atx" => g("outgoing_announce_frequency"),
        "prx" => g("incoming_pr_frequency"),
        "ptx" => g("outgoing_pr_frequency"),
        "held" => g("held_announces"),
        _ => return None,
    })
}

/// In-place sort matching `interfaces.sort(key=..., reverse=not sort_reverse)`:
/// descending by default, ascending with `-r`. Stable (equal keys keep order),
/// like Python's `list.sort`.
fn sort_interfaces(interfaces: &mut [Value], sort: &str, reverse: bool) {
    let field = sort.to_lowercase();
    // No-op for an unrecognised field (probe the first entry / any entry).
    if interfaces
        .first()
        .and_then(|i| sort_key(i, &field))
        .is_none()
        && interfaces.iter().all(|i| sort_key(i, &field).is_none())
    {
        return;
    }
    interfaces.sort_by(|a, b| {
        let ka = sort_key(a, &field).unwrap_or(0.0);
        let kb = sort_key(b, &field).unwrap_or(0.0);
        let ord = ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal);
        // reverse = not sort_reverse: default descending.
        if reverse {
            ord
        } else {
            ord.reverse()
        }
    });
}

// ---------------------------------------------------------------------------
// -j / --json
// ---------------------------------------------------------------------------

/// `-j`: emit the raw stats dict. rnstatus prints `json.dumps(stats)` after
/// hex-encoding byte values; rpc_query has already hex-encoded them, so this is
/// the structural equivalent. (Byte-for-byte JSON parity is out of scope per
/// the issue: key order / separators differ between Python and serde.)
pub fn render_json(stats: &Value) -> String {
    serde_json::to_string(stats).unwrap_or_else(|_| "null".to_string())
}

/// The key `--tables` adds to the `-j` object (Codeberg #174).
pub const TRANSPORT_TABLES_KEY: &str = "transport_tables";

/// `--tables`: fold the `transport_tables` RPC response into the `-j` stats
/// object, under one additive key.
///
/// The shape is deliberate on both sides of the read:
///
/// * **Additive, not an envelope.** The stats dict stays the top-level object,
///   so anything that parses `lnstatus -j` today — Periculum's `parse_status`
///   scans for the line whose object carries `interfaces` — keeps working
///   unchanged, and `-j` without `--tables` is byte-identical to before.
/// * **Absence is not emptiness.** `tables == None` means the daemon did not
///   answer the question (a Python `rnsd`, or an `lnsd` older than #174: both
///   match no arm for the command and close the connection), and the key is
///   then *omitted*. A daemon that does know the question answers with the
///   key present and its per-table lists possibly empty. A reader can tell
///   "cannot ask" from "nothing there" by presence alone; if it could not,
///   every assertion about an empty table would silently also pass against a
///   daemon that cannot answer it. This is the read-side tolerance question of
///   Codeberg #183, one layer up.
///
/// `stats` is left untouched when it is not a JSON object (an `rnsd` that
/// answered something unexpected), which keeps this a pure merge with no
/// opinion about the rest of the response.
pub fn merge_transport_tables(stats: &mut Value, tables: Option<Value>) {
    let (Some(obj), Some(tables)) = (stats.as_object_mut(), tables) else {
        return;
    };
    obj.insert(TRANSPORT_TABLES_KEY.to_string(), tables);
}

// ---------------------------------------------------------------------------
// -N / --identities  identities learned from announces (Leviculum extension)
// ---------------------------------------------------------------------------

/// Derive a destination hash from a dotted destination name and a 16-byte
/// identity hash:
///
/// `dest = sha256(sha256(name)[:10] || identity_hash)[:16]`
///
/// The inner truncation is the 10-byte name hash (`RNS.Destination.hash`,
/// Destination.py:114-122); the outer is the usual 16-byte truncated hash.
/// This is what turns the identity hash an announce reveals into the
/// `rnstransport.probe` / `lxmf.delivery` destination a prober needs.
pub fn derive_destination(full_name: &str, identity_hash: &[u8]) -> [u8; 16] {
    use sha2::Digest;
    let name_hash = sha2::Sha256::digest(full_name.as_bytes());
    let mut hasher = sha2::Sha256::new();
    hasher.update(&name_hash[..10]);
    hasher.update(identity_hash);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// The destinations worth deriving for every listed identity: the probe
/// responder every transport node runs, and the LXMF delivery destination.
const DERIVED_NAMES: [&str; 2] = ["lxmf.delivery", "rnstransport.probe"];

/// `--identities`: render the `identities` RPC response (a list of dicts, see
/// `build_identity_table` in leviculum-std) as one row per learned identity
/// plus a `derived:` line with the destinations computed from its identity
/// hash. Hashes are printed in full so they can be pasted into `lnprobe`.
/// Columns the daemon could not fill honestly show `-`; an unknown name shows
/// `?` (the daemon only knows names for aspects it registered itself).
pub fn render_identities(list: &Value, now_epoch: f64) -> String {
    let mut out = String::new();
    let empty = Vec::new();
    let rows = list.as_array().unwrap_or(&empty);
    if rows.is_empty() {
        pln(&mut out, "No identities have been learned from announces");
        return out;
    }

    // Explicit single-space separators on top of the padding, so a value
    // wider than its column (a long interface name in `via`) overflows into
    // extra width instead of fusing with the next column.
    pln(
        &mut out,
        &format!(
            "{:<33} {:<33} {:<19} {:<5} {:<23} {}",
            "identity", "announced destination", "name", "hops", "via", "last seen"
        ),
    );
    for rec in rows {
        let identity_hex = js(rec, "identity_hash").unwrap_or("-");
        let dest_hex = js(rec, "destination_hash").unwrap_or("-");
        let name = js(rec, "name").unwrap_or("?");
        let hops = ji(rec, "hops")
            .map(|h| h.to_string())
            .unwrap_or_else(|| "-".to_string());
        // Relayed: interface/next-hop-prefix; direct: the interface alone;
        // no live path: "-".
        let via = match js(rec, "interface") {
            Some(iface) => match js(rec, "via") {
                Some(hop) => format!("{iface}/{}", &hop[..hop.len().min(8)]),
                None => iface.to_string(),
            },
            None => "-".to_string(),
        };
        let last_seen = match jf(rec, "last_seen") {
            Some(t) => {
                let diff = (now_epoch - t).max(0.0);
                if diff < 60.0 {
                    "Just now".to_string()
                } else {
                    format!("{} ago", prettytime_compact(diff))
                }
            }
            None => "-".to_string(),
        };
        pln(
            &mut out,
            &format!(
                "{identity_hex:<33} {dest_hex:<33} {name:<19} {hops:<5} {via:<23} {last_seen}"
            ),
        );
        if let Some(identity) = hex_to_bytes(identity_hex) {
            let derived: Vec<String> = DERIVED_NAMES
                .iter()
                .map(|n| format!("{n}={}", hex_lower16(&derive_destination(n, &identity))))
                .collect();
            pln(&mut out, &format!("  derived: {}", derived.join(" ")));
        }
    }
    out
}

/// Parse a 32-hex-char hash into 16 bytes; `None` on any other input.
fn hex_to_bytes(hex: &str) -> Option<[u8; 16]> {
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = core::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

fn hex_lower16(bytes: &[u8; 16]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// -d / -D  discovered interfaces (rnstatus.py:182-306, Codeberg #32)
// ---------------------------------------------------------------------------

/// `RNS.prettytime(t, compact=True)`: at most the two most-significant nonzero
/// components, each as `<n>d/h/m/s` (integer seconds), joined like the
/// non-compact form.
fn prettytime_compact(mut t: f64) -> String {
    let neg = t < 0.0;
    if neg {
        t = t.abs();
    }
    let days = (t / 86400.0).floor() as i64;
    t %= 86400.0;
    let hours = (t / 3600.0).floor() as i64;
    t %= 3600.0;
    let minutes = (t / 60.0).floor() as i64;
    t %= 60.0;
    let seconds = t.floor() as i64;

    let mut components: Vec<String> = Vec::new();
    // compact: keep a component only while fewer than two are already shown.
    let push = |cond: bool, s: String, components: &mut Vec<String>| {
        if cond && components.len() < 2 {
            components.push(s);
        }
    };
    push(days > 0, format!("{days}d"), &mut components);
    push(hours > 0, format!("{hours}h"), &mut components);
    push(minutes > 0, format!("{minutes}m"), &mut components);
    push(seconds > 0, format!("{seconds}s"), &mut components);

    if components.is_empty() {
        return "0s".to_string();
    }
    let n = components.len();
    let mut tstr = String::new();
    for (i, c) in components.iter().enumerate() {
        if i == 0 {
        } else if i < n - 1 {
            tstr.push_str(", ");
        } else {
            tstr.push_str(" and ");
        }
        tstr.push_str(c);
    }
    if neg {
        format!("-{tstr}")
    } else {
        tstr
    }
}

/// Round to at most 4 decimals and strip trailing zeros, matching
/// `str(round(x, 4))` for the lat/lon display values (keeps one decimal).
fn py_round4_str(x: f64) -> String {
    let s = format!("{x:.4}"); // "52.5000", "13.4560", "52.0000"
    let trimmed = s.trim_end_matches('0'); // "52.", "13.456", "52."
    if let Some(base) = trimmed.strip_suffix('.') {
        format!("{base}.0") // "52.0"
    } else {
        trimmed.to_string() // "13.456"
    }
}

/// Minimal Python-`str(float)` for the height display (integer-valued floats
/// render with a single trailing zero, e.g. `100.0`).
fn py_float_str(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        let s = format!("{x:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Group an integer with commas: `867200000` -> `867,200,000` (Python `{:,}`).
fn fmt_thousands(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    let len = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// The location string for a discovered record's lat/lon (detail vs list vary
/// only in the "none" placeholder, passed as `absent`).
fn location_str(rec: &Value, absent: &str, with_height: bool) -> String {
    if not_null(rec, "latitude") && not_null(rec, "longitude") {
        let lat = py_round4_str(jf(rec, "latitude").unwrap_or(0.0));
        let lon = py_round4_str(jf(rec, "longitude").unwrap_or(0.0));
        if with_height && not_null(rec, "height") {
            let h = py_float_str(jf(rec, "height").unwrap_or(0.0));
            format!("{lat}, {lon}, {h}m h")
        } else {
            format!("{lat}, {lon}")
        }
    } else {
        absent.to_string()
    }
}

/// Render `lnstatus -d` (list) / `-D` (details) for the `discovered_interfaces`
/// RPC response (a list of per-record dicts). `now_epoch` is the client
/// wall-clock in Unix seconds; `details` selects the `-D` layout. Output
/// matches rnstatus.py:182-306.
pub fn render_discovered(
    list: &Value,
    details: bool,
    name_filter: Option<&str>,
    now_epoch: f64,
) -> String {
    let mut out = String::new();
    // rnstatus prints a leading blank line before the discovered block.
    out.push('\n');

    let empty = Vec::new();
    let records = list.as_array().unwrap_or(&empty);

    // Filter by name substring (case-insensitive), matching rnstatus.
    let filtered: Vec<&Value> = records
        .iter()
        .filter(|r| {
            let name = js(r, "name").unwrap_or("");
            match name_filter {
                Some(f) if !f.is_empty() => name.to_lowercase().contains(&f.to_lowercase()),
                _ => true,
            }
        })
        .collect();

    if details {
        for (idx, rec) in filtered.iter().enumerate() {
            let name = js(rec, "name").unwrap_or("");
            let if_type = js(rec, "type").unwrap_or("");
            let status_display = match js(rec, "status").unwrap_or("") {
                "available" => "Available",
                "unknown" => "Unknown",
                "stale" => "Stale",
                other => other,
            };
            let dago = now_epoch - jf(rec, "discovered").unwrap_or(now_epoch);
            let hago = now_epoch - jf(rec, "last_heard").unwrap_or(now_epoch);
            let transport_str = if jb(rec, "transport").unwrap_or(false) {
                "Enabled"
            } else {
                "Disabled"
            };
            let location = location_str(rec, "Unknown", true);

            let transport_id = js(rec, "transport_id");
            let network = match (js(rec, "transport_id"), js(rec, "network_id")) {
                (Some(t), Some(n)) if t != n => Some(n),
                _ => None,
            };

            if idx > 0 {
                out.push_str(&format!("\n{}\n\n", "=".repeat(32)));
            }
            if let Some(n) = network {
                pln(&mut out, &format!("Network   ID : {n}"));
            }
            if let Some(t) = transport_id {
                pln(&mut out, &format!("Transport ID : {t}"));
            }
            pln(&mut out, &format!("Name         : {name}"));
            pln(&mut out, &format!("Type         : {if_type}"));
            pln(&mut out, &format!("Status       : {status_display}"));
            pln(&mut out, &format!("Transport    : {transport_str}"));
            let hops = ji(rec, "hops").unwrap_or(0);
            let hs = if hops == 1 { "" } else { "s" };
            pln(&mut out, &format!("Distance     : {hops} hop{hs}"));
            pln(
                &mut out,
                &format!("Discovered   : {} ago", prettytime_compact(dago)),
            );
            pln(
                &mut out,
                &format!("Last Heard   : {} ago", prettytime_compact(hago)),
            );
            pln(&mut out, &format!("Location     : {location}"));

            if let Some(f) = ji(rec, "frequency") {
                pln(&mut out, &format!("Frequency    : {} Hz", fmt_thousands(f)));
            }
            if let Some(b) = ji(rec, "bandwidth") {
                pln(&mut out, &format!("Bandwidth    : {} Hz", fmt_thousands(b)));
            }
            if let Some(sf) = ji(rec, "sf") {
                pln(&mut out, &format!("Sprd. Factor : {sf}"));
            }
            if let Some(cr) = ji(rec, "cr") {
                pln(&mut out, &format!("Coding Rate  : {cr}"));
            }
            if let Some(m) = js(rec, "modulation") {
                pln(&mut out, &format!("Modulation   : {m}"));
            }
            if let Some(a) = js(rec, "reachable_on") {
                pln(&mut out, &format!("Address      : {a}"));
            }
            if let Some(p) = ji(rec, "port") {
                pln(&mut out, &format!("Port         : {p}"));
            }
            let value = ji(rec, "value").unwrap_or(0);
            pln(&mut out, &format!("Stamp Value  : {value}"));

            pln(&mut out, "\nConfiguration Entry:");
            if let Some(ce) = js(rec, "config_entry") {
                for line in ce.split('\n') {
                    pln(&mut out, &format!("  {line}"));
                }
            }
        }
    } else {
        pln(
            &mut out,
            &format!(
                "{:<25} {:<12} {:<12} {:<12} {:<8} {:<15}",
                "Name", "Type", "Status", "Last Heard", "Value", "Location"
            ),
        );
        pln(&mut out, &"-".repeat(89));
        for rec in &filtered {
            let raw_name = js(rec, "name").unwrap_or("");
            let name = if raw_name.chars().count() > 24 {
                let head: String = raw_name.chars().take(24).collect();
                format!("{head}…")
            } else {
                raw_name.to_string()
            };
            let if_type = js(rec, "type").unwrap_or("").replace("Interface", "");
            let status_display = match js(rec, "status").unwrap_or("") {
                "available" => "✓ Available",
                "unknown" => "? Unknown",
                "stale" => "× Stale",
                other => other,
            };
            let diff = now_epoch - jf(rec, "last_heard").unwrap_or(now_epoch);
            let last_heard_display = if diff < 60.0 {
                "Just now".to_string()
            } else if diff < 3600.0 {
                format!("{}m ago", (diff / 60.0) as i64)
            } else if diff < 86400.0 {
                format!("{}h ago", (diff / 3600.0) as i64)
            } else {
                format!("{}d ago", (diff / 86400.0) as i64)
            };
            let value = ji(rec, "value").unwrap_or(0).to_string();
            let location = location_str(rec, "N/A", false);

            pln(
                &mut out,
                &format!(
                    "{name:<25} {if_type:<12} {status_display:<12} {last_heard_display:<12} {value:<8} {location:<15}"
                ),
            );
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Local-mode render (rnstatus.py:361-671)
// ---------------------------------------------------------------------------

/// Render the full local-mode status output for `stats` (the decoded
/// `interface_stats` dict). `link_count` is the separate `link_count` query
/// result (only meaningful with `-l`). Output includes the leading blank line
/// per interface and the trailing blank line, exactly as rnstatus prints them.
pub fn render_status(stats: &Value, link_count: Option<i64>, opts: &StatusOptions) -> String {
    let mut out = String::new();

    // Work on a mutable copy of the interfaces array so sorting can reorder it.
    let mut interfaces: Vec<Value> = stats
        .get("interfaces")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if let Some(sort) = &opts.sort {
        sort_interfaces(&mut interfaces, sort, opts.reverse);
    }

    for ifstat in &interfaces {
        render_interface(&mut out, ifstat, opts);
    }

    render_trailer(&mut out, stats, link_count, opts);
    out
}

/// Push one line (with trailing newline), mirroring a Python `print(...)`.
fn pln(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

/// The default-hidden name prefixes (rnstatus.py:393-401) plus the I2P
/// non-connectable special case.
fn is_hidden(name: &str, ifstat: &Value) -> bool {
    let i2p_non_connectable =
        name.starts_with("I2PInterface[") && matches!(jb(ifstat, "i2p_connectable"), Some(false));
    name.starts_with("LocalInterface[")
        || name.starts_with("TCPInterface[Client")
        || name.starts_with("BackboneInterface[Client on")
        || name.starts_with("AutoInterfacePeer[")
        || name.starts_with("WeaveInterfacePeer[")
        || name.starts_with("I2PInterfacePeer[Connected peer")
        || i2p_non_connectable
}

fn render_interface(out: &mut String, ifstat: &Value, opts: &StatusOptions) {
    let name = js(ifstat, "name").unwrap_or("");

    // Hidden-by-default interfaces (rnstatus.py:393-401) are shown only with -a;
    // `dispall or not hidden` ⇒ skip when not dispall and hidden.
    if !opts.dispall && is_hidden(name, ifstat) {
        return;
    }
    // The redundant inner I2P guard (rnstatus.py:403).
    if name.starts_with("I2PInterface[") && matches!(jb(ifstat, "i2p_connectable"), Some(false)) {
        return;
    }

    // Display filter (rnstatus.py:404-413). The `burst_filter == None` branch in
    // Python is dead code (it is always a bool), so this is the effective logic.
    let name_matches = || {
        opts.name_filter
            .as_ref()
            .map(|f| name.to_lowercase().contains(&f.to_lowercase()))
            .unwrap_or(false)
    };
    let show_if = if opts.burst_filter {
        let burst_act = has(ifstat, "burst_active")
            && has(ifstat, "pr_burst_active")
            && (jb(ifstat, "burst_active").unwrap_or(false)
                || jb(ifstat, "pr_burst_active").unwrap_or(false));
        burst_act || name_matches()
    } else {
        opts.name_filter.is_none() || name_matches()
    };
    if !show_if {
        return;
    }

    pln(out, "");

    let ss = if jb(ifstat, "status").unwrap_or(false) {
        "Up"
    } else {
        "Down"
    };

    let modestr = match ji(ifstat, "mode") {
        Some(MODE_ACCESS_POINT) => "Access Point",
        Some(MODE_POINT_TO_POINT) => "Point-to-Point",
        Some(MODE_ROAMING) => "Roaming",
        Some(MODE_BOUNDARY) => "Boundary",
        Some(MODE_GATEWAY) => "Gateway",
        _ => "Full",
    };

    // Clients / peers header field (rnstatus.py:429-454).
    let mut clients: Option<i64> = None;
    let mut clients_string = String::new();
    if not_null(ifstat, "clients") {
        let c = ji(ifstat, "clients").unwrap_or(0);
        clients = Some(c);
        if name.starts_with("Shared Instance[") {
            let cnum = (c - 1).max(0);
            let spec = if cnum == 1 { " program" } else { " programs" };
            clients_string = format!("Serving   : {cnum}{spec}");
        } else if name.starts_with("I2PInterface[") {
            if matches!(jb(ifstat, "i2p_connectable"), Some(true)) {
                let spec = if c == 1 {
                    " connected I2P endpoint"
                } else {
                    " connected I2P endpoints"
                };
                clients_string = format!("Peers     : {c}{spec}");
            }
            // else: clients_string stays empty
        } else {
            clients_string = format!("Clients   : {c}");
        }
    }

    pln(out, &format!(" {name}"));

    if let Some(src) = js(ifstat, "autoconnect_source") {
        if not_null(ifstat, "autoconnect_source") {
            pln(out, &format!("    Source    : Auto-connect via <{src}>"));
        }
    }
    if not_null(ifstat, "ifac_netname") {
        if let Some(nn) = js(ifstat, "ifac_netname") {
            pln(out, &format!("    Network   : {nn}"));
        }
    }

    pln(out, &format!("    Status    : {ss}"));

    if clients.is_some() && !clients_string.is_empty() {
        pln(out, &format!("    {clients_string}"));
    }

    if !(name.starts_with("Shared Instance[")
        || name.starts_with("TCPInterface[Client")
        || name.starts_with("LocalInterface["))
    {
        pln(out, &format!("    Mode      : {modestr}"));
    }

    if not_null(ifstat, "bitrate") {
        if let Some(br) = jf(ifstat, "bitrate") {
            pln(out, &format!("    Rate      : {}", speed_str(br)));
        }
    }

    // noise_floor / cpu / mem / battery / airtime / channel_load / switch_id /
    // endpoint / via / tunnelstate / i2p_b32: lnsd never emits these, but honour
    // them if a stats source ever does (rnstatus.py:475-540).
    render_optional_hw_fields(out, ifstat);

    // ifac access line (rnstatus.py:535-537).
    if not_null(ifstat, "ifac_signature") {
        if let Some(sig_hex) = js(ifstat, "ifac_signature") {
            // hexrep(signature[-5:], delimit=False) == last 10 hex chars.
            let tail = if sig_hex.len() >= 10 {
                &sig_hex[sig_hex.len() - 10..]
            } else {
                sig_hex
            };
            let nb = ji(ifstat, "ifac_size").unwrap_or(0) * 8;
            pln(out, &format!("    Access    : {nb}-bit IFAC by <…{tail}>"));
        }
    }
    if not_null(ifstat, "i2p_b32") {
        if let Some(ep) = js(ifstat, "i2p_b32") {
            pln(out, &format!("    I2P B32   : {ep}"));
        }
    }

    // Queued / Held announces (rnstatus.py:542-554).
    if opts.astats {
        if let Some(aqn) = ji(ifstat, "announce_queue") {
            if not_null(ifstat, "announce_queue") && aqn > 0 {
                let word = if aqn == 1 { "announce" } else { "announces" };
                pln(out, &format!("    Queued    : {aqn} {word}"));
            }
        }
        if let Some(aqn) = ji(ifstat, "held_announces") {
            if not_null(ifstat, "held_announces") && aqn > 0 {
                let word = if aqn == 1 { "announce" } else { "announces" };
                pln(out, &format!("    Held      : {aqn} {word}"));
            }
        }
    }

    // TX drops (Codeberg #318): frames shed from the host-side send queue,
    // served by lnsd as `tx_queue_drops`. This line is OURS — rnstatus
    // never reads the key — and it stays silent at zero like the
    // Queued/Held counters above, so a healthy interface renders
    // byte-identical to rnstatus against either daemon.
    if let Some(drops) = ji(ifstat, "tx_queue_drops") {
        if not_null(ifstat, "tx_queue_drops") && drops > 0 {
            let word = if drops == 1 { "frame" } else { "frames" };
            pln(out, &format!("    TX drops  : {drops} {word}"));
        }
    }

    render_traffic_block(out, ifstat, opts, name, clients);
}

/// The optional hardware/telemetry lines (rnstatus.py:475-533). Each is gated
/// on field presence exactly as Python's `"key" in ifstat`.
fn render_optional_hw_fields(out: &mut String, ifstat: &Value) {
    if has(ifstat, "noise_floor") {
        let nstr = if !has(ifstat, "interference") {
            String::new()
        } else {
            let nf = jf(ifstat, "interference").unwrap_or(0.0);
            if nf != 0.0 {
                format!("\n    Intrfrnc. : {} dBm", num_str(ifstat, "interference"))
            } else if has(ifstat, "interference_last_ts") && has(ifstat, "interference_last_dbm") {
                let lago = 0.0; // time-relative; lnsd never emits these fields
                let ldbm = num_str(ifstat, "interference_last_dbm");
                format!("\n    Intrfrnc. : {ldbm} dBm {} ago", prettytime(lago))
            } else {
                ", no interference".to_string()
            }
        };
        if not_null(ifstat, "noise_floor") {
            pln(
                out,
                &format!(
                    "    Noise Fl. : {} dBm{nstr}",
                    num_str(ifstat, "noise_floor")
                ),
            );
        } else {
            pln(out, "    Noise Fl. : Unknown");
        }
    }
    // Last packet RSSI/SNR, stored by `apply_radio_stat`
    // (interfaces/rnode.rs:758-766). These lines are OURS: rnstatus has no
    // per-interface RSSI line (rnstatus.py:475-533 never reads the keys), and
    // only lnsd emits them, so against rnsd this still renders byte-identical
    // to rnstatus. On lnsd they are additive labelled lines inside the
    // presence-gated hardware block — the tolerance any parser of this block
    // already needs for Noise Fl./Battery. The machine surface stays `-j`,
    // where the keys pass through the stats dict untouched.
    if not_null(ifstat, "last_rssi") {
        pln(
            out,
            &format!("    Last RSSI : {} dBm", num_str(ifstat, "last_rssi")),
        );
    }
    if not_null(ifstat, "last_snr") {
        pln(
            out,
            &format!("    Last SNR  : {} dB", num_str(ifstat, "last_snr")),
        );
    }
    if has(ifstat, "cpu_load") {
        if not_null(ifstat, "cpu_load") {
            pln(
                out,
                &format!("    CPU load  : {} %", num_str(ifstat, "cpu_load")),
            );
        } else {
            pln(out, "    CPU load  : Unknown");
        }
    }
    if has(ifstat, "cpu_temp") {
        if not_null(ifstat, "cpu_temp") {
            pln(
                out,
                &format!("    CPU temp  : {}°C", num_str(ifstat, "cpu_temp")),
            );
        } else {
            pln(out, "    CPU load  : Unknown");
        }
    }
    if has(ifstat, "mem_load") {
        if not_null(ifstat, "cpu_load") {
            pln(
                out,
                &format!("    Mem usage : {} %", num_str(ifstat, "mem_load")),
            );
        } else {
            pln(out, "    Mem usage : Unknown");
        }
    }
    if not_null(ifstat, "battery_percent") {
        if let Some(bp) = jf(ifstat, "battery_percent") {
            let bss = js(ifstat, "battery_state").unwrap_or("");
            pln(out, &format!("    Battery   : {}% ({bss})", bp as i64));
        }
    }
    if has(ifstat, "airtime_short") && has(ifstat, "airtime_long") {
        pln(
            out,
            &format!(
                "    Airtime   : {}% (15s), {}% (1h)",
                num_str(ifstat, "airtime_short"),
                num_str(ifstat, "airtime_long")
            ),
        );
    }
    if has(ifstat, "channel_load_short") && has(ifstat, "channel_load_long") {
        pln(
            out,
            &format!(
                "    Ch. Load  : {}% (15s), {}% (1h)",
                num_str(ifstat, "channel_load_short"),
                num_str(ifstat, "channel_load_long")
            ),
        );
    }
    if has(ifstat, "switch_id") {
        if not_null(ifstat, "switch_id") {
            pln(
                out,
                &format!("    Switch ID : {}", scalar_str(ifstat, "switch_id")),
            );
        } else {
            pln(out, "    Switch ID : Unknown");
        }
    }
    if has(ifstat, "endpoint_id") {
        if not_null(ifstat, "endpoint_id") {
            pln(
                out,
                &format!("    Endpoint  : {}", scalar_str(ifstat, "endpoint_id")),
            );
        } else {
            pln(out, "    Endpoint  : Unknown");
        }
    }
    if has(ifstat, "via_switch_id") {
        if not_null(ifstat, "via_switch_id") {
            pln(
                out,
                &format!("    Via       : {}", scalar_str(ifstat, "via_switch_id")),
            );
        } else {
            pln(out, "    Via       : Unknown");
        }
    }
    if not_null(ifstat, "peers") {
        if let Some(p) = ji(ifstat, "peers") {
            pln(out, &format!("    Peers     : {p} reachable"));
        }
    }
    if not_null(ifstat, "tunnelstate") {
        if let Some(ts) = js(ifstat, "tunnelstate") {
            pln(out, &format!("    I2P       : {ts}"));
        }
    }
}

/// The announce / path-request / traffic block (rnstatus.py:556-636), including
/// the character-width padding that aligns the arrow columns.
fn render_traffic_block(
    out: &mut String,
    ifstat: &Value,
    opts: &StatusOptions,
    name: &str,
    mut clients: Option<i64>,
) {
    // announce_rate suffix (rnstatus.py:556-563).
    let art = if opts.astats {
        ji(ifstat, "announce_rate_target")
    } else {
        None
    };
    let arp = if opts.astats {
        ji(ifstat, "announce_rate_penalty")
    } else {
        None
    };
    let arg = if opts.astats {
        ji(ifstat, "announce_rate_grace")
    } else {
        None
    };
    // Python truthiness: art/arg truthy means present and non-zero.
    let art_str = match (art, arp, arg) {
        (Some(t), Some(p), Some(g)) if t != 0 && g != 0 => {
            format!(
                "(t:{}/p:{}/g:{})",
                prettytime(t as f64),
                prettytime(p as f64),
                g
            )
        }
        (Some(t), Some(p), _) if t != 0 => {
            format!("(t:{}/p:{})", prettytime(t as f64), prettytime(p as f64))
        }
        (Some(t), _, _) if t != 0 => format!("(t:{})", prettytime(t as f64)),
        _ => String::new(),
    };

    // Burst suffixes (rnstatus.py:565-573). `burst_activated` is an absolute
    // epoch timestamp (time.time() on rnsd, epoch-converted on lnsd), and the
    // rendered value is the elapsed burst duration `now - activated`.
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let burst_str = if jb(ifstat, "burst_active").unwrap_or(false) {
        let activated = jf(ifstat, "burst_activated").unwrap_or(0.0);
        format!(" burst for {}", prettytime(now_epoch - activated))
    } else {
        String::new()
    };
    let pburst_str = if jb(ifstat, "pr_burst_active").unwrap_or(false) {
        let activated = jf(ifstat, "pr_burst_activated").unwrap_or(0.0);
        format!("burst for {}", prettytime(now_epoch - activated))
    } else {
        String::new()
    };

    let mut rxb_str = format!("↓{}", prettysize(jf(ifstat, "rxb").unwrap_or(0.0), false));
    let mut txb_str = format!("↑{}", prettysize(jf(ifstat, "txb").unwrap_or(0.0), false));

    // Announce frequencies (rnstatus.py:578-590).
    let mut iaf = String::new();
    let mut oaf = String::new();
    let mut pc_str = String::new();
    let mut asr = false;
    if opts.astats && not_null(ifstat, "incoming_announce_frequency") {
        let mut oan = jf(ifstat, "outgoing_announce_frequency").unwrap_or(0.0);
        let ian = jf(ifstat, "incoming_announce_frequency").unwrap_or(0.0);
        if name.starts_with("Shared Instance[") && clients.is_some_and(|c| c > 0) {
            let c = clients.unwrap() as f64;
            oan -= oan / c;
        }
        oaf = prettyfrequency_d1_lpf(oan);
        iaf = prettyfrequency_d1_lpf(ian);

        let mut cspec = "c";
        if clients.is_none() {
            if let Some(p) = ji(ifstat, "peers") {
                if p != 0 {
                    clients = Some(p);
                    cspec = "p";
                }
            }
        }
        if let Some(c) = clients {
            if c > 0 {
                let oaf_field = jf(ifstat, "outgoing_announce_frequency").unwrap_or(0.0);
                pc_str = format!("{}/{cspec}", prettyfrequency_d1_lpf(oaf_field / c as f64));
            }
        }
        asr = true;
    }

    // Path-request frequencies (rnstatus.py:592-607).
    let mut ipf = String::new();
    let mut opf = String::new();
    let mut rpc_str = String::new();
    let mut psr = false;
    if opts.pstats && not_null(ifstat, "incoming_pr_frequency") {
        let mut opn = jf(ifstat, "outgoing_pr_frequency").unwrap_or(0.0);
        let ipn = jf(ifstat, "incoming_pr_frequency").unwrap_or(0.0);
        if name.starts_with("Shared Instance[") && clients.is_some_and(|c| c > 0) {
            let c = clients.unwrap() as f64;
            opn -= opn / c;
        }
        if opts.astats {
            opf = format!("↑{}", prettyfrequency_d1_lpf(opn));
            ipf = format!("↓{}", prettyfrequency_d1_lpf(ipn));
        } else {
            opf = format!("{}↑", prettyfrequency_d1_lpf(opn));
            ipf = format!("{}↓", prettyfrequency_d1_lpf(ipn));
        }
        let mut cspec = "c";
        if clients.is_none() {
            if let Some(p) = ji(ifstat, "peers") {
                if p != 0 {
                    clients = Some(p);
                    cspec = "p";
                }
            }
        }
        if let Some(c) = clients {
            if c > 0 {
                let opr_field = jf(ifstat, "outgoing_pr_frequency").unwrap_or(0.0);
                rpc_str = format!("{}/{cspec}", prettyfrequency_d1_lpf(opr_field / c as f64));
            }
        }
        psr = true;
    }

    // Column padding (rnstatus.py:609-620) — all widths in code points.
    if !asr {
        iaf.clear();
        oaf.clear();
    }
    if !psr {
        ipf.clear();
        opf.clear();
    }
    let amlen = clen(&iaf).max(clen(&oaf));
    iaf.push_str(&" ".repeat(amlen - clen(&iaf)));
    iaf.push('↓');
    oaf.push_str(&" ".repeat(amlen - clen(&oaf)));
    oaf.push('↑');
    let mlen = clen(&iaf)
        .max(clen(&oaf))
        .max(clen(&rxb_str))
        .max(clen(&txb_str))
        .max(clen(&ipf))
        .max(clen(&opf))
        .max(10);
    iaf.push_str(&" ".repeat(mlen - clen(&iaf)));
    oaf.push_str(&" ".repeat(mlen - clen(&oaf)));
    ipf.push_str(&" ".repeat(mlen - clen(&ipf)));
    opf.push_str(&" ".repeat(mlen - clen(&opf)));
    rxb_str.push_str(&" ".repeat(mlen - clen(&rxb_str)));
    txb_str.push_str(&" ".repeat(mlen - clen(&txb_str)));

    if psr {
        pln(out, &format!("    Path Rqs. : {opf}  {rpc_str}"));
        pln(out, &format!("                {ipf}  {pburst_str}"));
    }
    if asr {
        pln(out, &format!("    Announces : {oaf}  {pc_str}"));
        pln(out, &format!("                {iaf} {art_str}{burst_str}"));
    }

    let mut rxstat = rxb_str.clone();
    let mut txstat = txb_str.clone();
    if has(ifstat, "rxs") && has(ifstat, "txs") {
        let _ = write!(
            rxstat,
            "  {}",
            prettyspeed(jf(ifstat, "rxs").unwrap_or(0.0))
        );
        let _ = write!(
            txstat,
            "  {}",
            prettyspeed(jf(ifstat, "txs").unwrap_or(0.0))
        );
    }
    pln(
        out,
        &format!("    Traffic   : {txstat}\n                {rxstat}"),
    );
}

/// Totals / transport-instance / link-table trailer (rnstatus.py:638-671).
fn render_trailer(out: &mut String, stats: &Value, link_count: Option<i64>, opts: &StatusOptions) {
    let has_transport = not_null(stats, "transport_id");

    let mut lstr = String::new();
    if let Some(lc) = link_count {
        if opts.lstats {
            let ms = if lc == 1 { "y" } else { "ies" };
            if has_transport {
                lstr = format!(", {lc} entr{ms} in link table");
            } else {
                lstr = format!(" {lc} entr{ms} in link table");
            }
        }
    }

    if opts.totals {
        let mut rxb_str = format!("↓{}", prettysize(jf(stats, "rxb").unwrap_or(0.0), false));
        let mut txb_str = format!("↑{}", prettysize(jf(stats, "txb").unwrap_or(0.0), false));
        let strdiff = clen(&rxb_str) as i64 - clen(&txb_str) as i64;
        if strdiff > 0 {
            txb_str.push_str(&" ".repeat(strdiff as usize));
        } else if strdiff < 0 {
            rxb_str.push_str(&" ".repeat((-strdiff) as usize));
        }
        let rxstat = format!(
            "{rxb_str}  {}",
            prettyspeed(jf(stats, "rxs").unwrap_or(0.0))
        );
        let txstat = format!(
            "{txb_str}  {}",
            prettyspeed(jf(stats, "txs").unwrap_or(0.0))
        );
        pln(
            out,
            &format!("\n Totals       : {txstat}\n                {rxstat}"),
        );
    }

    if has_transport {
        if let Some(tid) = js(stats, "transport_id") {
            pln(
                out,
                &format!(
                    "\n Transport Instance {} running",
                    prettyhexrep_from_hex(tid)
                ),
            );
        }
        if not_null(stats, "network_id") {
            if let Some(nid) = js(stats, "network_id") {
                pln(
                    out,
                    &format!(" Network Identity   {}", prettyhexrep_from_hex(nid)),
                );
            }
        }
        if not_null(stats, "probe_responder") {
            if let Some(pr) = js(stats, "probe_responder") {
                pln(
                    out,
                    &format!(" Probe responder at {} active", prettyhexrep_from_hex(pr)),
                );
            }
        }
        if not_null(stats, "transport_uptime") {
            if let Some(up) = jf(stats, "transport_uptime") {
                pln(out, &format!(" Uptime is {}{lstr}", prettytime(up)));
            }
        }
    } else if !lstr.is_empty() {
        pln(out, &format!("\n{lstr}"));
    }

    pln(out, "");
}

/// Render a scalar numeric field with Python `str()` semantics (int stays int,
/// float keeps its repr). Used for the telemetry lines above.
fn num_str(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else {
                n.to_string()
            }
        }
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Render an arbitrary scalar field with Python `str()` semantics (numbers,
/// strings). Used for switch/endpoint/via IDs.
fn scalar_str(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => {
            // Python str(True) == "True"
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests;
