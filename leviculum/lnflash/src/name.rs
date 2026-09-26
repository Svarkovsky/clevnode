//! `--set-name` / `--clear-name` — what an operator calls a board
//! (Codeberg #235).
//!
//! The board-facing half is two control-envelope frames
//! (`TYPE_NODE_NAME` to set or clear, `TYPE_NODE_NAME_QUERY` to read);
//! everything in this module is the human-facing half: validate what a
//! person typed before a byte reaches a board, and say what the board
//! answered. Nothing here opens a port — the open lives with
//! [`crate::flow`], which proves it landed on the intended board — so the
//! parsing and the wire bytes are testable without one.
//!
//! # Why the flag exists
//!
//! A board has always had a name, derived from its identity:
//! `LNode-<hex8>` on the mesh and `LN-<hex8>` on BLE. That is enough to
//! tell two boards apart on a bench and nothing like enough to run a
//! deployment: an operator with four field nodes wants to read `Balkon-
//! Nord`, not eight hex digits, in Columba and in their phone's Bluetooth
//! list alike.
//!
//! # One name, two surfaces, two moments
//!
//! Setting a name sets **both** surfaces — a board answering to two names
//! in two places would be worse than the hex it replaced. They do not
//! adopt it at the same moment, and the board says which is which:
//!
//! * the **mesh** name is in force immediately, from the next announce;
//! * the **BLE** name was baked into the advertisement at boot and follows
//!   at the next reset.
//!
//! That is what [`reboot_note`] turns into a sentence. It is the media
//! profile's "takes effect at reboot" answer on a second feature, and for
//! the same reason: a promise the board cannot keep until it is reset must
//! be said out loud, not implied by an ack.
//!
//! # What is refused, and why refusal beats truncation
//!
//! The rules and their reasoning are
//! [`leviculum_core::node_name`] — length (32 bytes, derived there from
//! the airtime the name costs in every announce), UTF-8, no control
//! characters, no surrounding whitespace. This module only decides *when*
//! they are applied: at the command line, before any board is touched, so
//! a mistyped name stops the run rather than half-configuring a bench.
//!
//! **A name that arrives different from the one that was typed is worse
//! than an error message.** So nothing here trims, replaces or shortens
//! what an operator wrote. The one shortening that does happen —
//! the BLE surface's 11-byte bound — is performed by the board, reported
//! back as the actual string, and printed.

use std::io;

use leviculum_core::envelope::{
    IdentityReportWire, NodeNameState, TYPE_IDENTITY_QUERY, TYPE_NODE_NAME, TYPE_NODE_NAME_QUERY,
};
use leviculum_core::node_name::NodeName;

use crate::envelope::{self, SessionReply};
use crate::sys::Fd;

/// Validate what `--set-name` was given.
///
/// The error is the one [`leviculum_core::node_name`] produced, prefixed
/// with the flag, so an operator reads what to type instead rather than
/// "invalid".
pub fn parse_name(text: &str) -> Result<NodeName, String> {
    NodeName::parse(text.as_bytes()).map_err(|err| format!("--set-name {text:?}: {err}"))
}

/// One line describing what a board is called, for the transcript and for
/// grep.
///
/// Both surfaces always, even when they agree: an operator checking why a
/// phone shows something else must be able to see the two values side by
/// side without running a second command. Deliberately the same
/// `key=value` shape the board's own `[NAME ]` banner uses, so comparing
/// the two is comparing identical text.
pub fn describe(state: &NodeNameState) -> String {
    format!(
        "mesh={} ble={} src={}",
        state.mesh,
        state.ble,
        if state.stored { "flash" } else { "derived" }
    )
}

/// Read the board's name on an already-opened transport port.
///
/// Behind the capability probe like every other command, so firmware
/// without the frame is reported as such rather than written to blind.
pub fn query(fd: &Fd) -> io::Result<Result<NodeNameState, SessionReply>> {
    let Some(caps) = envelope::probe_capabilities(fd)? else {
        return Ok(Err(SessionReply::ProbeSilent));
    };
    if !caps.accepts(TYPE_NODE_NAME_QUERY) {
        return Ok(Err(SessionReply::NotAccepted));
    }
    Ok(envelope::query_node_name(fd)?.map_err(SessionReply::from))
}

/// Read the board's identity hashes on an already-opened transport port
/// — what `--set-name`'s transcript prints beside the names, so an
/// operator gets the probe destination without a debug-port reader.
///
/// Behind the capability probe like [`query`]; firmware from before the
/// frame comes back [`SessionReply::NotAccepted`] and the caller says so
/// instead of deriving hashes the board never confirmed.
pub fn identity(fd: &Fd) -> io::Result<Result<IdentityReportWire, SessionReply>> {
    let Some(caps) = envelope::probe_capabilities(fd)? else {
        return Ok(Err(SessionReply::ProbeSilent));
    };
    if !caps.accepts(TYPE_IDENTITY_QUERY) {
        return Ok(Err(SessionReply::NotAccepted));
    }
    Ok(envelope::query_identity(fd)?.map_err(SessionReply::from))
}

/// One line with the three hashes a prober needs, for the transcript and
/// for grep. The same `key=value` shape — and the same keys — as the
/// board's own `[IDENTITY]` banner on the debug port, so comparing the
/// two is comparing identical text. `none` marks a destination this
/// boot did not register.
pub fn describe_identity(report: &IdentityReportWire) -> String {
    fn hex(hash: &[u8; 16]) -> String {
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }
    fn maybe(hash: &Option<[u8; 16]>) -> String {
        match hash {
            Some(hash) => hex(hash),
            None => "none".to_string(),
        }
    }
    format!(
        "identity={} probe={} lxmf={}",
        hex(&report.identity),
        maybe(&report.probe),
        maybe(&report.lxmf)
    )
}

/// Set the board's name, or clear it back to the derived default
/// (`name == None`).
///
/// Returns the board's report — both effective names and whether BLE is
/// still a reset behind — because that is the only place those strings
/// exist: the derived defaults are built from an identity hash this side
/// never sees, and the BLE name may be a shortened form of what was sent.
pub fn send(fd: &Fd, name: Option<&NodeName>) -> io::Result<Result<NodeNameState, SessionReply>> {
    let Some(caps) = envelope::probe_capabilities(fd)? else {
        return Ok(Err(SessionReply::ProbeSilent));
    };
    if !caps.accepts(TYPE_NODE_NAME) {
        return Ok(Err(SessionReply::NotAccepted));
    }
    Ok(envelope::send_node_name(fd, name)?.map_err(SessionReply::from))
}

/// What to tell an operator about a report beyond the names themselves:
/// that the BLE surfaces are one reset behind, and that the BLE name is
/// shorter than the mesh one.
///
/// Empty when neither applies, so a caller can append it unconditionally.
pub fn reboot_note(state: &NodeNameState) -> String {
    let mut note = String::new();
    if state.ble_pending {
        note.push_str(&format!(
            " Bluetooth still advertises {}: the advertisement is built once at boot and cannot \
             be rebuilt while the radio is up. Reset the board and it will show {} — the mesh \
             name is already in force.",
            state.ble,
            shortened_hint(state)
        ));
    } else if state.ble.as_str() != state.mesh.as_str() && state.stored {
        note.push_str(&format!(
            " Bluetooth shows {} rather than {}: a BLE device name may be at most {} bytes, so \
             the name is shortened there and left whole on the mesh.",
            state.ble,
            state.mesh,
            leviculum_core::node_name::BLE_NAME_MAX_LEN
        ));
    }
    note
}

/// What the BLE surfaces will show after the reset, computed the way the
/// board will compute it.
///
/// Only reachable with a stored name — a *cleared* board's BLE default is
/// `LN-<hex8>`, a string this side cannot build, so the note says the
/// board will show "its derived name" instead of guessing at one.
fn shortened_hint(state: &NodeNameState) -> String {
    if !state.stored {
        return "its derived name".to_string();
    }
    leviculum_core::node_name::truncate_on_char_boundary(
        state.mesh.as_str(),
        leviculum_core::node_name::BLE_NAME_MAX_LEN,
    )
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::testing::{
        booting_firmware_stub, envelope_firmware_stub, envelope_firmware_stub_with_name,
        name_state, name_state_booted, nameless_firmware_stub, node_name_frame, old_firmware_stub,
        pre_236_firmware_stub, seen,
    };
    use crate::sys::testpty::Pty;
    use leviculum_core::envelope::{REFUSE_BUSY, REFUSE_UNSUPPORTED};

    fn name(text: &str) -> NodeName {
        NodeName::parse(text.as_bytes()).unwrap()
    }

    // -----------------------------------------------------------------
    // What a human may type
    // -----------------------------------------------------------------

    #[test]
    fn an_ordinary_name_is_accepted_verbatim() {
        assert_eq!(parse_name("Balkon-Nord").unwrap().as_str(), "Balkon-Nord");
        assert_eq!(parse_name("Balkon Nord").unwrap().as_str(), "Balkon Nord");
        assert_eq!(parse_name("Küche").unwrap().as_str(), "Küche");
    }

    #[test]
    fn a_refused_name_is_named_with_the_flag_and_the_reason() {
        // The refusal has to be usable: which flag, what was typed, and
        // what the rule is. "invalid" would leave the operator guessing.
        let err = parse_name(&"x".repeat(40)).unwrap_err();
        assert!(err.contains("--set-name"), "{err}");
        assert!(err.contains("32"), "{err}");
        assert!(err.contains("40"), "{err}");

        for typed in [" Balkon", "Balkon ", "a\nb", ""] {
            assert!(parse_name(typed).is_err(), "{typed:?} was accepted");
        }
    }

    #[test]
    fn nothing_here_trims_or_shortens_what_was_typed() {
        // The rule the whole refusal set exists for: a name that arrives
        // different from the one that was typed is worse than an error.
        assert!(parse_name(" Balkon ").is_err());
        assert_eq!(parse_name("Balkon").unwrap().as_bytes(), b"Balkon");
    }

    // -----------------------------------------------------------------
    // The bytes that reach the board
    // -----------------------------------------------------------------

    #[test]
    fn a_name_reaches_the_board_and_comes_back_as_a_report() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let chosen = name("Balkon");
        let state = send(&fd, Some(&chosen)).unwrap().unwrap();
        assert!(state.stored);
        assert_eq!(state.mesh.as_str(), "Balkon");
        assert_eq!(node_name_frame(&seen), Some(Some(chosen)));
    }

    #[test]
    fn a_name_that_fits_ble_still_waits_for_the_reset() {
        // The honesty this report exists for. The mesh name is in force
        // the moment the frame is answered; the advertisement was built
        // at boot and cannot be rebuilt, so a phone keeps showing the old
        // name until the board is reset.
        let pty = Pty::open();
        envelope_firmware_stub_with_name(&pty, seen(), name_state());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let state = send(&fd, Some(&name("Balkon"))).unwrap().unwrap();
        assert_eq!(state.mesh.as_str(), "Balkon", "the mesh name is immediate");
        assert!(state.ble_pending);
        assert_eq!(state.ble.as_str(), "LN-a1b2c3d4", "BLE is a boot behind");

        let note = reboot_note(&state);
        assert!(note.contains("Reset the board"), "{note}");
        assert!(note.contains("Balkon"), "{note}");
    }

    #[test]
    fn a_board_already_booted_with_its_name_reports_nothing_pending() {
        // The other half of the rule: a board flashed, named and reset is
        // showing the name on both surfaces, and must not be reported as
        // waiting for a second reset it does not need.
        let pty = Pty::open();
        envelope_firmware_stub_with_name(&pty, seen(), name_state_booted(Some("Balkon")));
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let state = query(&fd).unwrap().unwrap();
        assert!(state.stored);
        assert!(!state.ble_pending);
        assert_eq!(state.mesh.as_str(), "Balkon");
        assert_eq!(state.ble.as_str(), "Balkon");
        assert_eq!(reboot_note(&state), "");
        assert_eq!(describe(&state), "mesh=Balkon ble=Balkon src=flash");
    }

    #[test]
    fn a_name_too_long_for_ble_is_shortened_there_and_whole_on_the_mesh() {
        // The two surfaces' bounds differ, so the report has to carry two
        // strings and the transcript has to print both — an operator
        // hunting for `Balkon-Nord-Solar` in a Bluetooth list would
        // otherwise conclude the board never took the name.
        let pty = Pty::open();
        envelope_firmware_stub_with_name(
            &pty,
            seen(),
            name_state_booted(Some("Balkon-Nord-Solar")),
        );
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let state = query(&fd).unwrap().unwrap();
        assert_eq!(state.mesh.as_str(), "Balkon-Nord-Solar");
        assert_eq!(state.ble.as_str(), "Balkon-Nord");
        assert!(!state.ble_pending, "this board booted with the name");
        let note = reboot_note(&state);
        assert!(note.contains("at most 11 bytes"), "{note}");
        assert!(
            describe(&state).contains("ble=Balkon-Nord"),
            "the transcript prints the shortened form the phone will show"
        );
    }

    #[test]
    fn clearing_goes_back_to_the_derived_defaults() {
        // Which are two *different* strings, neither of which this side
        // could have built — the reason the report carries resolved names
        // rather than the stored record.
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub_with_name(&pty, seen.clone(), name_state_booted(Some("Balkon")));
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let state = send(&fd, None).unwrap().unwrap();
        assert!(!state.stored);
        assert_eq!(state.mesh.as_str(), "LNode-a1b2c3d4");
        assert_eq!(
            node_name_frame(&seen),
            Some(None),
            "a clear, not an empty name"
        );
        assert_eq!(
            describe(&state).split(' ').next_back().unwrap(),
            "src=derived"
        );
    }

    #[test]
    fn the_query_reads_the_board_back_without_changing_it() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let state = query(&fd).unwrap().unwrap();
        assert!(!state.stored);
        assert_eq!(state.mesh.as_str(), "LNode-a1b2c3d4");
        assert_eq!(state.ble.as_str(), "LN-a1b2c3d4");
        assert_eq!(node_name_frame(&seen), None, "read-only: no set frame");
    }

    // -----------------------------------------------------------------
    // The identity hashes riding along on the same port
    // -----------------------------------------------------------------

    #[test]
    fn the_identity_query_reports_the_three_hashes_the_board_registered() {
        use crate::envelope::testing::{STUB_LXMF_HASH, STUB_PROBE_HASH};

        let pty = Pty::open();
        envelope_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let report = identity(&fd).unwrap().unwrap();
        assert_eq!(
            report.identity,
            crate::envelope::testing::STUB_IDENTITY_HASH
        );
        assert_eq!(report.probe, Some(STUB_PROBE_HASH));
        assert_eq!(report.lxmf, Some(STUB_LXMF_HASH));

        // The transcript line uses the debug-port banner's exact keys,
        // so grep hits both with one pattern.
        let line = describe_identity(&report);
        assert!(line.starts_with("identity=a1b2c3d4"), "{line}");
        assert!(line.contains(" probe=0c2192e5"), "{line}");
        assert!(line.contains(" lxmf=9b421c5a"), "{line}");
    }

    #[test]
    fn an_unregistered_destination_prints_as_none_not_as_zeroes() {
        let report = IdentityReportWire {
            identity: [0x11; 16],
            probe: None,
            lxmf: None,
        };
        let line = describe_identity(&report);
        assert!(line.contains("probe=none"), "{line}");
        assert!(line.contains("lxmf=none"), "{line}");
    }

    #[test]
    fn firmware_from_before_the_identity_query_is_reported_not_derived() {
        // The pre-#236 board advertises a list without the query; the
        // host must say "cannot read them here" rather than derive
        // hashes the board never confirmed.
        let pty = Pty::open();
        pre_236_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            identity(&fd).unwrap().unwrap_err(),
            SessionReply::NotAccepted
        );
    }

    // -----------------------------------------------------------------
    // Boards that cannot take the name
    // -----------------------------------------------------------------

    #[test]
    fn a_binary_without_the_name_gate_refuses_instead_of_acking() {
        // An operator told the board is now `Balkon` who then finds it
        // under neither name on either surface has been lied to, and no
        // retry or reboot fixes it.
        let pty = Pty::open();
        let seen = seen();
        nameless_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let chosen = name("Balkon");
        let reply = send(&fd, Some(&chosen)).unwrap().unwrap_err();
        assert_eq!(reply, SessionReply::Refused(REFUSE_UNSUPPORTED));
        assert!(!reply.took_it(), "took_it drives the non-zero exit");
        // The frame did reach the board — a refusal, not silence.
        assert_eq!(node_name_frame(&seen), Some(Some(chosen)));

        assert_eq!(
            query(&fd).unwrap().unwrap_err(),
            SessionReply::Refused(REFUSE_UNSUPPORTED)
        );
    }

    #[test]
    fn a_board_still_booting_says_busy_rather_than_reporting_a_zero_hash_name() {
        // USB comes up several statements into the firmware's main, the
        // identity only after the LoRa bring-up's awaited SPI
        // transactions. `busy` is a retry; a report built from sixteen
        // zero bytes would send the operator looking for `LNode-00000000`.
        let pty = Pty::open();
        booting_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            query(&fd).unwrap().unwrap_err(),
            SessionReply::Refused(REFUSE_BUSY)
        );
        assert_eq!(
            send(&fd, Some(&name("Balkon"))).unwrap().unwrap_err(),
            SessionReply::Refused(REFUSE_BUSY)
        );
    }

    #[test]
    fn an_old_board_is_reported_as_such_rather_than_written_to_blind() {
        let pty = Pty::open();
        let seen = seen();
        old_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            send(&fd, Some(&name("Balkon"))).unwrap().unwrap_err(),
            SessionReply::ProbeSilent
        );
        assert_eq!(query(&fd).unwrap().unwrap_err(), SessionReply::ProbeSilent);
        assert_eq!(node_name_frame(&seen), None, "nothing may go on the wire");
    }

    #[test]
    fn a_board_without_the_frames_is_named_not_guessed_at() {
        let pty = Pty::open();
        let seen = seen();
        pre_236_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            send(&fd, Some(&name("Balkon"))).unwrap().unwrap_err(),
            SessionReply::NotAccepted
        );
        assert_eq!(query(&fd).unwrap().unwrap_err(), SessionReply::NotAccepted);
        assert_eq!(node_name_frame(&seen), None, "nothing may go on the wire");
    }
}
