use super::*;
use crate::MAX_FRAME;

// Fixtures written out as literal wire text, INDEPENDENT of
// `nmea_frame`: the checksums were computed with a separate XOR (python3)
// and three of the four are the reference's own literals
// (`meshtastic/src/gps/GPS.cpp:543/549`, `:1363`), copied character for
// character. A checksum bug in the const fn therefore cannot confirm
// itself.
const FIXTURE_PROBE: &[u8] = b"$PCAS06,0*1B\r\n";
const FIXTURE_CONSTELLATIONS: &[u8] = b"$PCAS04,7*1E\r\n";
const FIXTURE_SENTENCES: &[u8] = b"$PCAS03,1,0,0,1,1,0,0,0,0,0,,,0,0*03\r\n";
const FIXTURE_NAV_MODE: &[u8] = b"$PCAS11,3*1E\r\n";

/// The reference's sentence-selection line, GSV off. Kept as a fixture
/// because the ONE documented deviation is measured against it.
const REFERENCE_SENTENCES: &[u8] = b"$PCAS03,1,0,0,0,1,0,0,0,0,0,,,0,0*02\r\n";

/// A plausible answer to `$PCAS06,0` from a real L76K.
const PROBE_ANSWER_LINE: &[u8] = b"$GPTXT,01,01,02,SW=URANUS5,V5.3.0.0*1D\r\n";

fn on_bytes(m: &mut L76kInit, bytes: &[u8], now_ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    m.on_bytes(bytes, now_ms, &mut |o| out.push(o));
    out
}

fn poll(m: &mut L76kInit, locked: bool, sentences: u32, now_ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    m.poll(locked, sentences, now_ms, &mut |o| out.push(o));
    out
}

fn sends(outputs: &[Output]) -> Vec<(&'static str, &'static [u8])> {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::Send { step, frame } => Some((*step, *frame)),
            _ => None,
        })
        .collect()
}

fn acks(outputs: &[Output]) -> Vec<(&'static str, AckOutcome)> {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::AckResult { step, outcome } => Some((*step, *outcome)),
            _ => None,
        })
        .collect()
}

/// Drive the whole sequence with a generous clock and a steady sentence
/// flow, answering the probe. Returns the steps in the order they went
/// out.
fn drive_boot_sequence(answer_probe: bool) -> Vec<(&'static str, &'static [u8])> {
    let mut m = L76kInit::new();
    let mut seen = Vec::new();
    for i in 0..600u64 {
        let t = i * 100;
        let out = poll(&mut m, true, i as u32 + 1, t);
        for send in sends(&out) {
            seen.push(send);
            if answer_probe {
                on_bytes(&mut m, PROBE_ANSWER_LINE, t + 10);
            }
        }
    }
    seen
}

// ---- Frame bytes ----

// The exact bytes on the wire against the independent fixtures. This is
// the test the positive control targets: flipping one character in a
// frame body (or one fixture byte here) must turn it red.
#[test]
fn frames_match_independent_fixtures() {
    assert_eq!(PROBE_FRAME, FIXTURE_PROBE);
    assert_eq!(CONSTELLATIONS_FRAME, FIXTURE_CONSTELLATIONS);
    assert_eq!(SENTENCES_FRAME, FIXTURE_SENTENCES);
    assert_eq!(NAV_MODE_FRAME, FIXTURE_NAV_MODE);
}

// Every frame is a well-formed NMEA sentence: `$`, body, `*`, two
// uppercase hex digits of the XOR over the body, CRLF. Asserted by
// re-deriving the checksum here rather than by comparing to a fixture,
// so the framing rule itself is pinned and not just four byte strings.
#[test]
fn frames_are_well_formed_nmea() {
    for frame in [
        &PROBE_FRAME[..],
        &CONSTELLATIONS_FRAME[..],
        &SENTENCES_FRAME[..],
        &NAV_MODE_FRAME[..],
    ] {
        assert_eq!(frame[0], b'$');
        assert_eq!(&frame[frame.len() - 2..], b"\r\n");
        let star = frame.len() - 5;
        assert_eq!(frame[star], b'*');
        let ck = frame[1..star].iter().fold(0u8, |a, b| a ^ b);
        let text = core::str::from_utf8(&frame[star + 1..star + 3]).unwrap();
        assert_eq!(text, format!("{ck:02X}"), "checksum of {frame:?}");
    }
}

// The one documented deviation from the reference, pinned as a
// deviation: our sentence selection differs from Meshtastic's in
// exactly the GSV field and nothing else. GSV feeds the `sv=`/`cno=`
// heartbeat (#324), which is what tells an antenna problem from a
// configuration problem while presence still reads no-fix.
#[test]
fn sentence_selection_differs_from_reference_only_in_gsv() {
    assert_ne!(SENTENCES_FRAME, REFERENCE_SENTENCES);
    // Field 4 of $PCAS03 is nGSV; ours asks for it, the reference does
    // not. Everything up to that field and the whole tail after it are
    // character-identical.
    let field4 = b"$PCAS03,1,0,0,".len();
    assert_eq!(SENTENCES_FRAME[..field4], REFERENCE_SENTENCES[..field4]);
    assert_eq!(SENTENCES_FRAME[field4], b'1');
    assert_eq!(REFERENCE_SENTENCES[field4], b'0');
    let tail = field4 + 1;
    let ck = SENTENCES_FRAME.len() - 5;
    assert_eq!(SENTENCES_FRAME[tail..ck], REFERENCE_SENTENCES[tail..ck]);
    // GGA (field 1) and RMC (field 5) stay on, as in the reference.
    assert_eq!(SENTENCES_FRAME[b"$PCAS03,".len()], b'1');
    assert_eq!(SENTENCES_FRAME[b"$PCAS03,1,0,0,1,".len()], b'1');
}

// A baud command would desynchronise the line the driver is talking on:
// the driver's rate is whatever the presence machine's sweep locked, not
// a constant this crate could keep in step with. No frame is a $PCAS01.
#[test]
fn no_frame_changes_the_baud_rate() {
    for (step, frame) in drive_boot_sequence(true) {
        assert!(
            !frame.starts_with(b"$PCAS01"),
            "{step} must not set the module baud rate"
        );
    }
}

// ---- Ordering: nothing before lock ----

// No TX before the first baud lock — a command at an unlocked baud is
// garbage into the module. A moving sentence counter alone does not
// unlock TX.
#[test]
fn no_tx_before_lock() {
    let mut m = L76kInit::new();
    for i in 0..20u64 {
        let out = poll(&mut m, false, i as u32, i * 1_000);
        assert_eq!(out.len(), 0, "no output before lock");
    }
    let out = poll(&mut m, true, 20, 20_000);
    assert_eq!(sends(&out), vec![(Step::Probe.as_str(), &PROBE_FRAME[..])]);
}

// ---- The boot path ----

// probe → constellations → sentences → nav-mode, and nothing else,
// whether or not the probe was answered. An unanswered query must not
// stop the configuration: the commands are unacknowledged anyway, so a
// silent module may still be obeying them.
#[test]
fn boot_path_is_probe_then_the_three_config_sentences() {
    let expected = vec![
        (Step::Probe.as_str(), &PROBE_FRAME[..]),
        (Step::Constellations.as_str(), &CONSTELLATIONS_FRAME[..]),
        (Step::Sentences.as_str(), &SENTENCES_FRAME[..]),
        (Step::NavMode.as_str(), &NAV_MODE_FRAME[..]),
    ];
    assert_eq!(drive_boot_sequence(true), expected);
    assert_eq!(drive_boot_sequence(false), expected);
}

// The full happy path with the gates walked one by one, ending in the
// one-shot property: silent forever after the last step, whatever flows.
#[test]
fn full_happy_path() {
    let mut m = L76kInit::new();

    // Lock at t=5s with 3 sentences seen.
    let out = poll(&mut m, true, 3, 5_000);
    assert_eq!(sends(&out), vec![(Step::Probe.as_str(), &PROBE_FRAME[..])]);

    // The module answers the version query.
    let out = on_bytes(&mut m, PROBE_ANSWER_LINE, 5_100);
    assert_eq!(acks(&out), vec![(Step::Probe.as_str(), AckOutcome::Ack)]);

    let mut t = 5_100;
    for (step, frame) in [
        (Step::Constellations, &CONSTELLATIONS_FRAME[..]),
        (Step::Sentences, &SENTENCES_FRAME[..]),
        (Step::NavMode, &NAV_MODE_FRAME[..]),
    ] {
        // Inside the settle: nothing, even with sentences flowing.
        let out = poll(&mut m, true, 100, t + POST_STEP_SETTLE_MS - 1);
        assert_eq!(out.len(), 0, "settle before {} must hold", step.as_str());
        // Past the settle: the first poll only snapshots the counter.
        let out = poll(&mut m, true, 100, t + POST_STEP_SETTLE_MS);
        assert_eq!(out.len(), 0, "snapshot poll must not send");
        // A fresh sentence after the snapshot releases the step.
        t += POST_STEP_SETTLE_MS + 100;
        let out = poll(&mut m, true, 101, t);
        assert_eq!(sends(&out), vec![(step.as_str(), frame)]);
    }

    // One-shot: nothing ever again.
    for i in 0..30u64 {
        let at = t + 1_000 + i * 1_000;
        assert_eq!(poll(&mut m, true, 200 + i as u32, at).len(), 0);
        assert_eq!(on_bytes(&mut m, PROBE_ANSWER_LINE, at).len(), 0);
    }
}

// ---- The probe answer ----

// An answer that never arrives times out, is reported once, and the
// sequence continues to the first configuration sentence.
#[test]
fn probe_timeout_reports_once_and_continues() {
    let mut m = L76kInit::new();
    poll(&mut m, true, 3, 1_000);
    let out = poll(&mut m, true, 3, 999 + PROBE_TIMEOUT_MS);
    assert_eq!(out.len(), 0, "deadline must run its full length");
    let out = poll(&mut m, true, 3, 1_000 + PROBE_TIMEOUT_MS);
    assert_eq!(
        acks(&out),
        vec![(Step::Probe.as_str(), AckOutcome::Timeout)]
    );
    let timeout_t = 1_000 + PROBE_TIMEOUT_MS;

    // A late answer after the timeout is not re-reported.
    let out = on_bytes(&mut m, PROBE_ANSWER_LINE, timeout_t + 10);
    assert_eq!(out.len(), 0, "a late answer must not be reported twice");

    poll(&mut m, true, 4, timeout_t + POST_STEP_SETTLE_MS);
    let out = poll(&mut m, true, 5, timeout_t + 100 + POST_STEP_SETTLE_MS);
    assert_eq!(
        sends(&out),
        vec![(Step::Constellations.as_str(), &CONSTELLATIONS_FRAME[..])]
    );
}

// The answer arrives buried in a normal NMEA burst and split across two
// read chunks — still found. A `$` inside the noise must not desync the
// matcher.
#[test]
fn answer_scanner_finds_the_line_in_noise_and_across_chunks() {
    let mut m = L76kInit::new();
    poll(&mut m, true, 3, 1_000);

    let mut chunk1 = b"$GNRMC,,V,,,,,,,,,,N*4D\r\n$GPTXT,01,01,02,MISMATCH*00\r\n".to_vec();
    chunk1.extend_from_slice(&PROBE_ANSWER[..7]); // answer cut mid-line
    let out = on_bytes(&mut m, &chunk1, 1_100);
    assert_eq!(out.len(), 0);

    let mut chunk2 = PROBE_ANSWER[7..].to_vec();
    chunk2.extend_from_slice(b"URANUS5*1D\r\n$GNGGA,,,,,,0,00,99.99,,,,,,*56\r\n");
    let out = on_bytes(&mut m, &chunk2, 1_200);
    assert_eq!(acks(&out), vec![(Step::Probe.as_str(), AckOutcome::Ack)]);
}

// A truncated false start immediately followed by the real answer: the
// matcher must restart on the `$` that breaks the match rather than
// swallowing it, or the answer right behind it is lost.
#[test]
fn answer_scanner_restarts_on_a_dollar_that_breaks_the_match() {
    let mut m = L76kInit::new();
    poll(&mut m, true, 3, 1_000);

    let mut bytes = b"$GPTXT,01,01,02,S".to_vec(); // breaks off before "W="
    bytes.extend_from_slice(PROBE_ANSWER_LINE);
    let out = on_bytes(&mut m, &bytes, 1_100);
    assert_eq!(acks(&out), vec![(Step::Probe.as_str(), AckOutcome::Ack)]);
}

// Some other GPTXT line (the ATGM336H hardware string the reference
// probes for with $PCAS06,1) is not our answer.
#[test]
fn a_different_gptxt_line_is_not_the_answer() {
    let mut m = L76kInit::new();
    poll(&mut m, true, 3, 1_000);
    let out = on_bytes(&mut m, b"$GPTXT,01,01,02,HW=ATGM336H*4C\r\n", 1_100);
    assert_eq!(out.len(), 0);
}

// ---- The gate in front of every configuration sentence ----

// Neither time alone (settle passed, counter frozen) nor sentences alone
// (fresh sentences before the settle) release a step, and a lost lock —
// the presence machine's starve re-sweep — holds it even with the
// counter moving. A command written into a line we no longer follow is
// garbage into the module.
#[test]
fn config_gate_needs_settle_and_fresh_sentence_and_lock() {
    let mut m = L76kInit::new();
    poll(&mut m, true, 3, 1_000);
    on_bytes(&mut m, PROBE_ANSWER_LINE, 1_100);
    let gate_t = 1_100 + POST_STEP_SETTLE_MS;

    // Sentences flowing BEFORE the settle: no send.
    let out = poll(&mut m, true, 9, 1_150);
    assert_eq!(out.len(), 0, "pre-settle sentences must not release");

    // Settle passed, snapshot taken, counter frozen: no send, however
    // often polled.
    poll(&mut m, true, 9, gate_t);
    for i in 1..10u64 {
        let out = poll(&mut m, true, 9, gate_t + i * 1_000);
        assert_eq!(out.len(), 0, "frozen counter must not release");
    }

    // Counter moves but the lock is gone: still held.
    let out = poll(&mut m, false, 10, gate_t + 11_000);
    assert_eq!(out.len(), 0, "unlocked line must not release");

    // Locked again with a fresh sentence: released.
    let out = poll(&mut m, true, 11, gate_t + 12_000);
    assert_eq!(
        sends(&out),
        vec![(Step::Constellations.as_str(), &CONSTELLATIONS_FRAME[..])]
    );
}

// ---- Constants ----

#[test]
fn constants_hold_their_justifications() {
    assert!(MAX_FRAME >= SENTENCES_FRAME.len());
    // The settle is at least the reference's 250 ms between $PCAS
    // writes (`GPS.cpp:544/547/550`), and the probe wait at least its
    // 500 ms (`GPS.cpp:1363`). Constants on both sides, so const blocks:
    // they fail the build, not one test run.
    const { assert!(POST_STEP_SETTLE_MS >= 250) };
    const { assert!(PROBE_TIMEOUT_MS >= 500) };
}
