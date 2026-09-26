//! `--set-media` — which carriers a node meshes over.
//!
//! The board-facing half is two control-envelope frames
//! (`TYPE_MEDIA_PROFILE` to set, `TYPE_MEDIA_QUERY` to read); everything
//! in this module is the human-facing half: parse what a person types for
//! a profile, merge it with what the board already has, and say what
//! happened. Nothing here opens a port — the open lives with
//! [`crate::flow`], which proves it landed on the intended board — so the
//! parsing and the wire bytes are testable without a board.
//!
//! # Why the flag exists
//!
//! An LNode meshes over LoRa and BLE at once by default, so a packet
//! delivered over the other medium masks a loss on the medium under
//! test: every single-medium measurement against a dual-carrier node is
//! falsifiable. `--set-media` is the declaration that makes such a
//! measurement honest, and the board persists it so the reset that ends a
//! run does not quietly put the node back on both.
//!
//! # The parse table
//!
//! `key=value` pairs, comma- or whitespace-separated, in any order:
//!
//! | typed                  | means                                    |
//! |------------------------|------------------------------------------|
//! | `lora=on,ble=off`      | LoRa only                                |
//! | `lora=off,ble=on`      | BLE only                                 |
//! | `lora=on ble=on`       | both (whitespace separates too)          |
//! | `LORA=On,BLE=OFF`      | same as the first (keys and values are case-insensitive) |
//! | `ble=off`              | BLE off, LoRa left at whatever the board has |
//! | `lora=off,ble=off`     | neither — the node meshes over nothing but its USB serial |
//!
//! Accepted values: `on`/`off`, `true`/`false`, `yes`/`no`, `1`/`0`. A key
//! given twice, an unknown key, or an unknown value is refused by name
//! rather than guessed at — a misparse here silently produces a
//! measurement of the wrong thing, which is worse than no measurement.
//!
//! A pair that is left out keeps the board's own current setting, read
//! back over the query first. That is the [`crate::flow::set_tx_power`]
//! read-modify-write contract on a second frame: a host that cannot read
//! the other carrier must not substitute its own default for it.

use std::io;

use leviculum_core::envelope::{MediaProfileWire, TYPE_MEDIA_PROFILE, TYPE_MEDIA_QUERY};

use crate::envelope::{self, MediaState, SessionReply};
use crate::sys::Fd;

/// What `--set-media` was given: a value per carrier, absent meaning
/// "leave this one where the board has it".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaSpec {
    pub lora: Option<bool>,
    pub ble: Option<bool>,
}

impl MediaSpec {
    /// This spec applied to what the board reported as configured. The
    /// carriers the spec is silent about keep the board's value, which is
    /// why this takes the board's profile rather than a default.
    pub fn onto(self, current: MediaProfileWire) -> MediaProfileWire {
        MediaProfileWire {
            lora_enabled: self.lora.unwrap_or(current.lora_enabled),
            ble_enabled: self.ble.unwrap_or(current.ble_enabled),
        }
    }
}

/// Parse what `--set-media` was given. See the module docs for the table.
pub fn parse_media(text: &str) -> Result<MediaSpec, String> {
    const SHAPE: &str = "a media profile is lora=on|off and/or ble=on|off (comma or space \
                         separated), like \"lora=on,ble=off\"";
    let tokens: Vec<&str> = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return Err(format!("{SHAPE}; {:?} names no carrier", text.trim()));
    }
    let mut spec = MediaSpec::default();
    for token in tokens {
        let (key, value) = token
            .split_once('=')
            .ok_or_else(|| format!("{token:?} is not a key=value pair; {SHAPE}"))?;
        let enabled = parse_on_off(value, token)?;
        let slot = match key.to_ascii_lowercase().as_str() {
            "lora" => &mut spec.lora,
            "ble" => &mut spec.ble,
            other => {
                return Err(format!(
                    "{other:?} is not a carrier this firmware has; {SHAPE}"
                ))
            }
        };
        if slot.is_some() {
            // Two values for one carrier: which one wins would be an
            // arbitrary choice, and the wrong one silently measures the
            // wrong medium.
            return Err(format!(
                "{:?} is given twice in {:?}",
                key.to_ascii_lowercase(),
                text.trim()
            ));
        }
        *slot = Some(enabled);
    }
    Ok(spec)
}

/// One carrier's value. Spelled out rather than "anything but off is on":
/// a typo has to be named, not rounded to the dangerous direction.
fn parse_on_off(value: &str, token: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        other => Err(format!(
            "{other:?} in {token:?} is not on or off (also accepted: true/false, yes/no, 1/0)"
        )),
    }
}

/// One `key=value` line describing a profile, for the transcript and for
/// grep. Deliberately the same `lora=on ble=off` shape the board's own
/// `[MEDIA]` banner uses, so an operator comparing the two is comparing
/// identical text.
pub fn describe(profile: MediaProfileWire) -> String {
    format!(
        "lora={} ble={}",
        on_off(profile.lora_enabled),
        on_off(profile.ble_enabled)
    )
}

fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

/// Read the board's media profile on an already-opened transport port.
///
/// Behind the capability probe like every other command, so firmware
/// without the frame is reported as such rather than written to blind.
pub fn query(fd: &Fd) -> io::Result<Result<MediaState, SessionReply>> {
    let Some(caps) = envelope::probe_capabilities(fd)? else {
        return Ok(Err(SessionReply::ProbeSilent));
    };
    if !caps.accepts(TYPE_MEDIA_QUERY) {
        return Ok(Err(SessionReply::NotAccepted));
    }
    Ok(envelope::query_media_profile(fd)?.map_err(SessionReply::from))
}

/// Set the media profile on an already-opened transport port.
///
/// Returns the board's report — what it is running and what it is
/// configured for — because those can differ, and the difference is the
/// board saying "that carrier did not come up this boot". The caller must
/// pass a complete profile: [`MediaSpec::onto`] against what [`query`]
/// read is how a one-sided `--set-media ble=off` becomes one.
pub fn send_configured(
    fd: &Fd,
    profile: MediaProfileWire,
) -> io::Result<Result<MediaState, SessionReply>> {
    let Some(caps) = envelope::probe_capabilities(fd)? else {
        return Ok(Err(SessionReply::ProbeSilent));
    };
    if !caps.accepts(TYPE_MEDIA_PROFILE) {
        return Ok(Err(SessionReply::NotAccepted));
    }
    Ok(envelope::send_media_profile(fd, &profile)?.map_err(SessionReply::from))
}

/// What to tell an operator about a report that came back, beyond the
/// profile itself: which carriers are waiting for a reboot.
///
/// Empty when nothing is, so a caller can append it unconditionally.
pub fn reboot_note(state: MediaState) -> String {
    if !state.needs_reboot() {
        return String::new();
    }
    let mut waiting: Vec<&str> = Vec::new();
    if state.configured.lora_enabled && !state.running.lora_enabled {
        waiting.push("lora");
    }
    if state.configured.ble_enabled && !state.running.ble_enabled {
        waiting.push("ble");
    }
    if waiting.is_empty() {
        // running has a carrier configured says is off. The board's own
        // rule cannot produce this, so say what was seen rather than
        // inventing a reading of it.
        return format!(
            " The board reports it is running {} while configured for {}, which its own rules \
             should not allow; treat this board as unconfigured and re-read it.",
            describe(state.running),
            describe(state.configured)
        );
    }
    format!(
        " {} is configured on but not running: that carrier's driver was never started this \
         boot, so it cannot be started now. Reset the board and check its [MEDIA] line.",
        waiting.join(" and ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::testing::{
        envelope_firmware_stub, envelope_firmware_stub_with_media, media_frame, media_state,
        media_state_booted, medialess_firmware_stub, old_firmware_stub, pre_236_firmware_stub,
        seen,
    };
    use crate::sys::testpty::Pty;
    use leviculum_core::envelope::REFUSE_UNSUPPORTED;

    const LORA_ONLY: MediaProfileWire = MediaProfileWire {
        lora_enabled: true,
        ble_enabled: false,
    };
    const BLE_ONLY: MediaProfileWire = MediaProfileWire {
        lora_enabled: false,
        ble_enabled: true,
    };

    // -----------------------------------------------------------------
    // What a human may type
    // -----------------------------------------------------------------

    #[test]
    fn the_accepted_spellings_all_parse_to_the_same_profile() {
        // The parse table from the module docs, asserted rather than
        // described.
        for typed in [
            "lora=on,ble=off",
            "lora=on, ble=off",
            "lora=on ble=off",
            "ble=off,lora=on",
            "LORA=On,BLE=OFF",
            "lora=true,ble=false",
            "lora=yes,ble=no",
            "lora=1,ble=0",
        ] {
            assert_eq!(
                parse_media(typed).unwrap(),
                MediaSpec {
                    lora: Some(true),
                    ble: Some(false)
                },
                "{typed}"
            );
        }
    }

    #[test]
    fn a_one_sided_spec_leaves_the_other_carrier_absent() {
        // Absent is not "off": it means "keep what the board has", and
        // conflating the two would take a carrier down that nobody asked
        // about.
        assert_eq!(
            parse_media("ble=off").unwrap(),
            MediaSpec {
                lora: None,
                ble: Some(false)
            }
        );
        assert_eq!(
            parse_media("ble=off").unwrap().onto(MediaProfileWire::BOTH),
            LORA_ONLY
        );
        assert!(!parse_media("ble=off").unwrap().onto(BLE_ONLY).lora_enabled);
    }

    #[test]
    fn both_carriers_off_is_a_profile_a_person_may_ask_for() {
        // A node on nothing but its USB serial is a legitimate state — it
        // is what a control node in a two-medium comparison runs as.
        assert_eq!(
            parse_media("lora=off,ble=off")
                .unwrap()
                .onto(MediaProfileWire::BOTH),
            MediaProfileWire {
                lora_enabled: false,
                ble_enabled: false
            }
        );
    }

    #[test]
    fn garbage_is_refused_with_the_shape_that_was_expected() {
        for typed in [
            "",
            "lora",             // no value
            "lora=maybe",       // not a boolean
            "wifi=on",          // not a carrier
            "lora=on,lora=off", // the same carrier twice
            "lora=on,LORA=off", // ... in a different case
            "=on",              // no key
            "lora on",          // no '='
        ] {
            assert!(parse_media(typed).is_err(), "{typed:?} was accepted");
        }
        // The refusals name what was wrong, not just "invalid".
        assert!(parse_media("wifi=on").unwrap_err().contains("carrier"));
        assert!(parse_media("lora=maybe").unwrap_err().contains("on or off"));
        assert!(parse_media("lora=on,lora=off")
            .unwrap_err()
            .contains("twice"));
        assert!(parse_media("").unwrap_err().contains("no carrier"));
    }

    #[test]
    fn the_transcript_prints_the_same_shape_the_board_logs() {
        // An operator comparing lnflash's line with the board's [MEDIA]
        // banner must be comparing identical text, not two dialects.
        assert_eq!(describe(LORA_ONLY), "lora=on ble=off");
        assert_eq!(describe(MediaProfileWire::BOTH), "lora=on ble=on");
    }

    // -----------------------------------------------------------------
    // The bytes that reach the board
    // -----------------------------------------------------------------

    #[test]
    fn a_profile_reaches_the_board_and_comes_back_as_a_report() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let state = send_configured(&fd, LORA_ONLY).unwrap().unwrap();
        // Switching BLE off takes effect at once: running and configured
        // agree, and nothing waits for a reboot.
        assert_eq!(state.configured, LORA_ONLY);
        assert_eq!(state.running, LORA_ONLY);
        assert!(!state.needs_reboot());
        assert_eq!(reboot_note(state), "");
        assert_eq!(media_frame(&seen), Some(LORA_ONLY));
    }

    #[test]
    fn starting_a_carrier_that_did_not_come_up_this_boot_is_reported_as_pending() {
        // The honesty this whole report exists for. A board flashed with
        // ble=off and reset never spawned the BLE driver, and the board
        // cannot start one it does not have — so `ble=on` is answered
        // with "configured, not running" instead of an ack that would
        // have a measurement believe the carrier is back.
        let pty = Pty::open();
        envelope_firmware_stub_with_media(&pty, seen(), media_state_booted(LORA_ONLY));
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let state = send_configured(&fd, MediaProfileWire::BOTH)
            .unwrap()
            .unwrap();
        assert_eq!(state.configured, MediaProfileWire::BOTH);
        assert_eq!(state.running, LORA_ONLY);
        assert!(state.needs_reboot());
        // The note names the carrier that is waiting and what to do
        // about it: an operator who reads only this line must not go on
        // measuring a BLE link that is not there.
        let note = reboot_note(state);
        assert!(note.contains("ble"), "{note}");
        assert!(note.contains("Reset the board"), "{note}");
    }

    #[test]
    fn switching_a_carrier_off_and_back_on_within_one_boot_needs_no_reset() {
        // The other half of the same rule, and the reason it is stated as
        // "did not come up this boot" rather than "switched on": a
        // carrier whose driver IS running was only being ignored, and
        // un-ignoring it is immediate.
        let pty = Pty::open();
        envelope_firmware_stub_with_media(&pty, seen(), media_state());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let off = send_configured(&fd, LORA_ONLY).unwrap().unwrap();
        assert_eq!(off.running, LORA_ONLY);
        assert!(!off.needs_reboot());

        let on = send_configured(&fd, MediaProfileWire::BOTH)
            .unwrap()
            .unwrap();
        assert_eq!(on.running, MediaProfileWire::BOTH);
        assert!(!on.needs_reboot());
        assert_eq!(reboot_note(on), "");
    }

    #[test]
    fn the_query_reads_the_board_back_without_changing_it() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let state = query(&fd).unwrap().unwrap();
        assert_eq!(state.configured, MediaProfileWire::BOTH);
        assert_eq!(state.running, MediaProfileWire::BOTH);
        // Read-only: no set frame may appear on the wire for a query.
        assert_eq!(media_frame(&seen), None);
    }

    #[test]
    fn a_one_sided_set_keeps_the_carrier_it_did_not_name() {
        // The read-modify-write contract, on the same board across two
        // conversations: the board starts on BLE only, `--set-media
        // lora=on` must not also reset BLE to a host default.
        let pty = Pty::open();
        let seen = seen();
        let media = media_state();
        envelope_firmware_stub_with_media(&pty, seen.clone(), media);
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        // Put the board on BLE only first, then read-modify-write it.
        send_configured(&fd, BLE_ONLY).unwrap().unwrap();
        let current = query(&fd).unwrap().unwrap().configured;
        assert_eq!(current, BLE_ONLY);

        let merged = parse_media("lora=on").unwrap().onto(current);
        assert_eq!(merged, MediaProfileWire::BOTH, "ble must not be reset");
        send_configured(&fd, merged).unwrap().unwrap();
        assert_eq!(media_frame(&seen), Some(MediaProfileWire::BOTH));
    }

    #[test]
    fn a_binary_without_the_media_gate_refuses_instead_of_acking() {
        // The fc60b95 capability gate on the media frames. An ack here
        // would be read by a measurement run as "this node is
        // single-medium now" — the one lie this feature must not tell.
        let pty = Pty::open();
        let seen = seen();
        medialess_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let reply = send_configured(&fd, LORA_ONLY).unwrap().unwrap_err();
        assert_eq!(reply, SessionReply::Refused(REFUSE_UNSUPPORTED));
        assert!(!reply.took_it(), "took_it drives the non-zero exit");
        // The frame did reach the board — this is a refusal, not silence.
        assert_eq!(media_frame(&seen), Some(LORA_ONLY));

        assert_eq!(
            query(&fd).unwrap().unwrap_err(),
            SessionReply::Refused(REFUSE_UNSUPPORTED)
        );
    }

    #[test]
    fn an_old_board_is_reported_as_such_rather_than_written_to_blind() {
        let pty = Pty::open();
        let seen = seen();
        old_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            send_configured(&fd, LORA_ONLY).unwrap().unwrap_err(),
            SessionReply::ProbeSilent
        );
        assert_eq!(query(&fd).unwrap().unwrap_err(), SessionReply::ProbeSilent);
        assert_eq!(media_frame(&seen), None, "nothing may go on the wire");
    }

    #[test]
    fn a_board_without_the_frames_is_named_not_guessed_at() {
        let pty = Pty::open();
        let seen = seen();
        pre_236_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            send_configured(&fd, LORA_ONLY).unwrap().unwrap_err(),
            SessionReply::NotAccepted
        );
        assert_eq!(query(&fd).unwrap().unwrap_err(), SessionReply::NotAccepted);
        assert_eq!(media_frame(&seen), None, "nothing may go on the wire");
    }
}
