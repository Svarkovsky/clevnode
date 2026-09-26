//! Codeberg #65 instrumentation contract: the shape of the LNode debug lines
//! the host side greps.
//!
//! The firmware emits two line shapes on the CDC-ACM debug console:
//!
//! ```text
//! [INFO!] [PANIC_COUNT] total=<u32> t=<ms>                  (boot banner, once per boot)
//! [HEAP] used=<n> free=<n> watermark=<n> size=<n> t=<ms>    (every 30 s)
//! [SX_REG] rxgain_before=0xNN rxgain_after=0xNN txmod=0xNN  (once, end of init_radio)
//! [SX_REG_IQ] iq_before=0xNN iq_after=0xNN txmod=0xNN       (once, first SetPacketParams)
//! [SX_RX_ARM] site=<tag> timeout_ms=<u32> dark_ms=<u64|first>  (every SetRx)
//! [SX_RX_ADOPT] latched=0xNNNN preamble=<0|1> header=<0|1> rxdone=<0|1> stood_ms=<u32>
//! [SX_RX_TEARDOWN] site=<tag> preamble=<0|1> header=<0|1> rxdone=<0|1> armed_ms=<u32>
//! [SX_TX_DEFER] waited_ms=<u64> reason=<preamble|header> outcome=<frame|timeout|abandoned>
//! ```
//!
//! The adopt/teardown pair is read as a rate against each other: an
//! `[SX_RX_ADOPT]` with any flag set is a reception the pre-adoption firmware
//! destroyed, an `[SX_RX_TEARDOWN]` with any flag set is one still being
//! destroyed, and `site=` on the teardown says by which caller. Both are
//! emitted with the flags as read, all-zero included; a line that appeared
//! only when it had bad news would give a numerator with no denominator.
//!
//! `[SX_TX_DEFER]` is the third of that family and the one that reports a
//! behaviour rather than an observation: a transmit that found a reception
//! arriving on the window it was about to end waited for it instead.
//! `outcome=frame` against `outcome=timeout`, taken per `reason=`, is whether
//! the wait is earning its keep or merely delaying the transmitter — the ratio
//! the guard has to justify itself with, and the reason the line carries the
//! measured `waited_ms` rather than the bound it was allowed.
//!
//! The trailing ` t=<ms>` is board uptime at the moment the line was
//! formatted, appended to EVERY runtime line since the drain-latency audit
//! (#344). It is deliberately at the END: every consumer below anchors on a
//! `[TAG]`, and a leading stamp would break all of them at once. The parsers
//! here therefore have to keep working with an unknown-key field appended —
//! which is exactly what `stamp_does_not_disturb_the_existing_parsers` asserts.
//!
//! Two host-side consumers grep them: `scripts/catch-reboot.sh`, which reports
//! the cause of a reboot caught under sustained LoRa load, and the ad-hoc heap
//! analysis of a debug capture (Codeberg #50 left the peak-load stack question
//! open, so `[HEAP]` is still read by hand). If the firmware changes the shape,
//! both silently grep nothing. These tests fail first.
//!
//! Migrated here from `reticulum-integ/tests/lnode_instrumentation.rs` when
//! that crate was retired. The hardware half of that file — a live listen on
//! every attached LNode's debug port asserting the telemetry really flows —
//! did NOT come with it: periculum captures the debug port but has no step
//! class that asserts on the capture, so there is nowhere to express it. See
//! the retirement ledger.
//!
//! This lives in leviculum-std because the firmware crate cross-compiles to
//! thumbv7em and cannot run host tests, and leviculum-std is the crate that
//! owns host-side communication with a board.

use std::path::{Path, PathBuf};

/// Extract the persistent panic counter from a debug-capture line.
fn parse_panic_count(line: &str) -> Option<u32> {
    let idx = line.find("[PANIC_COUNT] total=")?;
    let rest = &line[idx + "[PANIC_COUNT] total=".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Extract (used, free, watermark, size) from a `[HEAP]` telemetry line.
fn parse_heap_line(line: &str) -> Option<(u64, u64, u64, u64)> {
    let idx = line.find("[HEAP] ")?;
    let rest = &line[idx + "[HEAP] ".len()..];
    let mut used = None;
    let mut free = None;
    let mut watermark = None;
    let mut size = None;
    for token in rest.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        let value: u64 = value.parse().ok()?;
        match key {
            "used" => used = Some(value),
            "free" => free = Some(value),
            "watermark" => watermark = Some(value),
            "size" => size = Some(value),
            _ => {}
        }
    }
    Some((used?, free?, watermark?, size?))
}

#[test]
fn panic_count_banner_line_parses() {
    // Real shape as emitted via log_critical! on boot.
    assert_eq!(parse_panic_count("[INFO!] [PANIC_COUNT] total=0"), Some(0));
    assert_eq!(parse_panic_count("[PANIC_COUNT] total=17"), Some(17));
    // Prefixed by capture-side timestamps or banner noise still parses.
    assert_eq!(
        parse_panic_count("2026-06-12T20:46:01Z [INFO!] [PANIC_COUNT] total=3 trailing"),
        Some(3)
    );
    assert_eq!(parse_panic_count("[INFO!] leviculum T114 booting"), None);
    assert_eq!(parse_panic_count("[PANIC_COUNT] total="), None);
}

#[test]
fn heap_telemetry_line_parses() {
    // Real line captured from the flashed T114 (2026-06-12).
    let line = "[HEAP] used=52376 free=13156 watermark=52376 size=65536";
    assert_eq!(parse_heap_line(line), Some((52376, 13156, 52376, 65536)));
    // Real line captured from the flashed Pocket V2.
    let line = "[HEAP] used=50016 free=15516 watermark=50016 size=65536";
    assert_eq!(parse_heap_line(line), Some((50016, 15516, 50016, 65536)));
    // Persistent-tail replay prefixes the tag chain; still parses.
    let line = "[INFO!] [PERSISTENT_LOG] [HEAP] used=1 free=2 watermark=3 size=4";
    assert_eq!(parse_heap_line(line), Some((1, 2, 3, 4)));
    assert_eq!(parse_heap_line("[HEAP] used=1 free=2"), None);
    assert_eq!(parse_heap_line("[DIAG_MEM] stack_free=9000"), None);
}

/// Extract the hex-valued fields of an `[SX_REG]`-family register read-back.
///
/// Returns the fields in the order the line carries them. Shared by both
/// lines: they differ in tag and field names, not in shape, and a consumer
/// that wanted one of them by name would have to parse `key=0xNN` anyway.
fn parse_reg_line(line: &str, tag: &str, keys: &[&str]) -> Option<Vec<u8>> {
    let idx = line.find(tag)?;
    let rest = &line[idx + tag.len()..];
    let mut found: Vec<Option<u8>> = keys.iter().map(|_| None).collect();
    for token in rest.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        let Some(pos) = keys.iter().position(|k| *k == key) else {
            continue; // the ` t=` stamp, and anything appended after it
        };
        found[pos] = u8::from_str_radix(value.strip_prefix("0x")?, 16).ok();
    }
    found.into_iter().collect()
}

const SX_REG_KEYS: [&str; 3] = ["rxgain_before", "rxgain_after", "txmod"];
const SX_REG_IQ_KEYS: [&str; 3] = ["iq_before", "iq_after", "txmod"];

/// The two register read-back lines parse, stamp and replay-wrapper included.
///
/// These are the only evidence a capture can carry that `35fdd87`'s two
/// register writes took effect: an unwritten register and a written one look
/// identical in every other line the firmware emits. The `before` field is
/// what makes each line a measurement — `after` alone would say a register
/// holds a value, not that we put it there — so a parser that dropped it
/// would silently turn the measurement back into an assertion.
#[test]
fn register_readback_lines_parse() {
    assert_eq!(
        parse_reg_line(
            "[SX_REG] rxgain_before=0x94 rxgain_after=0x96 txmod=0x0D t=412",
            "[SX_REG] ",
            &SX_REG_KEYS
        ),
        Some(vec![0x94, 0x96, 0x0D])
    );
    assert_eq!(
        parse_reg_line(
            "[SX_REG_IQ] iq_before=0x0D iq_after=0x0D txmod=0x0D t=511",
            "[SX_REG_IQ] ",
            &SX_REG_IQ_KEYS
        ),
        Some(vec![0x0D, 0x0D, 0x0D])
    );
    // The line is emitted through `log_fmt_critical`, so a board that crashed
    // replays it wrapped on the next boot. Still parses.
    assert_eq!(
        parse_reg_line(
            "[INFO!] [PERSISTENT_LOG] [SX_REG] rxgain_before=0x96 rxgain_after=0x96 txmod=0x0D t=412 t=7",
            "[SX_REG] ",
            &SX_REG_KEYS
        ),
        Some(vec![0x96, 0x96, 0x0D])
    );
    // A field missing is a parse failure, not a zero. A zero would read as a
    // register that answered 0x00.
    assert_eq!(
        parse_reg_line(
            "[SX_REG] rxgain_before=0x94 txmod=0x0D t=412",
            "[SX_REG] ",
            &SX_REG_KEYS
        ),
        None
    );
    // The two tags do not answer for each other.
    assert_eq!(
        parse_reg_line(
            "[SX_REG] rxgain_before=0x94 rxgain_after=0x96 txmod=0x0D",
            "[SX_REG_IQ] ",
            &SX_REG_IQ_KEYS
        ),
        None
    );
}

/// The firmware still emits what the parsers above expect, and — the part
/// that matters — each `after` value is still a second read of the register
/// rather than the value we sent.
///
/// The shaping lives in `leviculum_core::sx126x`'s two `Display` impls and the
/// read-write-read brackets in `probe_rx_init` / `apply_iq_polarity`, which
/// have their own host tests (`sx126x::probe_tests`). What is pinned here is
/// the seam those tests cannot see: that the firmware actually calls them, and
/// with the tag the host greps.
#[test]
fn the_firmware_still_emits_the_register_readback() {
    let sx = nrf_source("sx1262.rs");
    for (tag, call) in [
        ("\"[SX_REG] \"", "sx126x::probe_rx_init(self)"),
        ("\"[SX_REG_IQ] \"", "sx126x::apply_iq_polarity(self, false,"),
    ] {
        assert!(
            sx.contains(tag),
            "leviculum-nrf/src/sx1262.rs no longer emits the {tag} tag"
        );
        assert!(
            sx.contains(call),
            "leviculum-nrf/src/sx1262.rs no longer calls {call}, so the line it \
             logs is no longer the core probe's bracketed read-back"
        );
    }
    // Boot-critical, both of them: `init_radio` and the first
    // `SetPacketParams` run long before DTR-assert opens the runtime drain, so
    // a `log_fmt` here would be counted in RUNTIME_DROPPED and thrown away.
    for tag in ["[SX_REG] ", "[SX_REG_IQ] "] {
        let idx = sx
            .find(tag)
            .unwrap_or_else(|| panic!("leviculum-nrf/src/sx1262.rs: {tag} tag gone"));
        let call_start = sx[..idx].rfind("crate::log::").unwrap_or_else(|| {
            panic!("leviculum-nrf/src/sx1262.rs: {tag} is not logged through crate::log")
        });
        assert!(
            sx[call_start..idx].contains("log_fmt_critical"),
            "leviculum-nrf/src/sx1262.rs logs {tag} through the runtime-gated \
             log_fmt; it is emitted before DTR-assert and would be dropped"
        );
    }
    // The field names and their order are the parsers' contract, and they live
    // in core's Display impls.
    let core_src = {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("leviculum-core/src/sx126x.rs");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };
    assert!(
        core_src.contains("rxgain_before=0x{:02X} rxgain_after=0x{:02X} txmod=0x{:02X}"),
        "the [SX_REG] field order or spelling changed in leviculum-core/src/sx126x.rs"
    );
    assert!(
        core_src.contains("iq_before=0x{:02X} iq_after=0x{:02X} txmod=0x{:02X}"),
        "the [SX_REG_IQ] field order or spelling changed in leviculum-core/src/sx126x.rs"
    );
}

fn nrf_source(rel: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("leviculum-nrf/src")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The parsers above are only worth anything if they still describe what the
/// firmware writes. Pin the emitting format strings in the firmware source, so
/// a rename there fails here rather than in a grep that quietly returns
/// nothing months later.
#[test]
fn the_firmware_still_emits_the_shapes_these_parsers_expect() {
    let lib = nrf_source("lib.rs");
    assert!(
        lib.contains(r#""[HEAP] ""#),
        "leviculum-nrf/src/lib.rs no longer emits the `[HEAP] ` tag"
    );
    assert!(
        lib.contains("used={used} free={free} watermark={watermark} size={HEAP_SIZE}"),
        "the [HEAP] field order or spelling changed in leviculum-nrf/src/lib.rs"
    );
    // The banner format string lives in the shared helper (48f34f6 moved it
    // out of the BSP bins); each bin must still invoke it at boot.
    assert!(
        lib.contains("[PANIC_COUNT] total="),
        "leviculum-nrf/src/lib.rs no longer emits the [PANIC_COUNT] banner"
    );
    for bin in ["bin/t114.rs", "bin/rak4631.rs"] {
        assert!(
            nrf_source(bin).contains("log_panic_count()"),
            "leviculum-nrf/src/{bin} no longer emits the [PANIC_COUNT] banner at boot"
        );
    }
}

/// The uptime stamp of a captured line: its LAST `t=` field.
///
/// The last, not the first: a `[PERSISTENT_LOG]` replay wraps a line from the
/// previous boot — its stamp included — inside a line of this boot. The
/// firmware-side definition of this rule lives in `leviculum-log-line`; this
/// is the host-side copy, and the two are pinned against each other by
/// `the_firmware_stamps_every_line_with_board_uptime` below.
fn parse_stamp(line: &str) -> Option<u64> {
    line.trim_end_matches(['\r', '\n'])
        .rsplit(' ')
        .find_map(|field| field.strip_prefix("t="))
        .and_then(|v| v.parse().ok())
}

#[test]
fn uptime_stamp_parses_off_the_end_of_a_line() {
    assert_eq!(
        parse_stamp("[HEAP] used=52376 free=13156 watermark=52376 size=65536 t=91422"),
        Some(91422)
    );
    // A replayed line carries two. The line's own stamp is the outer one.
    assert_eq!(
        parse_stamp("[INFO!] [PERSISTENT_LOG] [LORA] RX 41 bytes t=91422 t=137"),
        Some(137)
    );
    assert_eq!(parse_stamp("[LORA] RX 41 bytes"), None);
    assert_eq!(parse_stamp("[LORA] rtt=5"), None);
}

#[test]
fn stamp_does_not_disturb_the_existing_parsers() {
    // The whole risk of appending a field is that a consumer keyed on the
    // rest of the line stops seeing it. Both parsers above, on real stamped
    // lines, must return exactly what they returned before the stamp existed.
    assert_eq!(
        parse_panic_count("[INFO!] [PANIC_COUNT] total=3 t=137"),
        Some(3)
    );
    assert_eq!(
        parse_heap_line("[HEAP] used=52376 free=13156 watermark=52376 size=65536 t=91422"),
        Some((52376, 13156, 52376, 65536))
    );
    assert_eq!(
        parse_heap_line("[INFO!] [PERSISTENT_LOG] [HEAP] used=1 free=2 watermark=3 size=4 t=9 t=2"),
        Some((1, 2, 3, 4))
    );
}

/// The stamp is only a measurement if the clock behind it was running, and
/// only usable if it is on every line. Both are properties of the firmware
/// source, so both are pinned here.
#[test]
fn the_firmware_stamps_every_line_with_board_uptime() {
    let log = nrf_source("log.rs");
    assert!(
        log.contains("embassy_time::Instant::now().as_millis()"),
        "leviculum-nrf/src/log.rs no longer reads board uptime for the t= stamp"
    );
    // Uniform: both formatting paths — the `log_fmt*` one and the tracing
    // subscriber's hand-built line — close through the shared shaper. A
    // stamp on some lines and not others is a trap for whoever reads the
    // log next.
    assert!(
        log.contains("leviculum_log_line::format_line("),
        "log_fmt/log_fmt_critical no longer shape their line through leviculum-log-line"
    );
    assert!(
        log.contains("leviculum_log_line::finish("),
        "the tracing subscriber no longer appends the t= stamp"
    );
    // The stamp is meaningful in every logging context only because RTC1 is
    // already running when the first line is written. Both entry points start
    // it — `embassy_nrf::init` — before they log anything. If that order ever
    // flips, the boot lines silently become t=0.
    for bin in ["bin/t114.rs", "bin/rak4631.rs"] {
        let src = nrf_source(bin);
        let init = src
            .find("embassy_nrf::init(")
            .unwrap_or_else(|| panic!("leviculum-nrf/src/{bin} no longer calls embassy_nrf::init"));
        let first_log = ["log_critical!", "info!(", "warn!(", "log_fmt"]
            .iter()
            .filter_map(|pat| src.find(pat))
            .min()
            .unwrap_or_else(|| panic!("leviculum-nrf/src/{bin} logs nothing at all"));
        assert!(
            init < first_log,
            "leviculum-nrf/src/{bin} logs before embassy_nrf::init starts the clock, \
             so the first lines would carry t=0 rather than a real uptime"
        );
    }
}

/// The panic handler and the HardFault exception must keep NOT logging.
///
/// They are the two contexts where a clock read would have needed a sentinel.
/// Today neither writes a log line: both capture their evidence into
/// `.uninit` RAM and `sys_reset`, and the NEXT boot logs it with a running
/// clock. That is what makes "every line carries a real stamp" true without
/// exception, so it is asserted rather than merely described.
/// The brace-balanced body that follows `marker` in `src`.
fn body_after<'a>(src: &'a str, marker: &str, what: &str) -> &'a str {
    let after = src
        .split_once(marker)
        .unwrap_or_else(|| panic!("leviculum-nrf/src/lib.rs: {what} not found ({marker:?})"))
        .1;
    let open = after
        .find('{')
        .unwrap_or_else(|| panic!("leviculum-nrf/src/lib.rs: {what} has no body"));
    let mut depth = 0usize;
    for (i, c) in after[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &after[open..open + i + 1];
                }
            }
            _ => {}
        }
    }
    panic!("leviculum-nrf/src/lib.rs: {what} body is not brace-balanced");
}

#[test]
fn the_fault_paths_still_capture_rather_than_log() {
    let lib = nrf_source("lib.rs");
    for (marker, what) in [
        ("#[panic_handler]", "the panic handler"),
        ("unsafe fn HardFault(", "the HardFault handler"),
    ] {
        let body = body_after(&lib, marker, what);
        assert!(
            !body.contains("log_fmt") && !body.contains("log_critical!"),
            "{what} now logs; it runs with the executor dead, and whether its line \
             can carry a real t= stamp is a decision that has to be made explicitly \
             rather than inherited"
        );
    }
}

/// Extract `(site, timeout_ms, dark_ms)` from an `[SX_RX_ARM]` line.
///
/// `dark_ms` is `Some(None)` for the boot arm, which carries the word `first`
/// rather than a digit: there is no previous window to measure from, and the
/// outer `Some` says the field was present and understood. A consumer that
/// coerced that to zero would put a fabricated measurement — "the radio was
/// re-armed instantly" — into the population this line exists to characterise.
#[allow(clippy::type_complexity)]
fn parse_rx_arm(line: &str) -> Option<(String, u32, Option<u64>)> {
    let idx = line.find("[SX_RX_ARM] ")?;
    let rest = &line[idx + "[SX_RX_ARM] ".len()..];
    let mut site = None;
    let mut timeout_ms = None;
    let mut dark_ms = None;
    for token in rest.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        match key {
            "site" => site = Some(value.to_string()),
            "timeout_ms" => timeout_ms = Some(value.parse().ok()?),
            "dark_ms" => {
                dark_ms = Some(if value == "first" {
                    None
                } else {
                    Some(value.parse().ok()?)
                })
            }
            _ => {} // the ` t=` stamp, and anything appended after it
        }
    }
    Some((site?, timeout_ms?, dark_ms?))
}

/// The receiver-arming line parses, in every shape the firmware emits it.
///
/// This is the line that makes "was the radio listening at instant X"
/// readable off a capture instead of inferred: `t=` is the instant the
/// receiver went live and `dark_ms` closes the span back to the previous
/// window's end. Both halves are useless if a consumer cannot split them out.
#[test]
fn receiver_arming_line_parses() {
    assert_eq!(
        parse_rx_arm("[SX_RX_ARM] site=idle timeout_ms=0 dark_ms=3 t=123456"),
        Some(("idle".to_string(), 0, Some(3)))
    );
    assert_eq!(
        parse_rx_arm("[SX_RX_ARM] site=ack timeout_ms=2590 dark_ms=0 t=91422"),
        Some(("ack".to_string(), 2590, Some(0)))
    );
    // The boot arm. Not a zero, and a parser must not turn it into one.
    assert_eq!(
        parse_rx_arm("[SX_RX_ARM] site=idle timeout_ms=0 dark_ms=first t=412"),
        Some(("idle".to_string(), 0, None))
    );
    // A replay of the previous boot's tail still parses.
    assert_eq!(
        parse_rx_arm(
            "[INFO!] [PERSISTENT_LOG] [SX_RX_ARM] site=yield timeout_ms=5180 dark_ms=461 t=9 t=2"
        ),
        Some(("yield".to_string(), 5180, Some(461)))
    );
    // A missing field is a parse failure, not a default.
    assert_eq!(
        parse_rx_arm("[SX_RX_ARM] site=csma timeout_ms=400 t=7"),
        None
    );
    assert_eq!(
        parse_rx_arm("[T114_LORA_LOOP] op=rx_timeout duration_ms=500"),
        None
    );
}

/// The two windows a capture must be able to tell apart carry different tags.
///
/// The six sites are the loop's six listening windows. Their timeouts
/// overlap — the airtime hold and the ack window are the same length by
/// construction — so the tag is the only thing distinguishing them.
#[test]
fn every_listening_window_has_its_own_tag() {
    let hold = "[SX_RX_ARM] site=hold timeout_ms=2590 dark_ms=1 t=1000";
    let ack = "[SX_RX_ARM] site=ack timeout_ms=2590 dark_ms=1 t=2000";
    let (hold_site, hold_to, _) = parse_rx_arm(hold).expect("hold parses");
    let (ack_site, ack_to, _) = parse_rx_arm(ack).expect("ack parses");
    assert_eq!(
        hold_to, ack_to,
        "the two windows really are the same length"
    );
    assert_ne!(hold_site, ack_site);
}

/// The arming line does not disturb the parsers that came before it.
#[test]
fn the_arming_line_is_not_mistaken_for_another_line() {
    assert_eq!(
        parse_heap_line("[SX_RX_ARM] site=idle timeout_ms=0 dark_ms=3 t=1"),
        None
    );
    assert_eq!(
        parse_stamp("[SX_RX_ARM] site=idle timeout_ms=0 dark_ms=3 t=123456"),
        Some(123456)
    );
    // `dark_ms=first` is a word in a numeric-looking field; the stamp parser
    // must still find the stamp past it.
    assert_eq!(
        parse_stamp("[SX_RX_ARM] site=idle timeout_ms=0 dark_ms=first t=412"),
        Some(412)
    );
}

/// The firmware still emits the arming line, at the `SetRx` and nowhere else,
/// and from every one of the six windows.
///
/// Three separate facts, and the middle one is the load-bearing one: a line
/// logged anywhere but immediately after the `SET_RX` command would carry a
/// `t=` that is not the instant the receiver went live, and the whole span
/// arithmetic downstream would be off by however much code sits in between.
#[test]
fn the_firmware_still_emits_the_receiver_arming_line() {
    let sx = nrf_source("sx1262.rs");
    assert!(
        sx.contains(r#""[SX_RX_ARM] ""#),
        "leviculum-nrf/src/sx1262.rs no longer emits the `[SX_RX_ARM] ` tag"
    );
    // The arm report is taken after the SetRx write and before the DIO1 wait.
    let set_rx = sx
        .find("self.write_command(opcode::SET_RX, &t).await?;")
        .expect("leviculum-nrf/src/sx1262.rs no longer issues SetRx");
    // Matched on `.arm(` rather than the whole receiver expression: rustfmt
    // splits `self.rx_arm.arm(..)` across lines once the argument list grows,
    // and a pin that a reformat can break is a pin nobody keeps.
    let arm = sx
        .find(".arm(site, timeout_ms,")
        .expect("leviculum-nrf/src/sx1262.rs no longer reports the arming");
    let wait = sx[set_rx..]
        .find("self.dio1.wait_for_high()")
        .map(|i| i + set_rx)
        .expect("leviculum-nrf/src/sx1262.rs no longer waits on DIO1 after SetRx");
    assert!(
        set_rx < arm && arm < wait,
        "the [SX_RX_ARM] stamp is not taken between the SetRx and the DIO1 \
         wait, so its t= is not the instant the receiver went live"
    );
    // The window end is recorded on the far side of that wait, which is what
    // makes dark_ms a gap rather than a window length.
    let ended = sx[wait..]
        .find(".window_ended(")
        .map(|i| i + wait)
        .expect("leviculum-nrf/src/sx1262.rs no longer records the window end");
    assert!(
        ended > wait,
        "the window end is recorded before the DIO1 wait, so dark_ms would \
         span a window rather than the gap between two"
    );
    // All six windows are tagged, and each exactly once: two windows sharing
    // a tag is a capture nobody can read back apart.
    let lora = nrf_source("lora.rs");
    for site in ["Idle", "Ack", "Csma", "Jitter", "Hold", "Yield"] {
        assert_eq!(
            lora.matches(&format!("RxSite::{site}")).count(),
            1,
            "leviculum-nrf/src/lora.rs does not arm exactly one window as RxSite::{site}"
        );
    }
    // The field names and their order are the parser's contract, and they
    // live in core's Display impl.
    let core_src = {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("leviculum-core/src/sx126x.rs");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };
    assert!(
        core_src.contains(r#""site={} timeout_ms={} dark_ms=""#),
        "the [SX_RX_ARM] field order or spelling changed in leviculum-core/src/sx126x.rs"
    );
    assert!(
        core_src.contains(r#"pub const DARK_MS_FIRST: &str = "first";"#),
        "the [SX_RX_ARM] no-previous-window sentinel changed in \
         leviculum-core/src/sx126x.rs; parse_rx_arm above still expects `first`"
    );
}

/// Extract `(preamble, header, rxdone, age_ms)` from an `[SX_RX_ADOPT]` or an
/// `[SX_RX_TEARDOWN]` line.
///
/// `age_ms` is `stood_ms` on the one and `armed_ms` on the other — the same
/// quantity, a window's age measured from its own arming, named for what the
/// line is about. Both are read as rates over the whole population, so a
/// consumer that skipped the all-zero lines would compute a numerator against
/// no denominator; there is deliberately nothing here that filters them.
fn parse_rx_latch(tag: &str, line: &str) -> Option<(bool, bool, bool, u32)> {
    let idx = line.find(&format!("[{tag}] "))?;
    let rest = &line[idx + tag.len() + 3..];
    let (mut preamble, mut header, mut rxdone, mut age) = (None, None, None, None);
    let bit = |v: &str| match v {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    };
    for token in rest.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        match key {
            "preamble" => preamble = Some(bit(value)?),
            "header" => header = Some(bit(value)?),
            "rxdone" => rxdone = Some(bit(value)?),
            "stood_ms" | "armed_ms" => age = Some(value.parse().ok()?),
            _ => {} // `latched=`, `site=`, the ` t=` stamp, anything appended
        }
    }
    Some((preamble?, header?, rxdone?, age?))
}

/// The adoption pair parses, in both shapes and including the all-zero case.
#[test]
fn the_adopt_and_teardown_lines_parse() {
    assert_eq!(
        parse_rx_latch(
            "SX_RX_ADOPT",
            "[SX_RX_ADOPT] latched=0x0016 preamble=1 header=1 rxdone=1 stood_ms=20 t=1421792"
        ),
        Some((true, true, true, 20))
    );
    // The denominator. A window adopted with an empty channel behind it is
    // still a sample, and it is the majority of them.
    assert_eq!(
        parse_rx_latch(
            "SX_RX_ADOPT",
            "[SX_RX_ADOPT] latched=0x0000 preamble=0 header=0 rxdone=0 stood_ms=101 t=9"
        ),
        Some((false, false, false, 101))
    );
    assert_eq!(
        parse_rx_latch(
            "SX_RX_TEARDOWN",
            "[SX_RX_TEARDOWN] site=arm preamble=1 header=0 rxdone=0 armed_ms=214 t=42"
        ),
        Some((true, false, false, 214))
    );
    // A replay of the previous boot's tail still parses.
    assert_eq!(
        parse_rx_latch(
            "SX_RX_TEARDOWN",
            "[INFO!] [PERSISTENT_LOG] [SX_RX_TEARDOWN] site=select preamble=0 header=0 \
             rxdone=0 armed_ms=3 t=9 t=2"
        ),
        Some((false, false, false, 3))
    );
    // A missing field is a parse failure, not a default, and a flag that is
    // not a bit is not a `false`.
    assert_eq!(
        parse_rx_latch(
            "SX_RX_TEARDOWN",
            "[SX_RX_TEARDOWN] site=tx preamble=0 header=0 armed_ms=3"
        ),
        None
    );
    assert_eq!(
        parse_rx_latch(
            "SX_RX_ADOPT",
            "[SX_RX_ADOPT] latched=0x0000 preamble=no header=0 rxdone=0 stood_ms=3"
        ),
        None
    );
    // The two do not answer for each other, and neither answers for the arm.
    assert_eq!(
        parse_rx_latch(
            "SX_RX_ADOPT",
            "[SX_RX_ARM] site=idle timeout_ms=0 dark_ms=1"
        ),
        None
    );
    assert_eq!(
        parse_rx_arm("[SX_RX_TEARDOWN] site=tx preamble=0 header=0 rxdone=0 armed_ms=3"),
        None
    );
}

/// The firmware still emits both halves of the adoption instrument, and the
/// field order they are read in is still the one the crate renders.
///
/// The pair only means anything together: without the teardown line the
/// adoptions are a count with nothing to compare against, and without the
/// adoption line the fix cannot report what it saved — after it lands, the
/// loss it removed can no longer be measured any other way.
#[test]
fn the_firmware_still_emits_both_halves_of_the_adoption_instrument() {
    let sx = nrf_source("sx1262.rs");
    for tag in ["[SX_RX_ADOPT] ", "[SX_RX_TEARDOWN] "] {
        // The quoted form: the tag as a string literal in the source, so a
        // mention in a comment does not satisfy the pin.
        assert!(
            sx.contains(&format!("\"{tag}\"")),
            "leviculum-nrf/src/sx1262.rs no longer emits the `{tag}` tag"
        );
    }
    let crate_src = {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("leviculum-nrf/rx-arming/src/lib.rs");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };
    for shape in [
        r#""preamble={} header={} rxdone={}""#,
        r#""latched={:#06x} {} stood_ms={}""#,
        r#""site={} {} armed_ms={}""#,
    ] {
        assert!(
            crate_src.contains(shape),
            "the field order or spelling of {shape} changed in \
             leviculum-nrf/rx-arming/src/lib.rs; parse_rx_latch above still \
             expects it"
        );
    }
}

/// Extract `(waited_ms, reason, outcome)` from an `[SX_TX_DEFER]` line.
///
/// All three fields are required. `waited_ms` alone says how much airtime the
/// transmitter gave up and nothing about what it bought; `outcome` alone is a
/// count with no cost attached; and without `reason` the two evidence bits —
/// a bare preamble and a decoded header, which earn the same bound and are not
/// the same evidence — collapse into one population that cannot be separated
/// afterwards.
fn parse_tx_defer(line: &str) -> Option<(u64, String, String)> {
    let idx = line.find("[SX_TX_DEFER] ")?;
    let rest = &line[idx + "[SX_TX_DEFER] ".len()..];
    let (mut waited, mut reason, mut outcome) = (None, None, None);
    for token in rest.split_whitespace() {
        let (key, value) = token.split_once('=')?;
        match key {
            "waited_ms" => waited = Some(value.parse().ok()?),
            "reason" => {
                reason = match value {
                    "preamble" | "header" => Some(value.to_string()),
                    _ => return None,
                }
            }
            "outcome" => {
                outcome = match value {
                    "frame" | "timeout" | "abandoned" => Some(value.to_string()),
                    _ => return None,
                }
            }
            _ => {} // the ` t=` stamp, and anything appended after it
        }
    }
    Some((waited?, reason?, outcome?))
}

/// The deferral line parses, in every outcome it can report.
#[test]
fn the_tx_defer_line_parses() {
    assert_eq!(
        parse_tx_defer("[SX_TX_DEFER] waited_ms=446 reason=header outcome=frame t=1053832"),
        Some((446, "header".into(), "frame".into()))
    );
    // The bound expiring is the case the ratio is computed against, so it has
    // to parse just as readily as the repaid one.
    assert_eq!(
        parse_tx_defer("[SX_TX_DEFER] waited_ms=728 reason=preamble outcome=timeout t=9"),
        Some((728, "preamble".into(), "timeout".into()))
    );
    assert_eq!(
        parse_tx_defer(
            "[INFO!] [PERSISTENT_LOG] [SX_TX_DEFER] waited_ms=60 reason=header \
             outcome=abandoned t=9 t=2"
        ),
        Some((60, "header".into(), "abandoned".into()))
    );
    // A missing field is a parse failure, not a default: a line with no
    // outcome would otherwise be counted as one.
    assert_eq!(
        parse_tx_defer("[SX_TX_DEFER] waited_ms=1 reason=header"),
        None
    );
    // A vocabulary this parser does not know is a firmware that moved, not a
    // value to pass through.
    assert_eq!(
        parse_tx_defer("[SX_TX_DEFER] waited_ms=1 reason=cad outcome=frame"),
        None
    );
    assert_eq!(
        parse_tx_defer("[SX_TX_DEFER] waited_ms=1 reason=header outcome=deferred"),
        None
    );
    // And it does not answer for its neighbours in the same family.
    assert_eq!(
        parse_tx_defer("[SX_RX_TEARDOWN] site=select preamble=1 header=1 rxdone=0 armed_ms=446"),
        None
    );
    assert_eq!(
        parse_rx_latch(
            "SX_RX_TEARDOWN",
            "[SX_TX_DEFER] waited_ms=1 reason=header outcome=frame"
        ),
        None
    );
}

/// The firmware still emits the deferral line, from the one site that may
/// defer, and in the grammar the parser above reads.
///
/// `site=` is deliberately absent from the line and that is what the second
/// half pins: the bound is spent once per call, so "exactly one caller" is
/// what makes the starvation argument hold, and a second `disarm_rx_for_tx`
/// appearing in the loop would turn one frame's airtime into a wait that
/// compounds. A capture cannot see that; this can.
#[test]
fn the_firmware_still_emits_the_transmit_deferral_line() {
    let sx = nrf_source("sx1262.rs");
    assert!(
        sx.contains(r#""[SX_TX_DEFER] ""#),
        "leviculum-nrf/src/sx1262.rs no longer emits the `[SX_TX_DEFER] ` tag"
    );
    let lora = nrf_source("lora.rs");
    assert_eq!(
        lora.matches("disarm_rx_for_tx(").count(),
        1,
        "exactly one site in leviculum-nrf/src/lora.rs may defer a transmit; \
         the bound is per call, so a second one compounds it"
    );
    // And that site is the idle select's outgoing arm, which is what the
    // teardown table named.
    assert!(
        lora.contains("RxTeardownBy::Select"),
        "the deferring site is no longer the idle select's outgoing arm"
    );
    let crate_src = {
        let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("leviculum-nrf/rx-arming/src/lib.rs");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    };
    assert!(
        crate_src.contains(r#""waited_ms={} reason={} outcome={}""#),
        "the [SX_TX_DEFER] field order or spelling changed in \
         leviculum-nrf/rx-arming/src/lib.rs; parse_tx_defer above still expects it"
    );
    for tag in [
        "\"preamble\"",
        "\"header\"",
        "\"frame\"",
        "\"timeout\"",
        "\"abandoned\"",
    ] {
        assert!(
            crate_src.contains(tag),
            "the [SX_TX_DEFER] vocabulary lost {tag} in \
             leviculum-nrf/rx-arming/src/lib.rs; parse_tx_defer above rejects \
             anything else"
        );
    }
}
