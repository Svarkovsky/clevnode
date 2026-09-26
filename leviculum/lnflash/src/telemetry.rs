//! "Send telemetry? [y/N]" — the configuration half of #236.
//!
//! Lew's binding UX decisions (2026-08-22) shape every function here.
//!
//! **Defaults first.** Most users configure nothing, so the prompt's default
//! answer is *no* and one profile — station — is the one that needs no
//! thought. Telemetry that nobody asked for is telemetry nobody expects.
//!
//! **The destination hash alone suffices.** Many users know only the LXMF
//! address, so on "yes" exactly one input is required. The key is optional;
//! hash-only means the node resolves it over the air and says so honestly in
//! its own `[TELEMETRY] … state=awaiting-key` line.
//!
//! **Activation is configuration, not firmware.** Everything here runs
//! against a board that is already flashed, through the #238 control
//! envelope, so it is reachable from the flash flow and from a standalone
//! session alike ([`crate::flow::set_telemetry`]).
//!
//! Nothing in this module opens a port — the open lives with
//! [`crate::flow`], which proves it landed on the intended board — so the
//! prompts, the flags and the bytes that go on the wire are all testable
//! without a board.

use std::io;

use leviculum_core::constants::{IDENTITY_KEY_SIZE, TRUNCATED_HASHBYTES};
use leviculum_core::envelope::{
    TelemetryTargetWire, TELEMETRY_PROFILE_OFF, TELEMETRY_PROFILE_STATION,
    TELEMETRY_PROFILE_TRACKER, TYPE_TELEMETRY_TARGET,
};

use crate::envelope::{self, SessionReply};
use crate::sys::Fd;
use crate::ui::Ui;

/// The one question the flash flow asks about telemetry. `[y/N]` rather
/// than `[Y/n]` is the defaults-first decision made visible: Enter leaves
/// the board exactly as it was.
pub const ASK_TELEMETRY: &str = "\nSend telemetry? [y/N]";

/// And the one input a "yes" needs.
pub const ASK_ADDRESS: &str = "  LXMF address (32 hex characters)";

/// The wire form that switches telemetry off.
///
/// The configured target is the on-switch, so "off" is the absence of one:
/// profile [`TELEMETRY_PROFILE_OFF`], a zeroed hash, no key. Sending the
/// old target back to say "forget it" would put a destination on the wire
/// for no reason.
pub fn clear_target() -> TelemetryTargetWire {
    TelemetryTargetWire {
        profile: TELEMETRY_PROFILE_OFF,
        dest_hash: [0u8; TRUNCATED_HASHBYTES],
        public_key: None,
    }
}

/// What the `--telemetry*` flags said, before any board is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryPlan {
    /// Ask. The profile is whatever `--telemetry-profile` already named, so
    /// the prompt still asks for the address and nothing else.
    Ask { profile: u8 },
    /// Decided on the command line; do not ask.
    Fixed(TelemetryTargetWire),
    /// `--no-telemetry`: send the clear frame. Distinct from answering "no"
    /// at the prompt, which sends nothing at all — a board that has a target
    /// stored keeps it unless somebody says to clear it.
    Clear,
}

impl Default for TelemetryPlan {
    fn default() -> Self {
        Self::Ask {
            profile: TELEMETRY_PROFILE_STATION,
        }
    }
}

/// Turn the plan into the target to send. `None` is "send nothing".
///
/// **Non-interactive runs never block here**, and not by detecting a
/// terminal: [`Ui::ask`] answers `None` both for `--yes`
/// ([`crate::ui::Assumed`], which takes every stated default rather than
/// waiting on a prompt nobody will read) and for a closed or piped stdin
/// ([`crate::ui::Console`] reading EOF). Every loop below treats `None` as
/// the default, which is also what keeps the re-prompt finite. This is the
/// same mechanism the radio prompt uses; there is no second rule for
/// telemetry.
pub fn resolve(ui: &mut dyn Ui, plan: &TelemetryPlan) -> io::Result<Option<TelemetryTargetWire>> {
    let profile = match plan {
        TelemetryPlan::Clear => return Ok(Some(clear_target())),
        TelemetryPlan::Fixed(target) => return Ok(Some(*target)),
        TelemetryPlan::Ask { profile } => *profile,
    };

    let answer = ui.ask(ASK_TELEMETRY)?.unwrap_or_default();
    if !matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "j" | "ja"
    ) {
        return Ok(None);
    }

    loop {
        let Some(typed) = ui.ask(ASK_ADDRESS)? else {
            // Enter, end of input, or a run that does not ask at all:
            // telemetry stays off. Saying so beats leaving the operator to
            // infer it from a transcript that just stops.
            ui.say("  no address given, so telemetry stays off");
            return Ok(None);
        };
        match parse_address(&typed) {
            Ok(dest_hash) => {
                return Ok(Some(TelemetryTargetWire {
                    profile,
                    dest_hash,
                    // Hash-only: the node resolves the key over the air.
                    // The prompt asks for one input, and this is why.
                    public_key: None,
                }));
            }
            Err(err) => ui.say(&format!("  {err}")),
        }
    }
}

/// Send the target on an already-opened transport port.
///
/// Takes the fd rather than a path on purpose: the open lives with the
/// caller ([`crate::flow`]'s `open_transport`), which proves the port still
/// belongs to the board it was resolved for before a frame goes out — and
/// the scripted-pty tests drive the same code the flash flow runs. The
/// capability probe is not optional: the frame is 23 or 87 bytes and
/// firmware that does not know the type would read it as a Reticulum
/// packet.
pub fn send_configured(fd: &Fd, target: &TelemetryTargetWire) -> io::Result<SessionReply> {
    envelope::probed(fd, TYPE_TELEMETRY_TARGET, |fd| {
        envelope::send_telemetry_target(fd, target)
    })
}

/// Send the target and, when the board took it, read back whether it has a
/// position source at all.
///
/// The read-back is the second clause of the send condition
/// (`docs/src/concepts/telemetry.md`): a stored target on a board with no
/// fixed position and no receiver produces no reports, ever, and an ack
/// that says nothing about that is an ack the operator will misread. It is
/// asked on the same open port as the target frame — a second open would be
/// a second connection to a board mid-configuration.
///
/// `None` for the sources is "the board did not say": firmware without the
/// query, or a reporter-less binary refusing by name. The caller then says
/// nothing about position sources, because inventing the answer here would
/// be the one failure worse than the silence.
pub fn send_configured_and_read_sources(
    fd: &Fd,
    target: &TelemetryTargetWire,
) -> io::Result<(SessionReply, Option<envelope::PositionSources>)> {
    let reply = send_configured(fd, target)?;
    if !reply.took_it() || target.profile == TELEMETRY_PROFILE_OFF {
        // A refusal has its own words already, and a cleared target sends
        // nothing whatever the board can measure.
        return Ok((reply, None));
    }
    Ok((reply, envelope::query_position_sources(fd)?.ok()))
}

/// The profile a `--telemetry-profile` value names.
pub fn parse_profile(name: &str) -> Result<u8, String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "tracker" => Ok(TELEMETRY_PROFILE_TRACKER),
        "station" => Ok(TELEMETRY_PROFILE_STATION),
        other => Err(format!(
            "{other:?} is not a telemetry profile; it is tracker (movement-driven) or station \
             (slow stationary heartbeat)"
        )),
    }
}

/// The name a profile id goes by, for the transcript.
pub fn profile_name(profile: u8) -> &'static str {
    match profile {
        TELEMETRY_PROFILE_OFF => "off",
        TELEMETRY_PROFILE_TRACKER => "tracker",
        TELEMETRY_PROFILE_STATION => "station",
        _ => "unknown",
    }
}

/// One line of `key=value` describing what will be sent, for the transcript
/// and for grep.
pub fn describe(target: &TelemetryTargetWire) -> String {
    if target.profile == TELEMETRY_PROFILE_OFF {
        return "telemetry off — the stored target is cleared".to_string();
    }
    format!(
        "target={} profile={} key={}",
        hex(&target.dest_hash),
        profile_name(target.profile),
        match target.public_key {
            Some(_) => "given",
            None => "resolved over the air",
        }
    )
}

/// An LXMF address as the user typed it: 32 hex digits, in whatever
/// grouping and case they came from.
pub fn parse_address(text: &str) -> Result<[u8; TRUNCATED_HASHBYTES], String> {
    parse_hex(text, "an LXMF address")
}

/// A destination's 64-byte public key: 128 hex digits.
pub fn parse_key(text: &str) -> Result<[u8; IDENTITY_KEY_SIZE], String> {
    parse_hex(text, "a telemetry key")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whitespace and colons are how humans group a hash when they read one off
/// a screen; neither carries meaning, so neither is an error.
fn tidy(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect()
}

/// Every refusal names the shape that was expected, because a user who
/// mistyped an address cannot guess what "invalid" meant.
fn parse_hex<const N: usize>(text: &str, what: &str) -> Result<[u8; N], String> {
    let tidied = tidy(text);
    let want = N * 2;
    // Hex-ness first: it also guarantees the string is ASCII, which is what
    // makes the two-character slicing below safe.
    if !tidied.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "{what} is {want} hex characters ({N} bytes); {:?} is not hexadecimal",
            text.trim()
        ));
    }
    if tidied.len() != want {
        return Err(format!(
            "{what} is {want} hex characters ({N} bytes, spaces and colons ignored); {:?} has {}",
            text.trim(),
            tidied.len()
        ));
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&tidied[i * 2..i * 2 + 2], 16)
            .map_err(|err| format!("{what}: {err}"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::testing::{
        envelope_firmware_stub, old_firmware_stub, positionless_firmware_stub,
        pre_236_firmware_stub, reporterless_firmware_stub, seen, telemetry_frame,
    };
    use crate::sys::testpty::Pty;
    use crate::ui::testing::Fake;
    use crate::ui::Assumed;
    use leviculum_core::envelope::{
        encode_telemetry_clear, encode_telemetry_target, ENVELOPE_HEADER_LEN,
    };

    /// The address used throughout, in the shape a user reads off a screen.
    const ADDRESS: &str = "a7b2c3d4e5f60718293a4b5c6d7e8f90";
    const ADDRESS_BYTES: [u8; 16] = [
        0xa7, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e, 0x8f,
        0x90,
    ];

    fn station(dest_hash: [u8; 16]) -> TelemetryTargetWire {
        TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_STATION,
            dest_hash,
            public_key: None,
        }
    }

    // -----------------------------------------------------------------
    // What a human may type
    // -----------------------------------------------------------------

    #[test]
    fn an_address_is_taken_however_a_human_spaced_or_cased_it() {
        for typed in [
            ADDRESS,
            "A7B2C3D4E5F60718293A4B5C6D7E8F90",
            "a7b2:c3d4:e5f6:0718:293a:4b5c:6d7e:8f90",
            "  a7b2 c3d4 e5f6 0718 293a 4b5c 6d7e 8f90  ",
            "a7b2C3d4:E5f60718 293a4b5c6d7e8f90",
        ] {
            assert_eq!(parse_address(typed).unwrap(), ADDRESS_BYTES, "{typed}");
        }
    }

    #[test]
    fn a_malformed_address_names_the_shape_that_was_expected() {
        for typed in ["a7b2", "", &format!("{ADDRESS}00"), "not an address at all"] {
            let err = parse_address(typed).unwrap_err();
            assert!(err.contains("32 hex characters"), "{typed}: {err}");
            assert!(err.contains("16 bytes"), "{typed}: {err}");
        }
        // Non-hex is named as such rather than as a length problem, and a
        // non-ASCII character must not panic the two-digit slicing.
        assert!(parse_address("zzzz").unwrap_err().contains("hexadecimal"));
        assert!(parse_address("ä7b2c3d4e5f60718293a4b5c6d7e8f90")
            .unwrap_err()
            .contains("hexadecimal"));
    }

    #[test]
    fn a_key_is_128_hex_characters_and_says_so_when_it_is_not() {
        let key = "5e".repeat(64);
        assert_eq!(parse_key(&key).unwrap(), [0x5Eu8; 64]);
        let err = parse_key(ADDRESS).unwrap_err();
        assert!(err.contains("128 hex characters"), "{err}");
        assert!(err.contains("64 bytes"), "{err}");
    }

    #[test]
    fn the_profiles_are_tracker_and_station_and_nothing_else() {
        assert_eq!(parse_profile("tracker").unwrap(), TELEMETRY_PROFILE_TRACKER);
        assert_eq!(
            parse_profile(" Station ").unwrap(),
            TELEMETRY_PROFILE_STATION
        );
        let err = parse_profile("beacon").unwrap_err();
        assert!(err.contains("tracker"), "{err}");
        assert!(err.contains("station"), "{err}");
        assert_eq!(profile_name(TELEMETRY_PROFILE_OFF), "off");
    }

    // -----------------------------------------------------------------
    // The prompt
    // -----------------------------------------------------------------

    #[test]
    fn the_prompt_defaults_to_no_so_a_user_who_configures_nothing_gets_nothing() {
        // Defaults first: Enter at the question leaves the board alone, and
        // "leaves it alone" means no frame at all, not a clear frame.
        let mut ui = Fake::agreeing();
        assert_eq!(resolve(&mut ui, &TelemetryPlan::default()).unwrap(), None);
        let said = ui.transcript();
        assert!(said.contains("Send telemetry? [y/N]"), "{said}");
        assert!(!said.contains("LXMF address"), "{said}");
    }

    #[test]
    fn only_an_explicit_yes_is_a_yes() {
        // Anything that is not a yes leaves the board alone, and does so
        // without asking for an address — a run that gets as far as the
        // second question has already decided the first one wrongly.
        for typed in ["n", "N", "no", "nonsense", "yeah"] {
            let mut ui = Fake::typing(&[typed]);
            assert_eq!(
                resolve(&mut ui, &TelemetryPlan::default()).unwrap(),
                None,
                "{typed}"
            );
            assert!(
                !ui.transcript().contains("LXMF address"),
                "{typed} was taken as a yes"
            );
        }
    }

    #[test]
    fn yes_asks_for_the_address_and_for_nothing_else() {
        // The 2026-08-22 decision, asserted: exactly ONE input on yes.
        let mut ui = Fake::typing(&["y", ADDRESS]);
        assert_eq!(
            resolve(&mut ui, &TelemetryPlan::default()).unwrap(),
            Some(station(ADDRESS_BYTES))
        );
        assert_eq!(
            ui.said,
            vec![ASK_TELEMETRY.to_string(), ASK_ADDRESS.to_string()],
            "yes must cost the user one question and one answer"
        );
    }

    #[test]
    fn a_malformed_address_is_asked_again_rather_than_sent() {
        let mut ui = Fake::typing(&["yes", "a7b2", "zzzz", ADDRESS]);
        assert_eq!(
            resolve(&mut ui, &TelemetryPlan::default()).unwrap(),
            Some(station(ADDRESS_BYTES))
        );
        let said = ui.transcript();
        assert!(said.contains("32 hex characters"), "{said}");
        assert!(said.contains("not hexadecimal"), "{said}");
        // Three asks for the address: two refused, one taken.
        assert_eq!(said.matches(ASK_ADDRESS).count(), 3, "{said}");
    }

    #[test]
    fn the_profile_a_flag_named_is_the_one_the_prompted_address_gets() {
        let mut ui = Fake::typing(&["y", ADDRESS]);
        let plan = TelemetryPlan::Ask {
            profile: TELEMETRY_PROFILE_TRACKER,
        };
        assert_eq!(
            resolve(&mut ui, &plan).unwrap().unwrap().profile,
            TELEMETRY_PROFILE_TRACKER
        );
    }

    #[test]
    fn a_run_with_nobody_to_ask_never_blocks_on_the_prompt() {
        // --yes: Assumed::ask takes the stated default, which is "no".
        let mut ui = Assumed::new(true);
        assert_eq!(resolve(&mut ui, &TelemetryPlan::default()).unwrap(), None);

        // A piped stdin that runs out mid-dialogue is the same story: the
        // address prompt reads EOF and the run ends off rather than looping.
        let mut ui = Fake::typing(&["y"]);
        assert_eq!(resolve(&mut ui, &TelemetryPlan::default()).unwrap(), None);
        assert!(
            ui.transcript().contains("telemetry stays off"),
            "{}",
            ui.transcript()
        );
    }

    #[test]
    fn the_flags_decide_without_asking_anything() {
        let target = station(ADDRESS_BYTES);
        let mut ui = Fake::refusing();
        assert_eq!(
            resolve(&mut ui, &TelemetryPlan::Fixed(target)).unwrap(),
            Some(target)
        );
        assert_eq!(
            resolve(&mut ui, &TelemetryPlan::Clear).unwrap(),
            Some(clear_target())
        );
        assert!(ui.transcript().is_empty(), "{}", ui.transcript());
    }

    #[test]
    fn the_transcript_says_whether_the_key_travels_or_gets_resolved() {
        let hash_only = describe(&station(ADDRESS_BYTES));
        assert!(
            hash_only.contains(&format!("target={ADDRESS}")),
            "{hash_only}"
        );
        assert!(hash_only.contains("profile=station"), "{hash_only}");
        assert!(hash_only.contains("resolved over the air"), "{hash_only}");

        let with_key = describe(&TelemetryTargetWire {
            public_key: Some([0x5E; 64]),
            ..station(ADDRESS_BYTES)
        });
        assert!(with_key.contains("key=given"), "{with_key}");
        assert!(describe(&clear_target()).contains("telemetry off"));
    }

    // -----------------------------------------------------------------
    // The bytes that reach the board
    // -----------------------------------------------------------------

    #[test]
    fn a_hash_only_answer_reaches_the_board_as_an_18_byte_payload() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let mut ui = Fake::typing(&["y", ADDRESS]);
        let target = resolve(&mut ui, &TelemetryPlan::default())
            .unwrap()
            .unwrap();
        assert_eq!(send_configured(&fd, &target).unwrap(), SessionReply::Acked);

        let decoded = telemetry_frame(&seen).expect("no telemetry frame reached the stub");
        assert_eq!(decoded, station(ADDRESS_BYTES));
        assert_eq!(decoded.public_key, None);
        // The frame the firmware decoded is the frame the encoder writes,
        // key-present flag explicitly absent: 1 + 16 + 1 payload bytes.
        assert_eq!(
            encode_telemetry_target(&decoded).len(),
            ENVELOPE_HEADER_LEN + 18
        );
    }

    #[test]
    fn a_key_given_on_the_command_line_reaches_the_board_with_the_flag_set() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let target = TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_TRACKER,
            dest_hash: ADDRESS_BYTES,
            public_key: Some(parse_key(&"5e".repeat(64)).unwrap()),
        };
        let mut ui = Fake::refusing();
        let resolved = resolve(&mut ui, &TelemetryPlan::Fixed(target))
            .unwrap()
            .unwrap();
        assert_eq!(
            send_configured(&fd, &resolved).unwrap(),
            SessionReply::Acked
        );

        let decoded = telemetry_frame(&seen).expect("no telemetry frame reached the stub");
        assert_eq!(decoded, target);
        assert_eq!(decoded.public_key, Some([0x5E; 64]));
        assert_eq!(
            encode_telemetry_target(&decoded).len(),
            ENVELOPE_HEADER_LEN + 82
        );
    }

    #[test]
    fn no_telemetry_puts_the_off_profile_on_the_wire() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let mut ui = Fake::refusing();
        let target = resolve(&mut ui, &TelemetryPlan::Clear).unwrap().unwrap();
        assert_eq!(send_configured(&fd, &target).unwrap(), SessionReply::Acked);

        let decoded = telemetry_frame(&seen).expect("no clear frame reached the stub");
        assert_eq!(decoded.profile, TELEMETRY_PROFILE_OFF);
        assert_eq!(decoded.dest_hash, [0u8; 16]);
        assert_eq!(decoded.public_key, None);
        // One spelling of "off": what this module builds and what the core
        // encoder calls a clear frame have to be the same bytes.
        assert_eq!(encode_telemetry_target(&decoded), encode_telemetry_clear());
    }

    #[test]
    fn an_old_board_is_reported_as_such_rather_than_written_to_blind() {
        let pty = Pty::open();
        let seen = seen();
        old_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert_eq!(
            send_configured(&fd, &station(ADDRESS_BYTES)).unwrap(),
            SessionReply::ProbeSilent
        );
        assert_eq!(telemetry_frame(&seen), None);
    }

    #[test]
    fn a_reporterless_board_refuses_instead_of_acking_what_nothing_honors() {
        // The T114's pre-wiring failure shape, proven impossible on the
        // host: the frame reaches the board, and the answer is a named
        // refusal — never the "telemetry on" ack of 06:17.
        let pty = Pty::open();
        let seen = seen();
        reporterless_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        let reply = send_configured(&fd, &station(ADDRESS_BYTES)).unwrap();
        assert_eq!(
            reply,
            SessionReply::Refused(leviculum_core::envelope::REFUSE_UNSUPPORTED)
        );
        assert!(!reply.took_it(), "took_it drives the non-zero exit");
        // The frame did go on the wire — the refusal is the firmware's
        // decision, not the probe's.
        assert!(telemetry_frame(&seen).is_some());
    }

    #[test]
    fn a_board_without_the_telemetry_consumer_is_named_not_guessed_at() {
        let pty = Pty::open();
        let seen = seen();
        pre_236_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert_eq!(
            send_configured(&fd, &station(ADDRESS_BYTES)).unwrap(),
            SessionReply::NotAccepted
        );
        assert_eq!(telemetry_frame(&seen), None);
    }
    // -----------------------------------------------------------------
    // The position source the target depends on (Lew, 2026-08-30)
    // -----------------------------------------------------------------

    /// A board with a receiver reports its source alongside the ack, on
    /// the same open port. This is the control for the test after it.
    #[test]
    fn a_board_with_a_receiver_reports_that_it_has_a_position_source() {
        let pty = Pty::open();
        envelope_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let (reply, sources) =
            send_configured_and_read_sources(&fd, &station(ADDRESS_BYTES)).unwrap();
        assert_eq!(reply, SessionReply::Acked);
        let sources = sources.expect("the board answered the query");
        assert!(sources.gnss);
        assert!(!sources.fixed);
        assert!(sources.any());
    }

    /// A board with neither a receiver nor a pin takes the target — valid
    /// configuration — and says it has no position source, which is what
    /// earns the consequence sentence.
    #[test]
    fn a_board_with_neither_source_acks_the_target_and_says_it_has_none() {
        let pty = Pty::open();
        let seen = seen();
        positionless_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let (reply, sources) =
            send_configured_and_read_sources(&fd, &station(ADDRESS_BYTES)).unwrap();
        assert_eq!(
            reply,
            SessionReply::Acked,
            "the target is stored, not refused"
        );
        assert!(telemetry_frame(&seen).is_some());
        assert!(!sources.expect("the board answered").any());
    }

    /// Setting a pin is the runtime path out of it: the same board, asked
    /// again after a fixed position reaches it, now has a source.
    #[test]
    fn setting_a_fixed_position_gives_a_sourceless_board_a_source() {
        let pty = Pty::open();
        positionless_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let (_, before) = send_configured_and_read_sources(&fd, &station(ADDRESS_BYTES)).unwrap();
        assert!(!before.expect("the board answered").any());

        let position = leviculum_core::envelope::FixedPositionWire {
            latitude_e6: 53_551_086,
            longitude_e6: 9_993_682,
            altitude_e2: None,
        };
        assert_eq!(
            crate::envelope::send_fixed_position(&fd, Some(&position)).unwrap(),
            crate::envelope::ControlOutcome::Acked
        );

        let after = crate::envelope::query_position_sources(&fd)
            .unwrap()
            .expect("the board answered");
        assert!(after.fixed, "the pin is a position source");
        assert!(after.any());
    }

    /// A cleared target is not asked about position sources at all: it
    /// sends nothing whatever the board can measure, so a warning about
    /// pins would be noise.
    #[test]
    fn clearing_the_target_asks_nothing_about_position_sources() {
        let pty = Pty::open();
        positionless_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let (reply, sources) = send_configured_and_read_sources(&fd, &clear_target()).unwrap();
        assert_eq!(reply, SessionReply::Acked);
        assert_eq!(sources, None);
    }

    /// A binary with no reporter refuses the query by name rather than
    /// answering "no sources", which would read as a promise that setting
    /// one would help. The caller gets `None` and says nothing.
    #[test]
    fn a_reporterless_board_refuses_the_query_instead_of_answering_zero() {
        let pty = Pty::open();
        reporterless_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert_eq!(
            crate::envelope::query_position_sources(&fd).unwrap(),
            Err(crate::envelope::ControlOutcome::Refused {
                reason: leviculum_core::envelope::REFUSE_UNSUPPORTED
            })
        );
    }
}
