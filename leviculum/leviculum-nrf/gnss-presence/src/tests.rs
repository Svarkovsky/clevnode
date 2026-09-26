use super::*;

/// Build a framed NMEA sentence with a computed checksum, so fixtures
/// cannot silently carry a wrong checksum (the classic documentation
/// sentence below doubles as the positive control that this helper
/// matches the parser's checksum algorithm).
fn nmea(body: &str) -> Vec<u8> {
    let ck = body.bytes().fold(0u8, |a, b| a ^ b);
    format!("${body}*{ck:02X}\r\n").into_bytes()
}

/// Valid RMC (status A): 1994-11-19T22:54:46Z, the classic NMEA
/// documentation sentence body.
fn valid_rmc() -> Vec<u8> {
    nmea("GPRMC,225446,A,4916.45,N,12311.12,W,000.5,054.7,191194,020.3,E")
}

/// Populated but not-valid RMC (status V, no FAA mode field → mode
/// falls back to NotValid).
fn invalid_rmc() -> Vec<u8> {
    nmea("GPRMC,225446,V,4916.45,N,12311.12,W,000.5,054.7,191194,020.3,E")
}

/// Cold-start void RMC as a real receiver emits it indoors before any
/// satellite: every data field empty, mode N. Parses checksum-clean to
/// `RMC(None)`.
fn void_rmc() -> Vec<u8> {
    nmea("GNRMC,,V,,,,,,,,,,N")
}

/// GGA with a fix solution (quality 1, 7 sats).
fn fix_gga() -> Vec<u8> {
    nmea("GPGGA,225446,4916.45,N,12311.12,W,1,07,1.0,9.0,M,46.9,M,,")
}

/// Line noise: no '$', no structure. What a wrong-baud stream looks
/// like after the UART mangles the framing.
const GARBAGE: &[u8] = &[0xff, 0x00, 0x5a, 0xa5, 0x13, 0x37, 0x00, 0xfe, 0x80, 0x7f];

fn feed(m: &mut PresenceMachine, bytes: &[u8], now_ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    m.on_bytes(bytes, now_ms, &mut |o| out.push(o));
    out
}

fn poll(m: &mut PresenceMachine, now_ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    m.poll(now_ms, &mut |o| out.push(o));
    out
}

fn transitions(outputs: &[Output]) -> Vec<(Presence, u32)> {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::Transition { state, baud } => Some((*state, *baud)),
            _ => None,
        })
        .collect()
}

fn set_bauds(outputs: &[Output]) -> Vec<u32> {
    outputs
        .iter()
        .filter_map(|o| match o {
            Output::SetBaud(b) => Some(*b),
            _ => None,
        })
        .collect()
}

fn rmc_count(outputs: &[Output]) -> usize {
    outputs
        .iter()
        .filter(|o| matches!(o, Output::Rmc(_)))
        .count()
}

fn gga_count(outputs: &[Output]) -> usize {
    outputs
        .iter()
        .filter(|o| matches!(o, Output::Gga(_)))
        .count()
}

// ---- Sweep ----

// A parsed sentence at the first baud locks it: NoFix is published at
// 9600 and no baud change is requested. The void cold-start sentence is
// deliberately the lock evidence — locking must not require a valid fix.
#[test]
fn locks_first_baud_on_valid_nmea() {
    let mut m = PresenceMachine::new(0);
    assert_eq!(m.published(), None);
    let out = feed(&mut m, &void_rmc(), 100);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 9600)]);
    assert_eq!(set_bauds(&out), Vec::<u32>::new());
    assert_eq!(m.current_baud(), 9600);
}

// Garbage at 9600, valid NMEA at 38400: the machine requests the baud
// change at window expiry and locks the second baud. Positive control
// inside: before the window expires no SetBaud is emitted.
#[test]
fn sweeps_to_next_baud_on_garbage() {
    let mut m = PresenceMachine::new(0);
    let out = feed(&mut m, GARBAGE, 500);
    assert_eq!(
        set_bauds(&out),
        Vec::<u32>::new(),
        "window must not end early"
    );
    assert_eq!(transitions(&out), vec![]);

    let out = poll(&mut m, DETECT_WINDOW_MS);
    assert_eq!(set_bauds(&out), vec![38_400]);
    assert_eq!(m.current_baud(), 38_400);

    let out = feed(&mut m, &void_rmc(), DETECT_WINDOW_MS + 500);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 38_400)]);
}

// A garbage half-sentence at the old baud must not poison parsing at
// the new baud: a '$' start swallowed by wrong-baud noise would leave
// the parser mid-sentence. The parser is reset per window.
#[test]
fn parser_resets_across_baud_change() {
    let mut m = PresenceMachine::new(0);
    // A partial sentence start with valid-looking prefix, never finished.
    feed(&mut m, b"$GPRMC,2254", 500);
    poll(&mut m, DETECT_WINDOW_MS);
    let out = feed(&mut m, &void_rmc(), DETECT_WINDOW_MS + 500);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 38_400)]);
}

// Total silence sweeps all three bauds, then settles NoHardware
// (baud=0: none found) and parks at 9600.
#[test]
fn silence_settles_no_hardware_after_full_sweep() {
    let mut m = PresenceMachine::new(0);
    let out = poll(&mut m, DETECT_WINDOW_MS);
    assert_eq!(set_bauds(&out), vec![38_400]);
    assert_eq!(transitions(&out), vec![]);

    let out = poll(&mut m, 2 * DETECT_WINDOW_MS);
    assert_eq!(set_bauds(&out), vec![115_200]);
    assert_eq!(transitions(&out), vec![]);

    let out = poll(&mut m, 3 * DETECT_WINDOW_MS);
    assert_eq!(transitions(&out), vec![(Presence::NoHardware, 0)]);
    assert_eq!(set_bauds(&out), vec![9_600]);
    assert_eq!(m.published(), Some(Presence::NoHardware));
}

// Positive control for the silence path: a single garbage byte anywhere
// in the pass suppresses NoHardware — the pass ends in a quiet re-sweep
// instead. A second, fully silent pass then settles NoHardware once.
#[test]
fn any_activity_suppresses_no_hardware() {
    let mut m = PresenceMachine::new(0);
    feed(&mut m, &[0xff], 100);
    poll(&mut m, DETECT_WINDOW_MS);
    poll(&mut m, 2 * DETECT_WINDOW_MS);
    let out = poll(&mut m, 3 * DETECT_WINDOW_MS);
    assert_eq!(
        transitions(&out),
        vec![],
        "activity in the pass must suppress the NoHardware settle"
    );
    assert_eq!(set_bauds(&out), vec![9_600], "pass must restart, not park");

    // Second pass: fully silent → NoHardware, exactly one event.
    poll(&mut m, 4 * DETECT_WINDOW_MS);
    poll(&mut m, 5 * DETECT_WINDOW_MS);
    let out = poll(&mut m, 6 * DETECT_WINDOW_MS);
    assert_eq!(transitions(&out), vec![(Presence::NoHardware, 0)]);
}

// A UART error (framing at wrong baud) is line activity: it must
// suppress NoHardware exactly as bytes do.
#[test]
fn uart_error_counts_as_activity() {
    let mut m = PresenceMachine::new(0);
    let mut out = Vec::new();
    m.on_uart_error(100, &mut |o| out.push(o));
    poll(&mut m, DETECT_WINDOW_MS);
    poll(&mut m, 2 * DETECT_WINDOW_MS);
    let out = poll(&mut m, 3 * DETECT_WINDOW_MS);
    assert_eq!(transitions(&out), vec![]);
    assert_eq!(set_bauds(&out), vec![9_600]);
}

// NoHardware is not a dead end: bytes appearing later (receiver plugged
// in at runtime) restart the sweep, and valid NMEA then locks a baud.
#[test]
fn no_hardware_recovers_when_bytes_appear() {
    let mut m = PresenceMachine::new(0);
    poll(&mut m, DETECT_WINDOW_MS);
    poll(&mut m, 2 * DETECT_WINDOW_MS);
    poll(&mut m, 3 * DETECT_WINDOW_MS);
    assert_eq!(m.published(), Some(Presence::NoHardware));

    let t = 100 * DETECT_WINDOW_MS;
    let out = feed(&mut m, &void_rmc(), t);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 9600)]);
}

// Repeated silent passes must not re-emit NoHardware (event-channel
// spam): after the settle, later silent windows publish nothing.
#[test]
fn duplicate_no_hardware_is_suppressed() {
    let mut m = PresenceMachine::new(0);
    poll(&mut m, DETECT_WINDOW_MS);
    poll(&mut m, 2 * DETECT_WINDOW_MS);
    let first = poll(&mut m, 3 * DETECT_WINDOW_MS);
    assert_eq!(transitions(&first).len(), 1);

    // Noise wakes it into a sweep that then goes silent again.
    feed(&mut m, &[0x55], 4 * DETECT_WINDOW_MS);
    poll(&mut m, 5 * DETECT_WINDOW_MS);
    poll(&mut m, 6 * DETECT_WINDOW_MS);
    let resweep = poll(&mut m, 7 * DETECT_WINDOW_MS);
    // Pass had activity (the wake byte) → re-sweep, no settle.
    assert_eq!(transitions(&resweep), vec![]);
    poll(&mut m, 8 * DETECT_WINDOW_MS);
    poll(&mut m, 9 * DETECT_WINDOW_MS);
    let second = poll(&mut m, 10 * DETECT_WINDOW_MS);
    assert_eq!(
        transitions(&second),
        vec![],
        "re-settling NoHardware must not re-emit the event"
    );
    assert_eq!(m.published(), Some(Presence::NoHardware));
}

// Once locked, the baud sticks through silence shorter than the starve
// threshold: no window expiry, no SetBaud. (Silence past the threshold
// re-sweeps — the "Locked starve re-sweep" section below.)
#[test]
fn locked_baud_sticks() {
    let mut m = PresenceMachine::new(0);
    poll(&mut m, DETECT_WINDOW_MS); // → 38400
    let lock_t = DETECT_WINDOW_MS + 500;
    feed(&mut m, &void_rmc(), lock_t);
    assert_eq!(m.current_baud(), 38_400);

    let out = poll(&mut m, lock_t + LOCK_STARVE_RESWEEP_MS - 1);
    assert_eq!(set_bauds(&out), Vec::<u32>::new());
    assert_eq!(m.current_baud(), 38_400);
}

// ---- Fix / hysteresis ----

// A valid RMC promotes to Fix immediately and the RMC is forwarded for
// the GnssFix fold. Order: the transition precedes the RMC so consumers
// never see fix data under a stale presence.
#[test]
fn valid_rmc_promotes_to_fix() {
    let mut m = PresenceMachine::new(0);
    let out = feed(&mut m, &valid_rmc(), 100);
    assert_eq!(
        transitions(&out),
        vec![(Presence::NoFix, 9600), (Presence::Fix, 9600)]
    );
    assert_eq!(rmc_count(&out), 1);
    let trans_pos = out
        .iter()
        .position(|o| {
            matches!(
                o,
                Output::Transition {
                    state: Presence::Fix,
                    ..
                }
            )
        })
        .unwrap();
    let rmc_pos = out
        .iter()
        .position(|o| matches!(o, Output::Rmc(_)))
        .unwrap();
    assert!(
        trans_pos < rmc_pos,
        "Fix transition must precede the RMC data"
    );
}

// GGA never touches presence — even a fix-quality GGA stream leaves the
// state at NoFix. This pins the "position/timebase gating stays keyed
// to valid RMC only" contract (#166): GGA content is forwarded for the
// snapshot, nothing more.
#[test]
fn gga_never_promotes_presence() {
    let mut m = PresenceMachine::new(0);
    let mut out = feed(&mut m, &fix_gga(), 100);
    for i in 1..5 {
        out.extend(feed(&mut m, &fix_gga(), 100 + i * 1000));
    }
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 9600)]);
    assert_eq!(gga_count(&out), 5);
    assert_eq!(m.published(), Some(Presence::NoFix));
}

// A populated but not-valid RMC must not promote either (and is still
// forwarded, because the fold clears the stale time claim from it).
#[test]
fn invalid_rmc_never_promotes() {
    let mut m = PresenceMachine::new(0);
    let out = feed(&mut m, &invalid_rmc(), 100);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 9600)]);
    assert_eq!(rmc_count(&out), 1);
}

// Fix survives invalid RMCs shorter than the hold (margin flaps), and a
// valid RMC inside the hold restarts it. Positive control: the same
// stream with the refresh removed demotes (next test).
#[test]
fn fix_holds_through_brief_invalidity() {
    let mut m = PresenceMachine::new(0);
    feed(&mut m, &valid_rmc(), 1_000);
    assert_eq!(m.published(), Some(Presence::Fix));

    // Invalid RMCs at 1 Hz for just under the hold.
    let mut out = Vec::new();
    for i in 1..(FIX_HOLD_MS / 1000) {
        out.extend(feed(&mut m, &invalid_rmc(), 1_000 + i * 1_000));
    }
    assert_eq!(transitions(&out), vec![], "no demotion inside the hold");

    // A valid RMC refreshes the hold...
    feed(&mut m, &valid_rmc(), 1_000 + FIX_HOLD_MS - 500);
    // ...so invalidity keeps being tolerated well past the original
    // deadline.
    let out = feed(&mut m, &invalid_rmc(), FIX_HOLD_MS + 5_000);
    assert_eq!(transitions(&out), vec![]);
    assert_eq!(m.published(), Some(Presence::Fix));
}

// Fix demotes to NoFix once no valid RMC arrived for the full hold —
// via an invalid sentence carrying the clock forward.
#[test]
fn fix_demotes_after_hold_via_sentences() {
    let mut m = PresenceMachine::new(0);
    feed(&mut m, &valid_rmc(), 1_000);
    let out = feed(&mut m, &invalid_rmc(), 1_000 + FIX_HOLD_MS);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 9600)]);
    assert_eq!(m.published(), Some(Presence::NoFix));
}

// ...and via bare time (receiver unplugged mid-run: bytes stop
// entirely, only poll carries the clock).
#[test]
fn fix_demotes_after_hold_via_poll() {
    let mut m = PresenceMachine::new(0);
    feed(&mut m, &valid_rmc(), 1_000);
    let out = poll(&mut m, 999 + FIX_HOLD_MS);
    assert_eq!(transitions(&out), vec![], "hold must run its full length");
    let out = poll(&mut m, 1_000 + FIX_HOLD_MS);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 9600)]);
}

// After a demotion the next valid RMC re-promotes immediately —
// reacquisition must not be penalised.
#[test]
fn repromotes_after_demotion() {
    let mut m = PresenceMachine::new(0);
    feed(&mut m, &valid_rmc(), 1_000);
    poll(&mut m, 1_000 + FIX_HOLD_MS);
    assert_eq!(m.published(), Some(Presence::NoFix));
    let out = feed(&mut m, &valid_rmc(), 2_000 + FIX_HOLD_MS);
    assert_eq!(transitions(&out), vec![(Presence::Fix, 9600)]);
}

// ---- Locked starve re-sweep (#324) ----

// A locked line that stops producing sentences entirely (module
// rebooted by UBX-CFG-RST, unplugged, or reconfigured away) re-enters
// the sweep from the default baud instead of staying deaf forever.
#[test]
fn locked_resweeps_after_sentence_starvation_on_silence() {
    let mut m = PresenceMachine::new(0);
    poll(&mut m, DETECT_WINDOW_MS); // → 38400
    let lock_t = DETECT_WINDOW_MS + 500;
    feed(&mut m, &void_rmc(), lock_t);
    assert_eq!(m.current_baud(), 38_400);

    // Just inside the starve threshold: still locked, no baud change.
    let out = poll(&mut m, lock_t + LOCK_STARVE_RESWEEP_MS - 1);
    assert_eq!(set_bauds(&out), Vec::<u32>::new(), "starve must run full");

    // At the threshold: re-sweep from the sweep's first baud.
    let out = poll(&mut m, lock_t + LOCK_STARVE_RESWEEP_MS);
    assert_eq!(set_bauds(&out), vec![9_600]);
    assert_eq!(m.current_baud(), 9_600);
}

// Post-RST at a fallen-back module baud the line is not silent but
// garbage (wrong-baud mangling): starvation is keyed to parsed
// sentences, not to line activity, so garbage must also re-sweep.
#[test]
fn locked_resweeps_after_sentence_starvation_on_garbage() {
    let mut m = PresenceMachine::new(0);
    poll(&mut m, DETECT_WINDOW_MS); // → 38400
    let lock_t = DETECT_WINDOW_MS + 500;
    feed(&mut m, &void_rmc(), lock_t);

    // Garbage keeps arriving every second — activity, but no sentence.
    let mut out = Vec::new();
    let mut t = lock_t;
    while t < lock_t + LOCK_STARVE_RESWEEP_MS {
        t += 1_000;
        out.extend(feed(&mut m, GARBAGE, t));
    }
    assert_eq!(set_bauds(&out), vec![9_600]);
    assert_eq!(m.current_baud(), 9_600);
}

// Positive control: a healthy receiver without a fix (void RMC every
// second, the indoor cold-start stream) must never trigger the starve
// re-sweep — sentence flow is the health signal.
#[test]
fn sentence_flow_prevents_starve_resweep() {
    let mut m = PresenceMachine::new(0);
    let mut out = feed(&mut m, &void_rmc(), 1_000);
    for i in 1..(3 * LOCK_STARVE_RESWEEP_MS / 1_000) {
        out.extend(feed(&mut m, &void_rmc(), 1_000 + i * 1_000));
    }
    assert_eq!(set_bauds(&out), Vec::<u32>::new());
    assert_eq!(m.current_baud(), 9_600);
}

// Starvation while Fix: the hold demotes to NoFix first (10 s), the
// re-sweep follows later (15 s) — the constants keep that order, so a
// consumer never sees a re-sweep under a published Fix.
#[test]
fn fix_demotes_before_starve_resweep() {
    let mut m = PresenceMachine::new(0);
    feed(&mut m, &valid_rmc(), 1_000);
    assert_eq!(m.published(), Some(Presence::Fix));

    let out = poll(&mut m, 1_000 + FIX_HOLD_MS);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 9600)]);
    assert_eq!(
        set_bauds(&out),
        Vec::<u32>::new(),
        "no re-sweep at the hold"
    );

    let out = poll(&mut m, 1_000 + LOCK_STARVE_RESWEEP_MS);
    assert_eq!(set_bauds(&out), vec![9_600]);
    assert_eq!(m.published(), Some(Presence::NoFix));
}

// After the starve re-sweep the machine is a full citizen again: a
// sentence at the new baud re-locks (no duplicate NoFix event — dedup),
// and a valid RMC promotes to Fix at the re-locked baud.
#[test]
fn relocks_and_promotes_after_starve_resweep() {
    let mut m = PresenceMachine::new(0);
    poll(&mut m, DETECT_WINDOW_MS); // → 38400
    let lock_t = DETECT_WINDOW_MS + 500;
    feed(&mut m, &void_rmc(), lock_t);
    poll(&mut m, lock_t + LOCK_STARVE_RESWEEP_MS);
    assert_eq!(m.current_baud(), 9_600);

    let t = lock_t + LOCK_STARVE_RESWEEP_MS + 1_000;
    let out = feed(&mut m, &void_rmc(), t);
    assert_eq!(
        transitions(&out),
        vec![],
        "NoFix re-settle must not re-emit"
    );
    let out = feed(&mut m, &valid_rmc(), t + 1_000);
    assert_eq!(transitions(&out), vec![(Presence::Fix, 9_600)]);
}

// A starve re-sweep over a truly dead line ends where any silent sweep
// ends: NoHardware. The unplugged-mid-run module is fully reported.
#[test]
fn starve_resweep_to_silence_settles_no_hardware() {
    let mut m = PresenceMachine::new(0);
    feed(&mut m, &void_rmc(), 1_000);
    let resweep_t = 1_000 + LOCK_STARVE_RESWEEP_MS;
    poll(&mut m, resweep_t); // → sweep idx 0
    poll(&mut m, resweep_t + DETECT_WINDOW_MS);
    poll(&mut m, resweep_t + 2 * DETECT_WINDOW_MS);
    let out = poll(&mut m, resweep_t + 3 * DETECT_WINDOW_MS);
    assert_eq!(transitions(&out), vec![(Presence::NoHardware, 0)]);
}

// ---- GSV: satellites in view and C/N0 (#324 instrumentation) ----

/// A real u-blox GPS GSV group: three sentences, 11 satellites in view,
/// C/N0 fields as a receiver reports them while acquiring (mostly 00 for
/// untracked SVs, a handful in the 39-43 dBHz band). Best C/N0 = 43.
fn gp_gsv_group() -> Vec<Vec<u8>> {
    vec![
        nmea("GPGSV,3,1,11,03,03,111,00,04,15,270,00,06,01,010,00,13,06,292,00"),
        nmea("GPGSV,3,2,11,14,25,170,00,16,57,208,39,18,67,296,40,19,40,246,00"),
        nmea("GPGSV,3,3,11,22,42,067,42,24,14,311,43,27,05,244,00"),
    ]
}

/// A real u-blox GLONASS GSV group: two sentences, 7 in view, empty C/N0
/// fields for untracked SVs (the null the spec allows). Best C/N0 = 36.
fn gl_gsv_group() -> Vec<Vec<u8>> {
    vec![
        nmea("GLGSV,2,1,07,65,08,041,,66,49,092,31,67,42,180,36,68,03,228,"),
        nmea("GLGSV,2,2,07,73,15,321,,74,60,330,28,75,12,027,"),
    ]
}

fn feed_all(m: &mut PresenceMachine, group: &[Vec<u8>], now_ms: u64) -> Vec<Output> {
    let mut out = Vec::new();
    for s in group {
        out.extend(feed(m, s, now_ms));
    }
    out
}

/// A full GSV group yields the in-view count and the best C/N0 of the
/// whole group — the discriminator between "antenna sees sky" and
/// "antenna is deaf" while presence still reads no-fix.
#[test]
fn gsv_group_reports_sv_in_view_and_best_cno() {
    let mut m = PresenceMachine::new(0);
    feed_all(&mut m, &gp_gsv_group(), 1_000);
    assert_eq!(m.sv_in_view(), 11);
    assert_eq!(m.cno_best(), Some(43));
}

/// The first sentence of a group must not publish a count on its own:
/// the group is only meaningful once complete (the driver doc warns the
/// GSV tail can split across reads).
#[test]
fn partial_gsv_group_publishes_nothing() {
    let mut m = PresenceMachine::new(0);
    let group = gp_gsv_group();
    feed_all(&mut m, &group[..2], 1_000);
    assert_eq!(m.sv_in_view(), 0, "incomplete group must not publish");
    assert_eq!(m.cno_best(), None);

    feed_all(&mut m, &group[2..], 1_100);
    assert_eq!(m.sv_in_view(), 11);
    assert_eq!(m.cno_best(), Some(43));
}

/// A group split at arbitrary byte boundaries across UART reads — the
/// real driver case, chunks aligned to idle detection, not to sentences.
#[test]
fn gsv_group_split_across_chunks_reports_same_counts() {
    let mut m = PresenceMachine::new(0);
    let stream: Vec<u8> = gp_gsv_group().concat();
    // Split mid-sentence in two places, including inside a C/N0 field.
    for chunk in [&stream[..30], &stream[30..97], &stream[97..]] {
        feed(&mut m, chunk, 1_000);
    }
    assert_eq!(m.sv_in_view(), 11);
    assert_eq!(m.cno_best(), Some(43));
}

/// A truncated tail — the last sentence of the group cut off mid-field,
/// so it never checksums — must leave the previously committed counts
/// untouched rather than publish half a group.
#[test]
fn truncated_gsv_tail_leaves_counts_untouched() {
    let mut m = PresenceMachine::new(0);
    feed_all(&mut m, &gp_gsv_group(), 1_000);
    assert_eq!(m.sv_in_view(), 11);

    // Next cycle: 8 in view, but the group's tail is lost.
    feed(
        &mut m,
        &nmea("GPGSV,2,1,08,03,03,111,00,04,15,270,00,06,01,010,00,13,06,292,00"),
        2_000,
    );
    feed(&mut m, b"$GPGSV,2,2,08,14,25,170,00,16,5", 2_000);
    assert_eq!(m.sv_in_view(), 11, "truncated tail must not poison sv");
    assert_eq!(m.cno_best(), Some(43), "truncated tail must not poison cno");
}

/// A truncated tail before any complete group leaves the counts at their
/// "nothing measured yet" values — never a partial-group number.
#[test]
fn truncated_gsv_tail_before_any_group_publishes_nothing() {
    let mut m = PresenceMachine::new(0);
    feed(
        &mut m,
        &nmea("GPGSV,2,1,08,03,03,111,00,04,15,270,00,06,01,010,00,13,06,292,42"),
        1_000,
    );
    feed(&mut m, b"$GPGSV,2,2,08,14,25,170,00,16,5", 1_000);
    assert_eq!(m.sv_in_view(), 0);
    assert_eq!(m.cno_best(), None);
}

/// A dropped middle sentence breaks the group: the tail arrives but the
/// group is incomplete, so nothing is committed.
#[test]
fn gsv_group_with_dropped_middle_publishes_nothing() {
    let mut m = PresenceMachine::new(0);
    let group = gp_gsv_group();
    feed(&mut m, &group[0], 1_000);
    feed(&mut m, &group[2], 1_000);
    assert_eq!(m.sv_in_view(), 0);
    assert_eq!(m.cno_best(), None);
}

/// Concurrent constellations report separate groups: in-view sums across
/// talkers, best C/N0 is the maximum over all of them.
#[test]
fn gsv_counts_sum_across_constellations() {
    let mut m = PresenceMachine::new(0);
    feed_all(&mut m, &gp_gsv_group(), 1_000);
    feed_all(&mut m, &gl_gsv_group(), 1_000);
    assert_eq!(m.sv_in_view(), 18, "11 GPS + 7 GLONASS");
    assert_eq!(m.cno_best(), Some(43), "max over GPS 43 and GLONASS 36");
}

/// A later cycle replaces the previous one per talker — the counts track
/// the receiver's current view, they do not accumulate.
#[test]
fn gsv_cycle_replaces_previous_counts() {
    let mut m = PresenceMachine::new(0);
    feed_all(&mut m, &gp_gsv_group(), 1_000);
    assert_eq!(m.sv_in_view(), 11);

    feed(&mut m, &nmea("GPGSV,1,1,04,03,03,111,21"), 2_000);
    assert_eq!(m.sv_in_view(), 4);
    assert_eq!(m.cno_best(), Some(21));
}

/// A re-sweep abandons the line, so the measurement it produced goes
/// with it: stale counts from a module that is no longer talking to us
/// would read as a healthy antenna.
#[test]
fn gsv_counts_clear_on_starve_resweep() {
    let mut m = PresenceMachine::new(0);
    feed_all(&mut m, &gp_gsv_group(), 1_000);
    assert_eq!(m.sv_in_view(), 11);

    poll(&mut m, 1_000 + LOCK_STARVE_RESWEEP_MS);
    assert_eq!(m.sv_in_view(), 0);
    assert_eq!(m.cno_best(), None);
}

/// Instrumentation only (#324 scope): GSV locks a baud like any other
/// checksum-clean sentence, but publishes no fold content and never
/// promotes presence — position and timebase stay keyed to valid RMC.
#[test]
fn gsv_is_instrumentation_only() {
    let mut m = PresenceMachine::new(0);
    let out = feed_all(&mut m, &gp_gsv_group(), 1_000);
    assert_eq!(transitions(&out), vec![(Presence::NoFix, 9600)]);
    assert_eq!(rmc_count(&out), 0);
    assert_eq!(gga_count(&out), 0);
    assert_eq!(m.published(), Some(Presence::NoFix));
    assert_eq!(m.sentences_seen(), 3);
}

// ---- Event tokens ----

// The exact debug-channel tokens periculum replays grep for.
#[test]
fn event_tokens_are_stable() {
    assert_eq!(Presence::NoHardware.as_str(), "no-hardware");
    assert_eq!(Presence::NoFix.as_str(), "no-fix");
    assert_eq!(Presence::Fix.as_str(), "fix");
}

// The sweep constants the doc-comments justify: order and window/hold
// relations (window ≥ 2 sentence periods, hold ≥ a handful of them).
#[test]
fn constants_hold_their_justifications() {
    assert_eq!(BAUD_SWEEP, [9600, 38_400, 115_200]);
    // Every operand below is a compile-time constant, so these belong in const
    // blocks: clippy::assertions_on_constants rejects the runtime form, and the
    // const one fails the BUILD rather than one test run — which is what an
    // invariant over a constant should do (same move as c746bf8).
    // Window must cover ≥2 NMEA periods.
    const { assert!(DETECT_WINDOW_MS >= 2_000) };
    // Hold must ride out multi-sentence flaps.
    const { assert!(FIX_HOLD_MS >= 5_000) };
    // Fix must demote before the starve re-sweep can fire.
    const { assert!(LOCK_STARVE_RESWEEP_MS > FIX_HOLD_MS) };
}
