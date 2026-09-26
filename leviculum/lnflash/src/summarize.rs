//! `--summarize`: a view over a `--watch` file.
//!
//! The watch file is the evidence and stays unfiltered
//! ([`crate::watch`]); this module is the classification the field walks
//! used to do by hand — announce vs data vs path request, per hour, plus
//! the last line seen per class (Codeberg #365 is six announce lines and
//! not one data line, which is the whole finding).
//!
//! A reception is a `[LORA] RX <n> bytes` line; split parts, idle notes
//! and `RX too short` are radio chatter, not packets. The class comes
//! from the `flags=` byte when the line carries one — the low two bits
//! are the Reticulum packet type — and a data packet addressed to the
//! well-known `rnstransport.path.request` destination is a path request.
//! Lines annotated in words instead (the #365 monitor wrote `ANNOUNCE
//! 2f9a770a..` with no flags) classify by the word. A bare `RX n bytes`
//! line, which is all today's firmware prints, counts as unclassified
//! rather than being guessed at.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use leviculum_core::crypto::truncated_hash;
use leviculum_core::Destination;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Announce,
    Data,
    PathRequest,
    LinkRequest,
    Proof,
}

const CLASSES: [Class; 5] = [
    Class::Announce,
    Class::Data,
    Class::PathRequest,
    Class::LinkRequest,
    Class::Proof,
];

impl Class {
    fn label(self) -> &'static str {
        match self {
            Class::Announce => "announce",
            Class::Data => "data",
            Class::PathRequest => "path request",
            Class::LinkRequest => "link request",
            Class::Proof => "proof",
        }
    }

    /// The spelling used in the per-hour rows, greppable as one token.
    fn column(self) -> &'static str {
        match self {
            Class::Announce => "announce",
            Class::Data => "data",
            Class::PathRequest => "path-request",
            Class::LinkRequest => "link-request",
            Class::Proof => "proof",
        }
    }
}

/// Read a watch file and render the summary.
pub fn report(text: &str) -> String {
    let path_request_hex = path_request_hex();
    let mut per_hour: BTreeMap<String, [u64; 5]> = BTreeMap::new();
    let mut totals = [0u64; 5];
    let mut last: [Option<&str>; 5] = [None; 5];
    let mut unclassified = 0u64;
    let mut gaps = 0u64;
    let mut receptions = 0u64;

    for line in text.lines() {
        if line.contains("[WATCH]") && line.contains("went away") {
            gaps += 1;
            continue;
        }
        let Some(rest) = reception(line) else {
            continue;
        };
        receptions += 1;
        match classify(rest, &path_request_hex) {
            Some(class) => {
                let index = class as usize;
                totals[index] += 1;
                last[index] = Some(line);
                per_hour.entry(hour_bucket(line).to_string()).or_default()[index] += 1;
            }
            None => unclassified += 1,
        }
    }

    let mut out = String::new();
    let mut counted = vec![
        format!("{} announce", totals[Class::Announce as usize]),
        format!("{} data", totals[Class::Data as usize]),
        format!("{} path request", totals[Class::PathRequest as usize]),
    ];
    for class in [Class::LinkRequest, Class::Proof] {
        if totals[class as usize] > 0 {
            counted.push(format!("{} {}", totals[class as usize], class.label()));
        }
    }
    if unclassified > 0 {
        counted.push(format!("{unclassified} unclassified"));
    }
    let _ = writeln!(
        out,
        "{receptions} [LORA] RX receptions: {}",
        counted.join(", ")
    );
    if gaps > 0 {
        let plural = if gaps == 1 { "" } else { "s" };
        let _ = writeln!(out, "{gaps} reconnect gap{plural}");
    }
    if receptions == 0 {
        return out;
    }

    // The columns shown are the ones the file needs: the three classes
    // the walks watch for always, the exotic two only when present.
    let shown: Vec<Class> = CLASSES
        .into_iter()
        .filter(|class| {
            matches!(class, Class::Announce | Class::Data | Class::PathRequest)
                || totals[*class as usize] > 0
        })
        .collect();
    let _ = writeln!(out, "\nper hour:");
    for (hour, counts) in &per_hour {
        let cells: Vec<String> = shown
            .iter()
            .map(|class| format!("{}={}", class.column(), counts[*class as usize]))
            .collect();
        let _ = writeln!(out, "  {hour}  {}", cells.join(" "));
    }
    let _ = writeln!(out, "\nlast seen per class:");
    for class in &shown {
        let _ = writeln!(
            out,
            "  {:<13} {}",
            class.label(),
            last[*class as usize].unwrap_or("none")
        );
    }
    out
}

/// The well-known path request destination, as lowercase hex: a PLAIN
/// destination named `rnstransport.path.request`, hash =
/// `truncated_hash(name_hash)` — computed from the same primitives the
/// stack uses rather than written down, so it cannot drift.
fn path_request_hex() -> String {
    let name_hash = Destination::compute_name_hash("rnstransport", &["path", "request"]);
    truncated_hash(&name_hash)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// The text after `[LORA] RX ` when the line is a reception — the next
/// two tokens must be a length and `bytes`, which is what separates a
/// packet from `RX split part`, `RX idle` and `RX too short`.
fn reception(line: &str) -> Option<&str> {
    let marker = "[LORA] RX ";
    let rest = &line[line.find(marker)? + marker.len()..];
    let mut tokens = rest.split_whitespace();
    let length = tokens.next()?;
    if length.is_empty() || !length.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    (tokens.next()? == "bytes").then_some(rest)
}

fn classify(rest: &str, path_request_hex: &str) -> Option<Class> {
    if let Some(flags) = flags_value(rest) {
        return Some(match flags & 0x03 {
            0x01 => Class::Announce,
            0x02 => Class::LinkRequest,
            0x03 => Class::Proof,
            _ if names_path_request(rest, path_request_hex) => Class::PathRequest,
            _ => Class::Data,
        });
    }
    if names_path_request(rest, path_request_hex) {
        return Some(Class::PathRequest);
    }
    if rest.contains("ANNOUNCE") {
        return Some(Class::Announce);
    }
    if rest.contains("LINKREQUEST") || rest.contains("LINK_REQUEST") {
        return Some(Class::LinkRequest);
    }
    if rest.contains("PROOF") {
        return Some(Class::Proof);
    }
    if rest.contains("DATA") {
        return Some(Class::Data);
    }
    None
}

/// The value of a `flags=0x..` token: the Reticulum header byte, whose
/// low two bits are the packet type.
fn flags_value(rest: &str) -> Option<u8> {
    let start = rest.find("flags=0x")? + "flags=0x".len();
    let hex: String = rest[start..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    u8::from_str_radix(&hex, 16).ok()
}

/// Whether the line names the path request destination — in words, or as
/// a hex token (possibly truncated with a trailing `..`, possibly behind
/// the firmware's `dst=` key) that prefixes the well-known hash.
fn names_path_request(rest: &str, path_request_hex: &str) -> bool {
    if rest.contains("PATH_REQUEST") || rest.contains("PATHREQ") {
        return true;
    }
    rest.split_whitespace().any(|token| {
        let hex = token
            .strip_prefix("dst=")
            .unwrap_or(token)
            .trim_end_matches("..");
        hex.len() >= 8
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && path_request_hex.starts_with(&hex.to_ascii_lowercase())
    })
}

/// The `YYYY-MM-DDTHH` prefix of a stamped line. A line without a stamp
/// (a watch file edited by hand, or one produced by something else) is
/// still counted, under a bucket that says what it is.
fn hour_bucket(line: &str) -> &str {
    let bytes = line.as_bytes();
    let stamped = bytes.len() >= 13
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && bytes[10] == b'T'
        && bytes[11..13].iter().all(u8::is_ascii_digit);
    if stamped {
        &line[..13]
    } else {
        "(unstamped)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The #365 walk, verbatim in shape: the timestamped `[LORA] RX …
    /// flags=0x01 ANNOUNCE` line, the later word-only announce lines,
    /// plus a data line, a path-request line (the real
    /// `rnstransport.path.request` hash prefix), a bare RX line as
    /// today's firmware prints it, radio chatter, and one gap.
    const FIXTURE: &str = "\
2026-09-04T15:21:59.000+02:00 [WATCH] watching 183004F712B4A7FE on /dev/ttyACM1
2026-09-04T15:22:27.101+02:00 [LORA] RX 183 bytes rssi=-69 flags=0x01 ANNOUNCE of 2f9a770a..
2026-09-04T15:23:28.202+02:00 [LORA] RX 183 bytes rssi=-100 ANNOUNCE 2f9a770a..
2026-09-04T15:24:28.303+02:00 [LORA] RX 183 bytes rssi=-100 ANNOUNCE 2f9a770a..
2026-09-04T15:25:27.404+02:00 [LORA] RX 183 bytes rssi=-107 ANNOUNCE 2f9a770a..
2026-09-04T15:26:38.505+02:00 [LORA] RX 183 bytes rssi=-107 ANNOUNCE 2f9a770a..
2026-09-04T15:27:42.606+02:00 [LORA] RX 183 bytes rssi=-108 ANNOUNCE 2f9a770a..
2026-09-04T15:59:01.000+02:00 [WATCH] 183004F712B4A7FE on /dev/ttyACM1 went away (EOF, the port closed); reconnecting
2026-09-04T15:59:13.400+02:00 [WATCH] reconnected to 183004F712B4A7FE on /dev/ttyACM1 after 12.4s gap
2026-09-04T16:02:03.707+02:00 [LORA] RX 91 bytes rssi=-95 flags=0x00 DATA to 9eb1f403..
2026-09-04T16:04:11.808+02:00 [LORA] RX 51 bytes rssi=-96 flags=0x00 DATA to 6b9f6601..
2026-09-04T16:05:00.909+02:00 [LORA] RX 12 bytes rssi=-90 snr=8
2026-09-04T16:05:30.010+02:00 [LORA] RX split part 23 bytes seq=1 rssi=-90 snr=8
2026-09-04T16:06:00.111+02:00 [LORA] TX 44 bytes
";

    #[test]
    fn the_fixture_summarizes_to_counts_per_hour_and_a_last_line_per_class() {
        assert_eq!(
            report(FIXTURE),
            "\
9 [LORA] RX receptions: 6 announce, 1 data, 1 path request, 1 unclassified
1 reconnect gap

per hour:
  2026-09-04T15  announce=6 data=0 path-request=0
  2026-09-04T16  announce=0 data=1 path-request=1

last seen per class:
  announce      2026-09-04T15:27:42.606+02:00 [LORA] RX 183 bytes rssi=-108 ANNOUNCE 2f9a770a..
  data          2026-09-04T16:02:03.707+02:00 [LORA] RX 91 bytes rssi=-95 flags=0x00 DATA to 9eb1f403..
  path request  2026-09-04T16:04:11.808+02:00 [LORA] RX 51 bytes rssi=-96 flags=0x00 DATA to 6b9f6601..
"
        );
    }

    #[test]
    fn the_path_request_destination_is_the_reference_hash() {
        // RNS.Destination.hash(None, "rnstransport", "path", "request"),
        // computed with the stack's own primitives. If this moves, either
        // the primitives broke or the well-known name did.
        assert_eq!(path_request_hex(), "6b9f66014d9853faab220fba47d02761");
    }

    #[test]
    fn the_flags_byte_decides_over_the_words_in_the_line() {
        // A payload dump could contain the word ANNOUNCE; a flags byte
        // saying data is the packet's own header and wins.
        let hex = path_request_hex();
        assert_eq!(
            classify("40 bytes rssi=-80 flags=0x00 ANNOUNCE-shaped payload", &hex),
            Some(Class::Data)
        );
        // Transported variants keep their low two bits.
        assert_eq!(
            classify("183 bytes rssi=-69 flags=0x51", &hex),
            Some(Class::Announce)
        );
        assert_eq!(
            classify("62 bytes rssi=-70 flags=0x02", &hex),
            Some(Class::LinkRequest)
        );
        assert_eq!(
            classify("47 bytes rssi=-71 flags=0x03", &hex),
            Some(Class::Proof)
        );
    }

    #[test]
    fn the_new_firmware_line_shape_classifies_all_four_classes() {
        // What the firmware prints since the flags=/dst= keys landed:
        // `RX <n> bytes rssi=<r> snr=<s> flags=0x<hh> dst=<hex8>`.
        let hex = path_request_hex();
        assert_eq!(
            classify("183 bytes rssi=-69 snr=5 flags=0x01 dst=2f9a770a", &hex),
            Some(Class::Announce)
        );
        assert_eq!(
            classify("91 bytes rssi=-95 snr=7 flags=0x00 dst=9eb1f403", &hex),
            Some(Class::Data)
        );
        // A data packet whose dst= is the well-known
        // rnstransport.path.request hash is a path request.
        assert_eq!(
            classify("51 bytes rssi=-96 snr=6 flags=0x00 dst=6b9f6601", &hex),
            Some(Class::PathRequest)
        );
        assert_eq!(
            classify("47 bytes rssi=-71 snr=4 flags=0x03 dst=9eb1f403", &hex),
            Some(Class::Proof)
        );
    }

    #[test]
    fn a_bare_rx_line_is_counted_but_not_guessed_at() {
        let hex = path_request_hex();
        assert_eq!(classify("12 bytes rssi=-90 snr=8", &hex), None);
    }

    #[test]
    fn radio_chatter_is_not_a_reception() {
        for line in [
            "ts [LORA] RX split part 23 bytes seq=1 rssi=-90 snr=8",
            "ts [LORA] RX idle (5)",
            "ts [LORA] RX too short (1)",
            "ts [LORA] TX 44 bytes",
            "ts [LORA] RX err: Timeout",
        ] {
            assert_eq!(reception(line), None, "{line}");
        }
        assert_eq!(
            reception("ts [LORA] RX 183 bytes rssi=-69 snr=5"),
            Some("183 bytes rssi=-69 snr=5")
        );
    }

    #[test]
    fn an_unstamped_line_lands_in_its_own_bucket_rather_than_vanishing() {
        let text = "[LORA] RX 10 bytes flags=0x01\n";
        let said = report(text);
        assert!(said.contains("(unstamped)  announce=1"), "{said}");
    }

    #[test]
    fn an_empty_file_says_so_instead_of_printing_empty_tables() {
        assert_eq!(
            report(""),
            "0 [LORA] RX receptions: 0 announce, 0 data, 0 path request\n"
        );
    }
}
