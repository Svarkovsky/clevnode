//! The startup facts a board must be able to state about itself, and the
//! sink each of them is written to.
//!
//! # Why routing is a property and not a call-site detail
//!
//! The firmware has two log sinks. `log_fmt` is gated on
//! `RUNTIME_DRAIN_OPEN` and silently drops everything until a reader has
//! attached to the debug port; `log_fmt_critical` bypasses that gate. The
//! difference is invisible at a call site — the two spellings differ by one
//! word — and it decides whether a line can ever be read on a board that
//! booted before anyone was listening.
//!
//! Which is most boards. Measured at the bench on a reflashed T114, with a
//! reader already waiting for the port to come back:
//!
//! ```text
//! [INFO!] [STG] lora-init t=191
//! [INFO!] [STG] main-loop t=1886
//! [SX_REG] rxgain_before=0x94 rxgain_after=0x96 txmod=0x04 t=1921
//! [LOG_GATE] opened, dropped 20 runtime lines pre-attach t=2581
//! ```
//!
//! The radio task ran from `t=191` and the gate opened at `t=2581`: twenty
//! lines emitted in between were dropped, and among them were the only two
//! lines that say what the radio was set to and under what lawful duty-cycle
//! cap it was transmitting. Neither is obtainable from a running board any
//! other way — the applied configuration lives in the radio's registers and
//! the enforced cap lives in the airtime tracker, and nothing reads either
//! back out. For a *regulatory* limit that is the wrong property to have:
//! the question "under what cap is this board transmitting?" has to be
//! answerable at the bench, not reconstructable from the source.
//!
//! So the two are emitted here, through [`LineSink`], with their route
//! stated as data. They are one-shot startup facts and not a stream: the
//! cost is two lines per boot. Anything that repeats belongs on the gated
//! sink, where it cannot lap the ring before a reader arrives.
//!
//! A third joins them on the same terms: [`tx_power_programmed`] states what
//! the PA was actually programmed with. The settings line above reports the
//! *request*, and for as long as the driver hardcoded `SetTxParams` those two
//! were different numbers — a board configured for 2 dBm said `txp=2` and
//! radiated about 14. Nothing readable off a running board named the
//! programmed power, so the sweep that would have caught it had nothing to
//! verify a point against. Unconditional, like the cap line: "what is this
//! board radiating" is not a question a silence can answer.
//!
//! The functions take a sink rather than calling the firmware's logger, so
//! the routing is assertable on the host: `leviculum-nrf` cross-compiles to
//! thumbv7em and runs no host tests, and reading the source to check which
//! function a call site names proves nothing about what the board emits.

use core::fmt::Arguments;

/// Which of the firmware's two log sinks a line is written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Bypasses the runtime drain gate: reaches the host even when the line
    /// was emitted before anything attached to the debug port.
    Critical,
    /// Dropped while the runtime drain gate is closed.
    Gated,
}

/// The firmware's debug log, as the facts below see it.
///
/// The firmware's implementation maps [`Route::Critical`] onto
/// `log_fmt_critical` and [`Route::Gated`] onto `log_fmt`; a test's
/// implementation records the route it was handed.
pub trait LineSink {
    /// Emit one line: the prefix, the body, and the route it goes out on.
    fn line(&mut self, route: Route, prefix: &str, args: Arguments);
}

/// The radio settings the modem was actually programmed with.
///
/// Human-readable throughout — `bw_hz` and `cr_denom` rather than the
/// SX1262 register codes — because the reader of this line is an operator
/// answering "what is this board doing", not the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveRadioConfig {
    pub freq_hz: u32,
    pub sf: u8,
    pub bw_hz: u32,
    pub cr_denom: u8,
    pub txp_dbm: i8,
    pub csma: bool,
}

/// `[LORA] active config: …` — what the radio is set to, emitted after
/// every successful `configure_lora`.
///
/// Critical: it is emitted once at boot and once per runtime
/// reconfiguration, and a board reconfigured in the field has to be able to
/// say what it was reconfigured to.
pub fn active_radio_config<S: LineSink>(sink: &mut S, c: &ActiveRadioConfig) {
    sink.line(
        Route::Critical,
        "[LORA] ",
        format_args!(
            "active config: freq={} sf={} bw={} cr={} txp={} csma={}",
            c.freq_hz, c.sf, c.bw_hz, c.cr_denom, c.txp_dbm, c.csma
        ),
    );
}

/// Who chose a limit the airtime tracker is enforcing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    /// The host sent this value explicitly, and it wins verbatim — including
    /// an explicit `0`, which means *unlimited* and not *unset*.
    Host,
    /// The firmware derived it from its own TX frequency because no explicit
    /// value reached it.
    Derived,
    /// Taken verbatim from the active configuration, with no way to tell an
    /// explicit host value from the compiled default.
    ///
    /// Only the short-term limit is ever this. The firmware derives no
    /// short-term cap, and the radio-config wire format carries no presence
    /// bit for `st_alock` the way it does for `lt_alock`, so a short frame
    /// that omitted the field and a host that sent `0` arrive identical. The
    /// *value* below is still the one loaded into the tracker; only its
    /// authorship is unknown, and the line says so rather than guessing.
    Config,
}

impl core::fmt::Display for LimitSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            LimitSource::Host => "host",
            LimitSource::Derived => "derived",
            LimitSource::Config => "config",
        })
    }
}

/// An `alock` u16 rendered the way a human reads a duty cycle.
///
/// The RNode `CMD_ST_ALOCK` / `CMD_LT_ALOCK` encoding is percent × 100, so
/// `1000` is 10 % and `0` is *no limit at all*. Those two facts are why this
/// exists: `lt=0` looks like the smallest possible cap and is the largest,
/// and a reader who mistakes "unlimited" for "0.1 %" has made the one error
/// on this line that has a legal consequence. Rendered with the raw number
/// beside it, never instead of it, so the line stays machine-readable.
struct Cap(u16);

impl core::fmt::Display for Cap {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 == 0 {
            f.write_str("unlimited")
        } else {
            // Integer division: no float formatter in the firmware's log path.
            write!(f, "{}.{:02}%", self.0 / 100, self.0 % 100)
        }
    }
}

/// The airtime limits the tracker is actually enforcing, and where each came
/// from.
///
/// Both limits are the RNode u16 encoding (percent × 100); `0` is unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AirtimeLimits {
    /// Effective long-term limit loaded into the tracker — the host's value
    /// or the frequency-derived lawful one, whichever won.
    pub lt_alock: u16,
    pub lt_source: LimitSource,
    /// Short-term limit loaded into the tracker.
    pub st_alock: u16,
    pub st_source: LimitSource,
    /// The TX frequency the lawful default is derived from.
    pub freq_hz: u32,
    /// The lawful long-term cap this frequency derives, stated whether or not
    /// it is the one being enforced: with it on the line, an operator can see
    /// that a host value is under, at, or above the lawful one without
    /// looking a sub-band up in a table.
    pub lawful_lt_alock: u16,
}

/// `[LORA_AIRTIME_LOCK] limits …` — the caps the board is transmitting under
/// and who chose them, emitted at radio bring-up and on every reconfiguration.
///
/// Critical for the same reason as the settings above, and more sharply: this
/// is the number a compliance question is about.
///
/// Unconditional, which is the whole point. It used to be emitted only when
/// the firmware derived the cap itself, so an explicit host value produced no
/// line at all and the cap in force had to be inferred from a silence — and
/// an explicit `0` (unlimited: a legitimate bench setting, an unacceptable
/// field one) was indistinguishable from a small derived cap from the outside.
pub fn airtime_limits<S: LineSink>(sink: &mut S, l: &AirtimeLimits) {
    sink.line(
        Route::Critical,
        "[LORA_AIRTIME_LOCK] ",
        format_args!(
            "limits lt={} lt_cap={} lt_src={} st={} st_cap={} st_src={} freq={} lawful={}",
            l.lt_alock,
            Cap(l.lt_alock),
            l.lt_source,
            l.st_alock,
            Cap(l.st_alock),
            l.st_source,
            l.freq_hz,
            l.lawful_lt_alock,
        ),
    );
}

/// Everything the transmit-power line states, as it was written to the chip.
///
/// Built from `sx126x::TxPowerProgram`, which the driver gets back from the
/// call that did the writing — so every field here is a byte that went out on
/// the SPI bus, not one the caller intended to send.
pub struct TxPowerProgrammed {
    /// The configured request, before any clamp.
    pub requested_dbm: i8,
    /// The first `SetTxParams` argument: what the chip was told to radiate.
    pub programmed_dbm: i8,
    /// The `SetPaConfig` argument block: PADutyCycle, HPMax, DeviceSel, PALut.
    pub pa_config: [u8; 4],
    /// The `SetTxParams` ramp byte.
    pub ramp: u8,
    /// The `REG_OCP` byte.
    pub ocp: u8,
    /// Whether the request had to be brought into the part's range.
    pub clamped: bool,
}

/// `[SX_TX_POWER] requested_dbm=… tx_params_dbm=… pa=… ocp=… clamped=…` — the
/// transmit power the radio is actually running, readable off a running board
/// without inference.
///
/// The line this replaces (`[SX_PA_PROFILE]`) reported a *substitution*, and
/// only when there was one. That was the right shape while the driver could
/// only reach four powers; it is the wrong shape now that it reaches all 32,
/// because the question at the rig is not "was I given something odd" but
/// "what is this board radiating right now" — and a fact that is emitted only
/// in the interesting case cannot answer that. So this one is unconditional,
/// the same decision and for the same reason as `[LORA_AIRTIME_LOCK]` beside
/// it: a cap in force had to be inferred from a silence, once.
///
/// Four things, none of them derivable from the others:
///
/// * `requested_dbm` — what the configuration asked for;
/// * `tx_params_dbm` — the byte `SetTxParams` was given, i.e. the output;
/// * `pa` — the four `SetPaConfig` bytes, so a board that somehow programmed
///   a different PA row says so rather than being taken on trust;
/// * `clamped` — whether the request was outside the part's range. Clamped and
///   announced, never refused.
///
/// `ramp` and `ocp` ride along because they are the other two bytes of the
/// same three-op sequence and cost nothing to state; `ocp` in particular is
/// the one value that could make a board deliver less than `tx_params_dbm`
/// says.
///
/// Critical, for the same reason as the two facts above: a board that boots
/// before a reader attaches drops everything gated, and this is the fact a
/// sweep has to confirm each point against before it measures it.
pub fn tx_power_programmed<S: LineSink>(sink: &mut S, p: &TxPowerProgrammed) {
    sink.line(
        Route::Critical,
        "[SX_TX_POWER] ",
        format_args!(
            "requested_dbm={} tx_params_dbm={} pa=0x{:02X},0x{:02X},0x{:02X},0x{:02X} \
             ramp=0x{:02X} ocp=0x{:02X} clamped={}",
            p.requested_dbm,
            p.programmed_dbm,
            p.pa_config[0],
            p.pa_config[1],
            p.pa_config[2],
            p.pa_config[3],
            p.ramp,
            p.ocp,
            if p.clamped { "yes" } else { "no" },
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    /// Records what the firmware's logger would have been handed. The line
    /// is rendered through the real [`crate::format_line`], so the text
    /// asserted below is the text that reaches the ring buffer.
    #[derive(Default)]
    struct Recorder {
        lines: Vec<(Route, String)>,
    }

    impl LineSink for Recorder {
        fn line(&mut self, route: Route, prefix: &str, args: Arguments) {
            let mut buf = [0u8; 1024];
            let bytes = crate::format_line(&mut buf, prefix, args, 191);
            self.lines
                .push((route, String::from_utf8_lossy(bytes).into_owned()));
        }
    }

    fn eu_medium() -> ActiveRadioConfig {
        ActiveRadioConfig {
            freq_hz: 869_463_000,
            sf: 8,
            bw_hz: 125_000,
            cr_denom: 5,
            txp_dbm: 22,
            csma: true,
        }
    }

    #[test]
    fn the_applied_configuration_goes_out_on_the_critical_sink() {
        let mut sink = Recorder::default();
        active_radio_config(&mut sink, &eu_medium());
        assert_eq!(
            sink.lines,
            [(
                Route::Critical,
                String::from(
                    "[LORA] active config: freq=869463000 sf=8 bw=125000 cr=5 txp=22 \
                     csma=true t=191\r\n"
                )
            )]
        );
    }

    /// 869.463 MHz falls in ERC 70-03 h1.7, 10 % duty cycle; the
    /// `CMD_LT_ALOCK` encoding of 10 % is 1000.
    fn derived_eu_limits() -> AirtimeLimits {
        AirtimeLimits {
            lt_alock: 1000,
            lt_source: LimitSource::Derived,
            st_alock: 0,
            st_source: LimitSource::Config,
            freq_hz: 869_463_000,
            lawful_lt_alock: 1000,
        }
    }

    #[test]
    fn a_derived_cap_names_the_frequency_it_came_from() {
        let mut sink = Recorder::default();
        airtime_limits(&mut sink, &derived_eu_limits());
        assert_eq!(
            sink.lines,
            [(
                Route::Critical,
                String::from(
                    "[LORA_AIRTIME_LOCK] limits lt=1000 lt_cap=10.00% lt_src=derived st=0 \
                     st_cap=unlimited st_src=config freq=869463000 lawful=1000 t=191\r\n"
                )
            )]
        );
    }

    /// The case that used to produce no line at all: the host sent an explicit
    /// `lt_alock`, so the firmware derived nothing. Both limits are the host's,
    /// and the lawful value for the frequency is stated beside them so the two
    /// can be compared without a sub-band table — here 5 % against a lawful
    /// 10 %, i.e. an operator who chose to stay under it.
    #[test]
    fn an_explicit_host_cap_is_stated_next_to_the_lawful_one() {
        let mut sink = Recorder::default();
        airtime_limits(
            &mut sink,
            &AirtimeLimits {
                lt_alock: 500,
                lt_source: LimitSource::Host,
                st_alock: 1500,
                st_source: LimitSource::Host,
                freq_hz: 869_463_000,
                lawful_lt_alock: 1000,
            },
        );
        assert_eq!(
            sink.lines,
            [(
                Route::Critical,
                String::from(
                    "[LORA_AIRTIME_LOCK] limits lt=500 lt_cap=5.00% lt_src=host st=1500 \
                     st_cap=15.00% st_src=host freq=869463000 lawful=1000 t=191\r\n"
                )
            )]
        );
    }

    /// The control. An explicit `0` is the dangerous value on this line: it
    /// disables the cap, and it is the one number that looks like the
    /// *smallest* cap while being the absence of one. A reader who cannot
    /// tell it from a derived 0.1 % has a compliance error, not a cosmetic
    /// one, so the two renderings are asserted against each other.
    #[test]
    fn an_explicit_zero_reads_as_unlimited_and_not_as_a_small_cap() {
        let mut unlimited = Recorder::default();
        airtime_limits(
            &mut unlimited,
            &AirtimeLimits {
                lt_alock: 0,
                lt_source: LimitSource::Host,
                st_alock: 0,
                st_source: LimitSource::Host,
                freq_hz: 869_463_000,
                lawful_lt_alock: 1000,
            },
        );

        // 864 MHz derives ERC 70-03 h1.3's 0.1 % cap: the smallest one the
        // table holds, and the one an unlimited board could be mistaken for.
        let mut smallest = Recorder::default();
        airtime_limits(
            &mut smallest,
            &AirtimeLimits {
                lt_alock: 10,
                lt_source: LimitSource::Derived,
                st_alock: 0,
                st_source: LimitSource::Config,
                freq_hz: 864_000_000,
                lawful_lt_alock: 10,
            },
        );

        assert!(
            unlimited.lines[0].1.contains("lt=0 lt_cap=unlimited"),
            "got {:?}",
            unlimited.lines[0].1
        );
        assert!(
            smallest.lines[0].1.contains("lt=10 lt_cap=0.10%"),
            "got {:?}",
            smallest.lines[0].1
        );
        assert_ne!(unlimited.lines[0].1, smallest.lines[0].1);
        // And the reason the two differ is stated, not left to the number:
        // one is a choice, the other is a band.
        assert!(unlimited.lines[0].1.contains("lt_src=host"));
        assert!(smallest.lines[0].1.contains("lt_src=derived"));
        // An operator who disabled a cap that exists can see that it exists.
        assert!(unlimited.lines[0].1.contains("lawful=1000"));
    }

    /// A frequency the table does not cover derives nothing, and `lawful=0`
    /// has to say so: "this band has no cap in our table" is a different
    /// statement from "the operator switched the cap off", and only the
    /// `lt_src` field separates them.
    #[test]
    fn an_out_of_band_frequency_states_that_it_derived_nothing() {
        let mut sink = Recorder::default();
        airtime_limits(
            &mut sink,
            &AirtimeLimits {
                lt_alock: 0,
                lt_source: LimitSource::Derived,
                st_alock: 0,
                st_source: LimitSource::Config,
                freq_hz: 915_000_000,
                lawful_lt_alock: 0,
            },
        );
        assert_eq!(sink.lines[0].0, Route::Critical);
        assert!(
            sink.lines[0].1.contains(
                "limits lt=0 lt_cap=unlimited lt_src=derived st=0 st_cap=unlimited \
                 st_src=config freq=915000000 lawful=0"
            ),
            "got {:?}",
            sink.lines[0].1
        );
    }

    /// `grep AIRTIME` on a fresh boot answers the question with no inference
    /// from silence: one line, whatever the origins are.
    #[test]
    fn every_origin_combination_emits_exactly_one_greppable_line() {
        for (lt_source, st_source) in [
            (LimitSource::Host, LimitSource::Host),
            (LimitSource::Derived, LimitSource::Config),
            (LimitSource::Host, LimitSource::Config),
        ] {
            let mut sink = Recorder::default();
            airtime_limits(
                &mut sink,
                &AirtimeLimits {
                    lt_source,
                    st_source,
                    ..derived_eu_limits()
                },
            );
            assert_eq!(sink.lines.len(), 1, "{lt_source:?}/{st_source:?}");
            assert!(sink.lines[0].1.contains("AIRTIME"));
        }
    }

    /// The PA bytes below are `sx126x::PA_CONFIG_HIGH_POWER`, spelled out
    /// because this crate does not depend on core.
    fn programmed(requested_dbm: i8, programmed_dbm: i8, clamped: bool) -> TxPowerProgrammed {
        TxPowerProgrammed {
            requested_dbm,
            programmed_dbm,
            pa_config: [0x04, 0x07, 0x00, 0x01],
            ramp: 0x04,
            ocp: 0x38,
            clamped,
        }
    }

    /// The case the line exists for: 23 hardware scenarios configure 2 dBm,
    /// and the only line that mentioned it said `txp=2` — the request. Now the
    /// programmed byte is beside it, so the two can be read against each other
    /// instead of one standing in for the other.
    #[test]
    fn the_line_states_the_request_the_programmed_byte_and_the_pa_config() {
        let mut sink = Recorder::default();
        tx_power_programmed(&mut sink, &programmed(2, 2, false));
        assert_eq!(
            sink.lines,
            [(
                Route::Critical,
                String::from(
                    "[SX_TX_POWER] requested_dbm=2 tx_params_dbm=2 pa=0x04,0x07,0x00,0x01 \
                     ramp=0x04 ocp=0x38 clamped=no t=191\r\n"
                )
            )]
        );
    }

    /// A clamp is named as one. The request stays on the line beside the
    /// programmed value: "clamped" without both numbers would say that
    /// something was changed without saying from what.
    #[test]
    fn a_clamped_request_keeps_both_numbers_and_says_it_was_clamped() {
        let mut sink = Recorder::default();
        tx_power_programmed(&mut sink, &programmed(37, 22, true));
        assert_eq!(
            sink.lines,
            [(
                Route::Critical,
                String::from(
                    "[SX_TX_POWER] requested_dbm=37 tx_params_dbm=22 pa=0x04,0x07,0x00,0x01 \
                     ramp=0x04 ocp=0x38 clamped=yes t=191\r\n"
                )
            )]
        );
    }

    /// **The control that the old line could not have.** A negative power is a
    /// real configuration — `lnflash --radio-txpower` accepts -9 — and it has
    /// to survive the formatting as a negative number rather than as a wrapped
    /// byte. `247` on this line would be the u8 reinterpretation of -9.
    #[test]
    fn a_negative_power_is_printed_as_a_negative_number() {
        let mut sink = Recorder::default();
        tx_power_programmed(&mut sink, &programmed(-9, -9, false));
        assert!(
            sink.lines[0]
                .1
                .contains("requested_dbm=-9 tx_params_dbm=-9"),
            "{}",
            sink.lines[0].1
        );
        assert!(!sink.lines[0].1.contains("247"), "{}", sink.lines[0].1);
    }

    /// The line is unconditional: every power produces exactly one, including
    /// the ones that need no clamp. The old line's silence in the ordinary
    /// case is precisely what made "what is this board radiating" unanswerable
    /// at the bench.
    #[test]
    fn every_power_produces_a_line() {
        for dbm in -9i8..=22 {
            let mut sink = Recorder::default();
            tx_power_programmed(&mut sink, &programmed(dbm, dbm, false));
            assert_eq!(sink.lines.len(), 1, "{dbm} dBm: {:?}", sink.lines);
            assert_eq!(sink.lines[0].0, Route::Critical, "{dbm} dBm");
        }
    }

    /// The reason these two are on the critical sink at all: a board that
    /// boots before a reader attaches drops everything gated, and the two
    /// facts an operator needs must survive that.
    #[test]
    fn a_pre_attach_boot_keeps_both_facts_and_drops_the_gated_ones() {
        struct PreAttach {
            kept: Vec<String>,
            dropped: usize,
        }
        impl LineSink for PreAttach {
            fn line(&mut self, route: Route, prefix: &str, args: Arguments) {
                if route == Route::Gated {
                    self.dropped += 1;
                    return;
                }
                let mut buf = [0u8; 1024];
                let bytes = crate::format_line(&mut buf, prefix, args, 191);
                self.kept.push(String::from_utf8_lossy(bytes).into_owned());
            }
        }

        let mut sink = PreAttach {
            kept: Vec::new(),
            dropped: 0,
        };
        // A routine runtime line, for contrast: it is gated and is lost.
        sink.line(Route::Gated, "[LORA] ", format_args!("RX 41 bytes"));
        active_radio_config(&mut sink, &eu_medium());
        airtime_limits(&mut sink, &derived_eu_limits());

        assert_eq!(sink.dropped, 1);
        assert_eq!(sink.kept.len(), 2, "kept: {:?}", sink.kept);
        assert!(sink.kept[0].starts_with("[LORA] active config: "));
        assert!(sink.kept[1].starts_with("[LORA_AIRTIME_LOCK] limits "));
    }
}
