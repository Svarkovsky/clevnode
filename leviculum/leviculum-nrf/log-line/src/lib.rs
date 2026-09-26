//! Line shaping for the firmware's debug log, including the board-side
//! uptime stamp.
//!
//! # Why the board stamps its own lines
//!
//! Log lines are formatted into `LOG_RING` and drained later by
//! `debug_writer_task` in 64-byte USB packets on a 100 ms timeout loop.
//! A host-side capture therefore records *drain* time, not *emission*
//! time: deltas between two lines are compressed to zero inside one
//! drain burst and inflated to the loop period across bursts. A
//! measurement taken that way came out bimodal at 0 ms and exactly
//! 50 ms on two boards at once — the quantisation of the drain, not
//! anything either board did. Across two boards there is no common
//! time base at all.
//!
//! A stamp taken at format time, before the line enters the ring,
//! removes the drain from the measurement entirely.
//!
//! # The shape
//!
//! ```text
//! <prefix><body> t=<uptime_ms>\r\n
//! ```
//!
//! `t=` goes at the END and not in front of the `[TAG]`. Every existing
//! consumer anchors on the tag — `scripts/catch-reboot.sh` greps
//! `PANIC_COUNT] total`, `lnflash::verify` splits on `[FW_BUILD]`,
//! periculum's `fw_build_version_in` finds `[FW_BUILD]` — and a leading
//! stamp would break all of them at once. Appending also matches the
//! `EVENT key=val t=<ms>` convention the project already uses for
//! protocol events.
//!
//! # The last-`t=` rule
//!
//! A line may contain more than one `t=`. The boot-time replay of the
//! persistent tail wraps a line from the PREVIOUS boot, stamp and all,
//! inside a line of THIS boot:
//!
//! ```text
//! [INFO!] [PERSISTENT_LOG] [LORA] RX 41 bytes t=91422 t=137
//! ```
//!
//! Both stamps are true. The inner one is when the wrapped line was
//! emitted, last boot; the trailing one is when the replay line itself
//! was emitted, this boot. The stamp of a line is therefore always its
//! LAST `t=`, which is what [`parse_stamp`] reads. The same holds for a
//! `tracing` event whose own fields happen to include a `t`.
//!
//! # Which sink a line goes to
//!
//! Shaping is half of what makes a line readable; the other half is
//! whether it is emitted at all. [`facts`] holds the startup lines whose
//! *route* — the gated sink or the one that bypasses the gate — is a
//! property rather than a call-site detail, and states that route as data
//! so it can be asserted on the host.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod facts;

use core::fmt::Write;

/// Upper bound on the bytes ` t=<ms>` occupies: three for ` t=` and
/// twenty for the widest decimal `u64`.
pub const STAMP_MAX: usize = 3 + 20;

/// The trailing CRLF every line carries.
pub const TERMINATOR: usize = 2;

/// How many bytes of a `cap`-byte line buffer the prefix and body may
/// use.
///
/// The remainder is reserved so the stamp and the CRLF can never be the
/// part that gets truncated. The formatter has always dropped whatever
/// overran its buffer; with the stamp appended last, that silent drop
/// would land on the stamp — producing either a line that reads as
/// unstamped or, worse, one carrying half its digits. A stamp that is
/// sometimes wrong is worse than no stamp, so the body yields instead.
pub const fn body_limit(cap: usize) -> usize {
    cap.saturating_sub(STAMP_MAX + TERMINATOR)
}

/// A `core::fmt::Write` sink appending into `buf`, dropping whatever
/// does not fit.
///
/// Truncation rather than error is the firmware's long-standing
/// behaviour here: a log line is diagnostics, and a formatting failure
/// must never propagate into the code being diagnosed. This type makes
/// the boundary explicit so callers can cap the body at
/// [`body_limit`].
pub struct Sink<'a> {
    buf: &'a mut [u8],
    len: &'a mut usize,
}

impl<'a> Sink<'a> {
    /// Append into `buf`, continuing at `*len` and advancing it.
    pub fn new(buf: &'a mut [u8], len: &'a mut usize) -> Self {
        Self { buf, len }
    }
}

impl Write for Sink<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let remaining = self.buf.len().saturating_sub(*self.len);
        let to_copy = s.len().min(remaining);
        self.buf[*self.len..*self.len + to_copy].copy_from_slice(&s.as_bytes()[..to_copy]);
        *self.len += to_copy;
        Ok(())
    }
}

/// Append ` t=<stamp_ms>` and the CRLF, closing the line.
///
/// Call once, after the body, with the body capped at
/// `body_limit(buf.len())`. Returns the new length.
pub fn finish(buf: &mut [u8], mut len: usize, stamp_ms: u64) -> usize {
    let mut w = Sink::new(buf, &mut len);
    let _ = write!(w, " t={stamp_ms}");
    let _ = w.write_str("\r\n");
    len
}

/// Format one complete log line: `<prefix><args> t=<stamp_ms>\r\n`.
///
/// `stamp_ms` is milliseconds of board uptime, taken by the caller at
/// the moment of the log call.
pub fn format_line<'a>(
    buf: &'a mut [u8],
    prefix: &str,
    args: core::fmt::Arguments,
    stamp_ms: u64,
) -> &'a [u8] {
    let mut len = 0usize;
    {
        let limit = body_limit(buf.len());
        let mut w = Sink::new(&mut buf[..limit], &mut len);
        let _ = w.write_str(prefix);
        let _ = w.write_fmt(args);
    }
    let len = finish(buf, len, stamp_ms);
    &buf[..len]
}

/// The uptime stamp of a captured line: its LAST `t=` field.
///
/// `None` for a line that carries none — every line the current
/// firmware emits at runtime carries one, so a `None` on a capture is
/// itself a finding (old firmware, or a line torn by the reader).
///
/// Host-side consumers should use this rather than a hand-rolled
/// `find("t=")`, which would pick up the replayed inner stamp of a
/// `[PERSISTENT_LOG]` line.
pub fn parse_stamp(line: &str) -> Option<u64> {
    line.trim_end_matches(['\r', '\n'])
        .rsplit(' ')
        .find_map(|field| field.strip_prefix("t="))
        .and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;

    fn line(prefix: &str, args: core::fmt::Arguments, stamp: u64) -> String {
        let mut buf = [0u8; 1024];
        let bytes = format_line(&mut buf, prefix, args, stamp);
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn a_whole_line_has_the_shape_the_consumers_expect() {
        assert_eq!(
            line(
                "[INFO] ",
                format_args!("RX {} bytes rssi={}", 41, -97),
                1234
            ),
            "[INFO] RX 41 bytes rssi=-97 t=1234\r\n"
        );
    }

    #[test]
    fn the_empty_body_still_closes_the_line() {
        // `[T114_SX_TIMEOUT]` logs with an empty body; the prefix's own
        // trailing space then meets the stamp's leading one.
        assert_eq!(
            line("[T114_SX_TIMEOUT] ", format_args!(""), 7),
            "[T114_SX_TIMEOUT]  t=7\r\n"
        );
    }

    #[test]
    fn the_stamp_is_rendered_verbatim_at_both_ends_of_the_range() {
        assert_eq!(parse_stamp(&line("[X] ", format_args!("a"), 0)), Some(0));
        assert_eq!(
            parse_stamp(&line("[X] ", format_args!("a"), u64::MAX)),
            Some(u64::MAX)
        );
    }

    #[test]
    fn a_body_that_overruns_the_buffer_loses_the_body_not_the_stamp() {
        // The failure this reserves against: the body eats the buffer,
        // the stamp is truncated to a few digits, and the line reads as
        // a plausible-but-wrong measurement.
        let mut buf = [0u8; 64];
        let long = "0123456789".repeat(20);
        let bytes = format_line(&mut buf, "[X] ", format_args!("{long}"), 4_294_967_295);
        let text = std::str::from_utf8(bytes).unwrap();
        assert!(text.ends_with(" t=4294967295\r\n"), "got {text:?}");
        assert_eq!(parse_stamp(text), Some(4_294_967_295));
        assert!(bytes.len() <= 64);
    }

    #[test]
    fn body_limit_reserves_the_stamp_and_the_terminator() {
        assert_eq!(body_limit(1024), 1024 - 25);
        // A buffer too small for a stamp yields a zero body rather than
        // an underflow.
        assert_eq!(body_limit(4), 0);
    }

    #[test]
    fn the_stamp_of_a_replayed_line_is_the_outer_one() {
        // A [PERSISTENT_LOG] replay carries last boot's stamp inside
        // this boot's. The line's own stamp is the trailing one.
        let replayed = "[INFO!] [PERSISTENT_LOG] [LORA] RX 41 bytes t=91422 t=137\r\n";
        assert_eq!(parse_stamp(replayed), Some(137));
    }

    #[test]
    fn parse_stamp_declines_what_is_not_a_stamp() {
        assert_eq!(parse_stamp("[LORA] RX 41 bytes"), None);
        assert_eq!(parse_stamp("[LORA] t=abc"), None);
        // A `t=` glued to the end of another token is a different field,
        // not this one.
        assert_eq!(parse_stamp("[LORA] rtt=5"), None);
        assert_eq!(parse_stamp("[LORA] t="), None);
    }

    #[test]
    fn lines_emitted_in_order_carry_non_decreasing_stamps() {
        // The clock is monotonic (`embassy_time::Instant`), and the
        // formatter renders what it is given: so an ordered sequence of
        // stamps survives formatting as an ordered sequence of lines.
        let stamps = [0u64, 1, 1, 999, 1_000, 86_400_000];
        let mut prev = 0u64;
        for s in stamps {
            let rendered = parse_stamp(&line("[LORA] ", format_args!("op=rx_success"), s))
                .expect("every emitted line carries a stamp");
            assert_eq!(rendered, s);
            assert!(rendered >= prev, "{rendered} < {prev}");
            prev = rendered;
        }
    }

    #[test]
    fn a_deaf_window_is_the_arithmetic_the_stamps_were_added_for() {
        // Two consecutive rx_success lines. The radio was not listening
        // between the end of the first reception and the start of the
        // second: t2 - duration2 - t1.
        let first = "[T114_LORA_LOOP] op=rx_success duration_ms=120 t=1000\r\n";
        let second = "[T114_LORA_LOOP] op=rx_success duration_ms=80 t=1500\r\n";
        let t1 = parse_stamp(first).unwrap();
        let t2 = parse_stamp(second).unwrap();
        let d2: u64 = 80;
        assert_eq!(t2 - d2 - t1, 420);
    }
}
