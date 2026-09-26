use super::*;
use crate::{AckOutcome, ModuleInit, Output, MAX_FRAME};

// Fixtures computed INDEPENDENTLY of this crate (python3, 8-bit
// Fletcher over class..payload) — the frame consts must match them
// byte for byte, so a checksum bug in `ubx_frame` cannot self-confirm.
// Cross-check anchor: the public u-blox cold-start example frame
// `B5 62 06 04 04 00 FF FF 02 00 0E 61` (resetMode 0x02) differs from
// our resetMode 0x01 frame by exactly the expected checksum delta.
const FIXTURE_CFG: [u8; 21] = [
    0xB5, 0x62, 0x06, 0x09, 0x0D, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xFF,
    0x00, 0x00, 0x17, 0x2F, 0xAE,
];
const FIXTURE_RST: [u8; 12] = [
    0xB5, 0x62, 0x06, 0x04, 0x04, 0x00, 0xFF, 0xFF, 0x01, 0x00, 0x0D, 0x5F,
];
const FIXTURE_PMS: [u8; 16] = [
    0xB5, 0x62, 0x06, 0x86, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x94, 0x5A,
];
const FIXTURE_ANT: [u8; 12] = [
    0xB5, 0x62, 0x06, 0x13, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x1E, 0xD1,
];

// Module→host ACK/NAK frames, same independent computation.
const ACK_CFG: [u8; 10] = [0xB5, 0x62, 0x05, 0x01, 0x02, 0x00, 0x06, 0x09, 0x17, 0x40];
const ACK_PMS: [u8; 10] = [0xB5, 0x62, 0x05, 0x01, 0x02, 0x00, 0x06, 0x86, 0x94, 0xBD];
const NAK_PMS: [u8; 10] = [0xB5, 0x62, 0x05, 0x00, 0x02, 0x00, 0x06, 0x86, 0x93, 0xB8];
const ACK_ANT: [u8; 10] = [0xB5, 0x62, 0x05, 0x01, 0x02, 0x00, 0x06, 0x13, 0x21, 0x4A];
const NAK_ANT: [u8; 10] = [0xB5, 0x62, 0x05, 0x00, 0x02, 0x00, 0x06, 0x13, 0x20, 0x45];

fn on_bytes(m: &mut UbxInit, bytes: &[u8], now_ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    m.on_bytes(bytes, now_ms, &mut |o| out.push(o));
    out
}

fn poll(m: &mut UbxInit, locked: bool, sentences: u32, now_ms: u64) -> Vec<Output> {
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

// ---- Frame bytes ----

// The exact bytes on the wire, against the independent fixture. This is
// the test the positive control targets: corrupting one payload byte in
// `lib.rs` (or one fixture byte here) must turn it red. The cold-start
// frame is asserted too although it is no longer sent at boot — it is
// kept as a diagnostic frame and a diagnostic that has silently rotted
// is worse than none.
#[test]
fn frames_match_independent_fixtures() {
    assert_eq!(FACTORY_CLEAR_FRAME, FIXTURE_CFG);
    assert_eq!(COLD_START_FRAME, FIXTURE_RST);
    assert_eq!(FULL_POWER_FRAME, FIXTURE_PMS);
    assert_eq!(ANTENNA_SUPPLY_FRAME, FIXTURE_ANT);
}

// The documented payload semantics, pinned independently of the raw
// fixture so a wrong mask cannot hide behind a matching checksum.
#[test]
fn payload_semantics_hold() {
    // CFG-CFG: clear+load all sections, save nothing, all devices. The
    // load in the SAME message is what makes the reset unnecessary: a
    // clear alone only replaces the Permanent Configuration (M8 spec
    // §3.1), the load copies it into the Current Configuration.
    assert_eq!(&FACTORY_CLEAR_FRAME[6..10], &[0xFF, 0xFF, 0x00, 0x00]);
    assert_eq!(&FACTORY_CLEAR_FRAME[10..14], &[0x00; 4]);
    assert_eq!(&FACTORY_CLEAR_FRAME[14..18], &[0xFF, 0xFF, 0x00, 0x00]);
    assert_eq!(FACTORY_CLEAR_FRAME[18], 0x17);
    // CFG-RST (diagnostic only): cold-start mask, controlled software
    // reset.
    assert_eq!(&COLD_START_FRAME[6..8], &[0xFF, 0xFF]);
    assert_eq!(COLD_START_FRAME[8], 0x01);
    // CFG-PMS: powerSetupValue 0x00 = full power.
    assert_eq!(FULL_POWER_FRAME[7], 0x00);
    // CFG-ANT: flags = svcs alone (supply control on, scd/ocd/
    // pdwnOnSCD/recovery off — no automatic power-down path); pins
    // all-zero with reconfig (bit 15) clear, current routing kept.
    assert_eq!(&ANTENNA_SUPPLY_FRAME[6..8], &[0x01, 0x00]);
    assert_eq!(&ANTENNA_SUPPLY_FRAME[8..10], &[0x00, 0x00]);
}

// ---- Ordering: nothing before lock ----

// No TX before the first baud lock — a UBX frame at an unlocked baud is
// garbage into the module. Sentences alone (parser catching a stray
// clean sentence mid-sweep cannot happen, but the guard must not key on
// the counter) do not unlock TX.
#[test]
fn no_tx_before_lock() {
    let mut m = UbxInit::new();
    for i in 0..20u64 {
        let out = poll(&mut m, false, i as u32, i * 1_000);
        assert_eq!(out.len(), 0, "no output before lock");
    }
    let out = poll(&mut m, true, 20, 20_000);
    assert_eq!(
        sends(&out),
        vec![(Step::FactoryClear.as_str(), &FACTORY_CLEAR_FRAME[..])]
    );
}

// ---- The boot path never contains a reset ----

// Drive a whole boot sequence with generous time and a steady sentence
// flow, acknowledging every step. All three ACK frames are fed after
// each send: only the matching class/id is ever consumed, so this also
// exercises the non-matching path. Returns the steps in order.
fn drive_boot_sequence() -> Vec<(&'static str, &'static [u8])> {
    let mut m = UbxInit::new();
    let mut seen = Vec::new();
    for i in 0..600u64 {
        let t = i * 100;
        let out = poll(&mut m, true, i as u32 + 1, t);
        for send in sends(&out) {
            seen.push(send);
            on_bytes(&mut m, &ACK_CFG, t + 10);
            on_bytes(&mut m, &ACK_PMS, t + 20);
            on_bytes(&mut m, &ACK_ANT, t + 30);
        }
    }
    seen
}

// The every-boot sequence is cfg → pms → ant, and nothing else — in
// particular no UBX-CFG-RST. A cold start wipes ephemeris, almanac and
// last position, so forcing one on every boot costs a fresh sky
// download (tens of minutes at a marginal window) where a warm or hot
// start needs seconds (#324). Config hygiene does not need it: the
// CFG-CFG clear+load applies the defaults to the Current Configuration
// directly (M8 spec §3.1), and CFG-PMS/CFG-ANT take effect immediately
// like every UBX-CFG message.
#[test]
fn boot_path_is_cfg_pms_ant_without_reset() {
    let seen = drive_boot_sequence();
    assert_eq!(
        seen,
        vec![
            (Step::FactoryClear.as_str(), &FACTORY_CLEAR_FRAME[..]),
            (Step::FullPower.as_str(), &FULL_POWER_FRAME[..]),
            (Step::AntennaSupply.as_str(), &ANTENNA_SUPPLY_FRAME[..]),
        ]
    );
}

// The same run, asserted on the bytes rather than the step names: no
// frame the sequencer emits at boot is the cold-start frame, and none
// carries the UBX-CFG-RST class/id (0x06 0x04) under any other guise.
#[test]
fn cold_start_frame_never_reaches_the_wire_at_boot() {
    for (step, frame) in drive_boot_sequence() {
        assert_ne!(
            frame,
            &COLD_START_FRAME[..],
            "{step} must not send the cold-start frame"
        );
        assert_ne!(
            &frame[2..4],
            &[0x06, 0x04],
            "{step} must not send UBX-CFG-RST"
        );
    }
}

// ---- The full happy path ----

// lock → CFG-CFG → ACK → (settle + fresh sentence) → CFG-PMS → ACK →
// (settle + fresh sentence) → CFG-ANT → ACK → done, and one-shot:
// silent forever after.
#[test]
fn full_happy_path() {
    let mut m = UbxInit::new();

    // Lock at t=5s with 3 sentences seen.
    let out = poll(&mut m, true, 3, 5_000);
    assert_eq!(
        sends(&out),
        vec![(Step::FactoryClear.as_str(), &FACTORY_CLEAR_FRAME[..])]
    );

    // Module ACKs CFG-CFG promptly.
    let out = on_bytes(&mut m, &ACK_CFG, 5_100);
    assert_eq!(
        acks(&out),
        vec![(Step::FactoryClear.as_str(), AckOutcome::Ack)]
    );

    // Inside the settle: nothing, even with sentences flowing.
    let out = poll(&mut m, true, 5, 5_600);
    assert_eq!(out.len(), 0, "settle after CFG must hold");

    // Past the settle: first poll snapshots the counter, no send yet.
    let out = poll(&mut m, true, 6, 5_100 + POST_CFG_SETTLE_MS);
    assert_eq!(out.len(), 0, "snapshot poll must not send");
    // A fresh sentence after the snapshot releases CFG-PMS.
    let out = poll(&mut m, true, 7, 5_200 + POST_CFG_SETTLE_MS);
    assert_eq!(
        sends(&out),
        vec![(Step::FullPower.as_str(), &FULL_POWER_FRAME[..])]
    );
    let pms_t = 5_200 + POST_CFG_SETTLE_MS;

    // Module ACKs CFG-PMS.
    let out = on_bytes(&mut m, &ACK_PMS, pms_t + 100);
    assert_eq!(
        acks(&out),
        vec![(Step::FullPower.as_str(), AckOutcome::Ack)]
    );

    // Settle after PMS, snapshot, then a fresh sentence releases
    // CFG-ANT.
    let out = poll(&mut m, true, 8, pms_t + 600);
    assert_eq!(out.len(), 0, "settle after PMS must hold");
    let out = poll(&mut m, true, 9, pms_t + 100 + POST_PMS_SETTLE_MS);
    assert_eq!(out.len(), 0, "snapshot poll must not send");
    let out = poll(&mut m, true, 10, pms_t + 200 + POST_PMS_SETTLE_MS);
    assert_eq!(
        sends(&out),
        vec![(Step::AntennaSupply.as_str(), &ANTENNA_SUPPLY_FRAME[..])]
    );
    let ant_t = pms_t + 200 + POST_PMS_SETTLE_MS;

    // Module ACKs CFG-ANT.
    let out = on_bytes(&mut m, &ACK_ANT, ant_t + 100);
    assert_eq!(
        acks(&out),
        vec![(Step::AntennaSupply.as_str(), AckOutcome::Ack)]
    );

    // One-shot: nothing ever again, whatever flows.
    for i in 0..30u64 {
        let t = ant_t + 1_000 + i * 1_000;
        assert_eq!(poll(&mut m, true, 100 + i as u32, t).len(), 0);
        assert_eq!(on_bytes(&mut m, &ACK_PMS, t).len(), 0);
        assert_eq!(on_bytes(&mut m, &ACK_ANT, t).len(), 0);
    }
}

// ---- ACK outcomes ----

// A NAK is reported and the sequence continues (reference behaviour:
// warn and move on) — CFG-PMS still goes out after a NAK'd CFG-CFG.
#[test]
fn nak_reports_and_continues() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000);
    let nak_cfg: [u8; 10] = [0xB5, 0x62, 0x05, 0x00, 0x02, 0x00, 0x06, 0x09, 0x16, 0x3B];
    let out = on_bytes(&mut m, &nak_cfg, 1_100);
    assert_eq!(
        acks(&out),
        vec![(Step::FactoryClear.as_str(), AckOutcome::Nak)]
    );

    poll(&mut m, true, 4, 1_100 + POST_CFG_SETTLE_MS);
    let out = poll(&mut m, true, 5, 1_200 + POST_CFG_SETTLE_MS);
    assert_eq!(
        sends(&out),
        vec![(Step::FullPower.as_str(), &FULL_POWER_FRAME[..])]
    );
}

// An ACK that never arrives times out, is reported, and the sequence
// continues to the next step. The CFG-CFG ACK is the one most likely to
// be lost: clearing the ioPort sub-section resets the module's I/O
// system, and the spec warns that undefined data may be output for a
// short period afterwards (M8 spec §UBX-CFG-CFG, clearMask/ioPort).
#[test]
fn ack_timeout_reports_and_continues() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000);
    let out = poll(&mut m, true, 3, 999 + CFG_ACK_TIMEOUT_MS);
    assert_eq!(out.len(), 0, "deadline must run its full length");
    let out = poll(&mut m, true, 3, 1_000 + CFG_ACK_TIMEOUT_MS);
    assert_eq!(
        acks(&out),
        vec![(Step::FactoryClear.as_str(), AckOutcome::Timeout)]
    );
    let timeout_t = 1_000 + CFG_ACK_TIMEOUT_MS;

    poll(&mut m, true, 4, timeout_t + POST_CFG_SETTLE_MS);
    let out = poll(&mut m, true, 5, timeout_t + 100 + POST_CFG_SETTLE_MS);
    assert_eq!(
        sends(&out),
        vec![(Step::FullPower.as_str(), &FULL_POWER_FRAME[..])]
    );
}

// A NAK'd CFG-PMS does not end the sequence: CFG-ANT still follows
// (reference behaviour: warn and move on).
#[test]
fn pms_nak_continues_to_ant() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000);
    on_bytes(&mut m, &ACK_CFG, 1_100);
    poll(&mut m, true, 4, 1_100 + POST_CFG_SETTLE_MS);
    let out = poll(&mut m, true, 5, 1_200 + POST_CFG_SETTLE_MS);
    let pms_t = 1_200 + POST_CFG_SETTLE_MS;
    assert_eq!(
        sends(&out),
        vec![(Step::FullPower.as_str(), &FULL_POWER_FRAME[..])]
    );
    let out = on_bytes(&mut m, &NAK_PMS, pms_t + 100);
    assert_eq!(
        acks(&out),
        vec![(Step::FullPower.as_str(), AckOutcome::Nak)]
    );

    poll(&mut m, true, 6, pms_t + 100 + POST_PMS_SETTLE_MS);
    let out = poll(&mut m, true, 7, pms_t + 200 + POST_PMS_SETTLE_MS);
    assert_eq!(
        sends(&out),
        vec![(Step::AntennaSupply.as_str(), &ANTENNA_SUPPLY_FRAME[..])]
    );
}

// The ANT NAK path ends the sequence (no retry, nothing after the
// final step).
#[test]
fn ant_nak_ends_sequence() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000);
    on_bytes(&mut m, &ACK_CFG, 1_100);
    poll(&mut m, true, 4, 1_100 + POST_CFG_SETTLE_MS);
    poll(&mut m, true, 5, 1_200 + POST_CFG_SETTLE_MS);
    let pms_t = 1_200 + POST_CFG_SETTLE_MS;
    on_bytes(&mut m, &ACK_PMS, pms_t + 100);
    poll(&mut m, true, 6, pms_t + 100 + POST_PMS_SETTLE_MS);
    let out = poll(&mut m, true, 7, pms_t + 200 + POST_PMS_SETTLE_MS);
    let ant_t = pms_t + 200 + POST_PMS_SETTLE_MS;
    assert_eq!(sends(&out).len(), 1);
    let out = on_bytes(&mut m, &NAK_ANT, ant_t + 100);
    assert_eq!(
        acks(&out),
        vec![(Step::AntennaSupply.as_str(), AckOutcome::Nak)]
    );
    assert_eq!(poll(&mut m, true, 50, ant_t + 60_000).len(), 0);
    assert_eq!(poll(&mut m, true, 51, ant_t + 61_000).len(), 0);
}

// ---- Post-CFG gate: the I/O-reset race ----

// CFG-PMS must not fire into a module whose I/O system is still coming
// back: clearing the ioPort sub-section resets it, and the load may put
// the port back on the default baud (M8 spec §3.1 and §UBX-CFG-CFG,
// clearMask/ioPort — "undefined data may be output for a short period
// of time"). Neither time alone (settle passed, no fresh sentence) nor
// sentences alone (fresh sentences before the settle — the pre-clear
// burst still in flight) release the step, and a lost lock (the
// presence machine's starve re-sweep after a baud fallback) holds it
// even with the counter moving.
#[test]
fn pms_gate_needs_settle_and_fresh_sentence_and_lock() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000);
    on_bytes(&mut m, &ACK_CFG, 1_100);
    let gate_t = 1_100 + POST_CFG_SETTLE_MS;

    // Sentences flowing BEFORE the settle (pre-clear burst): no send.
    let out = poll(&mut m, true, 9, 1_600);
    assert_eq!(out.len(), 0, "pre-settle sentences must not release PMS");

    // Settle passed, snapshot taken — counter frozen (module silent or
    // babbling at a baud we no longer follow): no send, however often
    // polled.
    poll(&mut m, true, 9, gate_t);
    for i in 1..10u64 {
        let out = poll(&mut m, true, 9, gate_t + i * 1_000);
        assert_eq!(out.len(), 0, "frozen counter must not release PMS");
    }

    // Counter moves but the lock is gone (starve re-sweep in progress
    // after a baud fallback): still held.
    let out = poll(&mut m, false, 10, gate_t + 11_000);
    assert_eq!(out.len(), 0, "unlocked line must not release PMS");

    // Locked again with a fresh sentence: released.
    let out = poll(&mut m, true, 11, gate_t + 12_000);
    assert_eq!(
        sends(&out),
        vec![(Step::FullPower.as_str(), &FULL_POWER_FRAME[..])]
    );
}

// ---- Post-PMS gate: same rules for CFG-ANT ----

// CFG-ANT obeys the same settle + fresh-sentence + lock gate as the
// step before it: neither time alone nor a frozen counter releases
// it, and a lost lock holds it even with the counter moving.
#[test]
fn ant_gate_needs_settle_and_fresh_sentence_and_lock() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000);
    on_bytes(&mut m, &ACK_CFG, 1_100);
    poll(&mut m, true, 4, 1_100 + POST_CFG_SETTLE_MS);
    poll(&mut m, true, 5, 1_200 + POST_CFG_SETTLE_MS);
    let pms_t = 1_200 + POST_CFG_SETTLE_MS;
    on_bytes(&mut m, &ACK_PMS, pms_t + 100);
    let gate_t = pms_t + 100 + POST_PMS_SETTLE_MS;

    // Sentences flowing BEFORE the settle: no send.
    let out = poll(&mut m, true, 9, pms_t + 500);
    assert_eq!(out.len(), 0, "pre-settle sentences must not release ANT");

    // Settle passed, snapshot taken — counter frozen: no send.
    poll(&mut m, true, 9, gate_t);
    for i in 1..10u64 {
        let out = poll(&mut m, true, 9, gate_t + i * 1_000);
        assert_eq!(out.len(), 0, "frozen counter must not release ANT");
    }

    // Counter moves but the lock is gone: still held.
    let out = poll(&mut m, false, 10, gate_t + 11_000);
    assert_eq!(out.len(), 0, "unlocked line must not release ANT");

    // Locked again with a fresh sentence: released.
    let out = poll(&mut m, true, 11, gate_t + 12_000);
    assert_eq!(
        sends(&out),
        vec![(Step::AntennaSupply.as_str(), &ANTENNA_SUPPLY_FRAME[..])]
    );
}

// ---- ACK scanner robustness ----

// The ACK hides between NMEA text and wrong-baud noise, split across
// two chunks — still found. NMEA is ASCII, so a 0xB5 sync byte cannot
// occur inside a sentence.
#[test]
fn ack_scanner_finds_frame_in_noise_and_across_chunks() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000);

    let mut chunk1 = b"$GNRMC,,V,,,,,,,,,,N*4D\r\n".to_vec();
    chunk1.extend_from_slice(&[0xFF, 0xB5, 0x00, 0x13]); // noise incl. a fake sync
    chunk1.extend_from_slice(&ACK_CFG[..4]); // frame cut mid-header
    let out = on_bytes(&mut m, &chunk1, 1_100);
    assert_eq!(out.len(), 0);

    let mut chunk2 = ACK_CFG[4..].to_vec();
    chunk2.extend_from_slice(b"$GNGGA,,,,,,0,00,99.99,,,,,,*56\r\n");
    let out = on_bytes(&mut m, &chunk2, 1_200);
    assert_eq!(
        acks(&out),
        vec![(Step::FactoryClear.as_str(), AckOutcome::Ack)]
    );
}

// Corrupting a checksum byte must kill the frame — the scanner-side
// positive control that the Fletcher check is real.
#[test]
fn ack_scanner_rejects_corrupt_checksum() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000);
    let mut bad = ACK_CFG;
    bad[9] ^= 0x01;
    let out = on_bytes(&mut m, &bad, 1_100);
    assert_eq!(out.len(), 0, "corrupt checksum must not count as ACK");
    // The intact frame right after is still recognised.
    let out = on_bytes(&mut m, &ACK_CFG, 1_200);
    assert_eq!(
        acks(&out),
        vec![(Step::FactoryClear.as_str(), AckOutcome::Ack)]
    );
}

// An ACK for a different message (wrong class/id payload) is not ours.
#[test]
fn ack_for_other_message_is_ignored() {
    let mut m = UbxInit::new();
    poll(&mut m, true, 3, 1_000); // awaiting CFG-CFG ack (06,09)
    let out = on_bytes(&mut m, &ACK_PMS, 1_100); // ack for (06,86)
    assert_eq!(out.len(), 0);
}

// ---- Constants ----

#[test]
fn constants_hold_their_justifications() {
    assert!(MAX_FRAME >= FACTORY_CLEAR_FRAME.len());
    assert!(MAX_FRAME >= COLD_START_FRAME.len());
    assert!(MAX_FRAME >= FULL_POWER_FRAME.len());
    assert!(MAX_FRAME >= ANTENNA_SUPPLY_FRAME.len());
    // Both settles hold at least the reference's 1 s after a message
    // that restarts a receiver subsystem (`GPS.cpp:711-713`). Constants on
    // both sides, so const blocks: they fail the build, not one test run.
    const { assert!(POST_CFG_SETTLE_MS >= 1_000) };
    const { assert!(POST_PMS_SETTLE_MS >= 1_000) };
}
