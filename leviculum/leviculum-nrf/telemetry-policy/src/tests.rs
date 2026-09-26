//! Behaviour tests for the telemetry send policy (Codeberg #236).
//!
//! Each test names one rule from `docs/src/concepts/telemetry.md` or from
//! the #236 UX decisions and exercises it alone. The state machine has no
//! I/O, so every one of them is a few microseconds of arithmetic — which
//! is the whole point of the policy being a crate and not a branch inside
//! the firmware's main loop.

use super::*;

/// Somewhere in Hamburg, HDOP 1.2 — a fix any profile accepts.
fn good_fix() -> Fix {
    Fix {
        latitude_e6: 53_551_086,
        longitude_e6: 9_993_682,
        hdop_e2: Some(120),
    }
}

/// Move `metres` due north of `from` (latitude only, so the expected
/// distance is exact under the model).
fn north_of(from: Fix, metres: i64) -> Fix {
    Fix {
        latitude_e6: from.latitude_e6 + (metres * 1_000_000 / METRES_PER_DEGREE_LAT) as i32,
        ..from
    }
}

/// A node that HAS a position source — a GNSS build, or a fixed position
/// set. That is the second clause of the send condition (Lew, 2026-08-30),
/// and every cadence test below is about a node past it; the nodes without
/// one have their own section at the end.
///
/// Deliberately not the default of [`SendPolicy::new`]: a node that has
/// never said it can answer "where am I" has not got a position source, and
/// a policy that assumed one would report from a board that cannot.
fn reporting_node() -> SendPolicy {
    let mut p = SendPolicy::new();
    p.set_position_source(true);
    p
}

/// A policy already past the target dance and past its settle window,
/// with one report on the clock at `t`.
fn ready(profile: Profile, t: u64) -> SendPolicy {
    let mut p = reporting_node();
    p.set_target(profile, true);
    assert_eq!(p.poll(t, Some(good_fix())), Some(ReportReason::Immediate));
    p.note_sent(t, Some(good_fix()));
    p
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[test]
fn station_is_the_default_profile() {
    assert_eq!(Profile::DEFAULT, Profile::Station);
    assert_eq!(SendPolicy::new().profile(), Profile::Station);
}

#[test]
fn profile_ids_match_the_envelope_allocation() {
    assert_eq!(Profile::Tracker.to_wire(), 0x01);
    assert_eq!(Profile::Station.to_wire(), 0x02);
    assert_eq!(Profile::from_wire(0x01), Some(Profile::Tracker));
    assert_eq!(Profile::from_wire(0x02), Some(Profile::Station));
    // 0x00 is the clear encoding, not a profile. Pinned to the literal,
    // not to the constant: this crate cannot depend on
    // `leviculum_core::envelope`, so the two allocations are held
    // together by this assertion here and a compile-time one in the
    // firmware glue that sees both.
    assert_eq!(PROFILE_ID_OFF, 0x00);
    assert_eq!(PROFILE_ID_TRACKER, 0x01);
    assert_eq!(PROFILE_ID_STATION, 0x02);
    assert_eq!(Profile::from_wire(PROFILE_ID_OFF), None);
    assert_eq!(Profile::from_wire(0x7F), None);
}

#[test]
fn the_station_profile_has_no_movement_path() {
    // What makes a station a station: distance never justifies a report.
    assert_eq!(PolicyParams::STATION.min_distance_m, 0);
    assert_eq!(
        PolicyParams::STATION.min_interval_ms,
        PolicyParams::STATION.max_interval_ms
    );
}

#[test]
fn setting_a_target_loads_that_profiles_parameters() {
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    assert_eq!(p.params(), PolicyParams::TRACKER);
    p.set_target(Profile::Station, true);
    assert_eq!(p.params(), PolicyParams::STATION);
}

// ---------------------------------------------------------------------------
// Target lifecycle — the hash-only UX decision
// ---------------------------------------------------------------------------

#[test]
fn no_target_is_the_default_and_sends_nothing() {
    let mut p = reporting_node();
    assert_eq!(p.state(), TargetState::Off);
    assert_eq!(p.poll(0, Some(good_fix())), None);
    assert_eq!(p.poll(u64::MAX / 2, Some(good_fix())), None);
}

#[test]
fn a_hash_only_target_waits_for_the_key_and_says_so() {
    let mut p = reporting_node();
    assert_eq!(
        p.set_target(Profile::Station, false),
        TargetState::AwaitingKey
    );
    assert_eq!(p.state().as_str(), "awaiting-key");
    // Nothing can be encrypted, so nothing is due — at any time.
    assert_eq!(p.poll(0, Some(good_fix())), None);
    assert_eq!(p.poll(10 * 60 * 60_000, Some(good_fix())), None);
}

#[test]
fn the_immediate_report_fires_when_the_key_arrives_not_when_the_target_is_set() {
    let mut p = reporting_node();
    p.set_target(Profile::Station, false);
    assert_eq!(p.poll(1_000, Some(good_fix())), None);

    assert!(p.note_key_available());
    assert_eq!(p.state(), TargetState::Ready);
    assert_eq!(
        p.poll(1_100, Some(good_fix())),
        Some(ReportReason::Immediate)
    );
}

#[test]
fn a_target_set_with_a_known_key_is_ready_at_once() {
    let mut p = reporting_node();
    assert_eq!(p.set_target(Profile::Station, true), TargetState::Ready);
    assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
}

#[test]
fn the_immediate_report_survives_until_it_is_actually_sent() {
    // A node with no path must retry, not go quiet for an hour.
    let mut p = ready(Profile::Station, 0);
    p.set_target(Profile::Station, true);
    for t in [0, 5_000, 60_000, 600_000] {
        assert_eq!(p.poll(t, None), Some(ReportReason::Immediate), "at t={t}");
    }
    p.note_sent(600_000, None);
    assert_eq!(p.poll(600_001, None), None);
}

#[test]
fn key_arrival_on_an_already_ready_target_changes_nothing() {
    let mut p = ready(Profile::Station, 0);
    assert!(!p.note_key_available());
    assert_eq!(p.poll(1_000, Some(good_fix())), None);
}

#[test]
fn clearing_the_target_switches_telemetry_off() {
    let mut p = ready(Profile::Tracker, 0);
    p.clear_target();
    assert_eq!(p.state(), TargetState::Off);
    assert_eq!(p.state().as_str(), "off");
    // Not even the heartbeat, which is the whole point of "the target is
    // the switch".
    assert_eq!(p.poll(10 * 60 * 60_000, Some(good_fix())), None);
}

#[test]
fn a_cleared_target_does_not_leave_cadence_state_for_the_next_one() {
    let mut p = ready(Profile::Tracker, 0);
    p.note_sent(0, Some(good_fix()));
    p.clear_target();
    // A new target, key already known: the first thing owed is the
    // immediate report, not a movement report against the old
    // recipient's reference position.
    p.set_target(Profile::Tracker, true);
    assert_eq!(
        p.poll(1_000, Some(north_of(good_fix(), 500))),
        Some(ReportReason::Immediate)
    );
}

#[test]
fn losing_the_key_returns_to_awaiting_and_disarms() {
    let mut p = ready(Profile::Station, 0);
    p.set_target(Profile::Station, true);
    assert!(p.note_key_lost());
    assert_eq!(p.state(), TargetState::AwaitingKey);
    assert_eq!(p.poll(1_000, Some(good_fix())), None);
    // And it re-arms when the key comes back.
    assert!(p.note_key_available());
    assert_eq!(
        p.poll(2_000, Some(good_fix())),
        Some(ReportReason::Immediate)
    );
}

// ---------------------------------------------------------------------------
// Heartbeat — the maximum interval
// ---------------------------------------------------------------------------

#[test]
fn the_heartbeat_fires_exactly_at_the_maximum_interval() {
    let max = PolicyParams::STATION.max_interval_ms;
    let mut p = ready(Profile::Station, 0);
    assert_eq!(p.poll(max - 1, None), None);
    assert_eq!(p.poll(max, None), Some(ReportReason::Heartbeat));
}

#[test]
fn the_heartbeat_fires_without_any_position() {
    // "no fix, no position" does not mean "no fix, no message": a
    // heartbeat still carries battery and time.
    let mut p = ready(Profile::Station, 0);
    let max = PolicyParams::STATION.max_interval_ms;
    assert_eq!(p.poll(max, None), Some(ReportReason::Heartbeat));
}

#[test]
fn the_heartbeat_restarts_from_the_report_that_was_actually_sent() {
    let max = PolicyParams::STATION.max_interval_ms;
    let mut p = ready(Profile::Station, 0);
    assert_eq!(p.poll(max, None), Some(ReportReason::Heartbeat));
    // Radio refused it: the cadence must not advance.
    assert_eq!(p.poll(max + 1_000, None), Some(ReportReason::Heartbeat));
    p.note_sent(max + 1_000, None);
    assert_eq!(p.poll(max + 2_000, None), None);
    assert_eq!(p.poll(2 * max + 1_000, None), Some(ReportReason::Heartbeat));
}

// ---------------------------------------------------------------------------
// Movement — distance and the minimum interval
// ---------------------------------------------------------------------------

#[test]
fn a_tracker_reports_after_moving_far_enough() {
    let mut p = ready(Profile::Tracker, 0);
    let settle = PolicyParams::TRACKER.settle_ms;
    let t = settle + PolicyParams::TRACKER.min_interval_ms;
    let moved = north_of(good_fix(), 120);
    assert_eq!(p.poll(t, Some(moved)), Some(ReportReason::Movement));
}

#[test]
fn a_tracker_that_has_not_moved_far_enough_stays_quiet() {
    let mut p = ready(Profile::Tracker, 0);
    let t = PolicyParams::TRACKER.settle_ms + PolicyParams::TRACKER.min_interval_ms;
    // 20 m against a 50 m gate.
    assert_eq!(p.poll(t, Some(north_of(good_fix(), 20))), None);
}

#[test]
fn the_minimum_interval_holds_a_movement_report_back() {
    let mut p = ready(Profile::Tracker, 0);
    // Settle out of the way, so this test is about the interval floor
    // alone: the tracker preset happens to set the two to the same 60 s
    // and a test that cannot tell them apart proves neither.
    p.set_params(PolicyParams {
        settle_ms: 0,
        ..PolicyParams::TRACKER
    });
    let far = north_of(good_fix(), 500);
    let min = PolicyParams::TRACKER.min_interval_ms;
    assert_eq!(p.poll(min - 1, Some(far)), None);
    assert_eq!(p.poll(min, Some(far)), Some(ReportReason::Movement));
}

#[test]
fn distance_is_measured_against_the_last_reported_position_not_the_last_seen_one() {
    let mut p = ready(Profile::Tracker, 0);
    let base = PolicyParams::TRACKER.settle_ms + PolicyParams::TRACKER.min_interval_ms;
    // Crawl 20 m at a time: no single step passes the gate, but the
    // third step is 60 m from the reported position and must report.
    assert_eq!(p.poll(base, Some(north_of(good_fix(), 20))), None);
    assert_eq!(p.poll(base + 1_000, Some(north_of(good_fix(), 40))), None);
    assert_eq!(
        p.poll(base + 2_000, Some(north_of(good_fix(), 60))),
        Some(ReportReason::Movement)
    );
}

#[test]
fn a_station_never_reports_on_movement() {
    let mut p = ready(Profile::Station, 0);
    let far = north_of(good_fix(), 100_000);
    // Anywhere short of the heartbeat, however far it has been dragged.
    assert_eq!(p.poll(PolicyParams::STATION.settle_ms + 1, Some(far)), None);
    assert_eq!(
        p.poll(PolicyParams::STATION.max_interval_ms - 1, Some(far)),
        None
    );
    // And what does fire at the heartbeat is the heartbeat, not movement.
    assert_eq!(
        p.poll(PolicyParams::STATION.max_interval_ms, Some(far)),
        Some(ReportReason::Heartbeat)
    );
}

// ---------------------------------------------------------------------------
// Settle
// ---------------------------------------------------------------------------

#[test]
fn the_settle_window_suppresses_the_first_movement_report() {
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    // Immediate report at t=0 with no fix at all, so the settle anchor
    // is the first usable fix that follows.
    assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
    p.note_sent(0, None);

    let settle = PolicyParams::TRACKER.settle_ms;
    let min = PolicyParams::TRACKER.min_interval_ms;
    // First usable fix arrives at min (interval floor already clear).
    assert_eq!(p.poll(min, Some(good_fix())), None, "inside settle");
    assert_eq!(
        p.poll(min + settle - 1, Some(north_of(good_fix(), 500))),
        None,
        "still inside settle"
    );
    assert_eq!(
        p.poll(min + settle, Some(north_of(good_fix(), 500))),
        Some(ReportReason::Movement)
    );
}

#[test]
fn the_settle_window_does_not_hold_back_the_heartbeat() {
    // A cold start must still prove it is alive.
    let mut p = reporting_node();
    p.set_target(Profile::Station, true);
    assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
    p.note_sent(0, None);
    let max = PolicyParams::STATION.max_interval_ms;
    // First usable fix lands one millisecond before the heartbeat, so
    // the settle window is wide open when it fires.
    assert_eq!(p.poll(max - 1, Some(good_fix())), None);
    assert_eq!(p.poll(max, Some(good_fix())), Some(ReportReason::Heartbeat));
}

// ---------------------------------------------------------------------------
// Accuracy threshold
// ---------------------------------------------------------------------------

#[test]
fn a_fix_worse_than_the_threshold_is_not_a_position() {
    let p = ready(Profile::Tracker, 0);
    let bad = Fix {
        hdop_e2: Some(PolicyParams::TRACKER.max_hdop_e2 + 1),
        ..good_fix()
    };
    assert!(!p.position_is_reportable(bad));
    let edge = Fix {
        hdop_e2: Some(PolicyParams::TRACKER.max_hdop_e2),
        ..good_fix()
    };
    assert!(p.position_is_reportable(edge));
}

#[test]
fn a_fix_without_an_accuracy_number_is_refused() {
    // "We do not know how good this is" is not "it is good".
    let p = ready(Profile::Tracker, 0);
    assert!(!p.position_is_reportable(Fix {
        hdop_e2: None,
        ..good_fix()
    }));
}

#[test]
fn an_inaccurate_fix_neither_reports_nor_moves_the_reference() {
    let mut p = ready(Profile::Tracker, 0);
    let base = PolicyParams::TRACKER.settle_ms + PolicyParams::TRACKER.min_interval_ms;
    let far_but_bad = Fix {
        hdop_e2: Some(900),
        ..north_of(good_fix(), 500)
    };
    assert_eq!(p.poll(base, Some(far_but_bad)), None);
    // The reference is still the reported position, so a good fix 20 m
    // from it is still under the gate.
    assert_eq!(p.poll(base + 1_000, Some(north_of(good_fix(), 20))), None);
}

#[test]
fn a_bad_fix_does_not_start_the_settle_window() {
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
    p.note_sent(0, None);
    let settle = PolicyParams::TRACKER.settle_ms;
    let min = PolicyParams::TRACKER.min_interval_ms;
    // A stream of unusable fixes across the whole would-be settle window.
    for t in [min, min + settle / 2, min + settle] {
        assert_eq!(
            p.poll(
                t,
                Some(Fix {
                    hdop_e2: Some(900),
                    ..good_fix()
                })
            ),
            None
        );
    }
    // The first usable fix only now starts settling.
    let first_good = min + settle + 1;
    assert_eq!(p.poll(first_good, Some(good_fix())), None);
    assert_eq!(
        p.poll(first_good + settle - 1, Some(north_of(good_fix(), 500))),
        None
    );
    assert_eq!(
        p.poll(first_good + settle, Some(north_of(good_fix(), 500))),
        Some(ReportReason::Movement)
    );
}

// ---------------------------------------------------------------------------
// Distance arithmetic
// ---------------------------------------------------------------------------

#[test]
fn one_degree_of_latitude_is_one_degree_of_latitude() {
    let a = Fix {
        latitude_e6: 0,
        longitude_e6: 0,
        hdop_e2: Some(100),
    };
    let b = Fix {
        latitude_e6: 1_000_000,
        ..a
    };
    assert!(moved_at_least(a, b, 111_000));
    assert!(!moved_at_least(a, b, 112_000));
}

#[test]
fn longitude_shrinks_with_latitude() {
    // 0.001° of longitude is ~111 m at the equator and ~66 m at 53°N.
    let equator = Fix {
        latitude_e6: 0,
        longitude_e6: 0,
        hdop_e2: Some(100),
    };
    let equator_east = Fix {
        longitude_e6: 1_000,
        ..equator
    };
    assert!(moved_at_least(equator, equator_east, 110));
    assert!(!moved_at_least(equator, equator_east, 112));

    let north = Fix {
        latitude_e6: 53_000_000,
        longitude_e6: 0,
        hdop_e2: Some(100),
    };
    let north_east = Fix {
        longitude_e6: 1_000,
        ..north
    };
    assert!(moved_at_least(north, north_east, 66));
    assert!(!moved_at_least(north, north_east, 68));
}

#[test]
fn the_antimeridian_is_one_step_wide_not_three_hundred_and_fifty_nine_degrees() {
    let west = Fix {
        latitude_e6: 0,
        longitude_e6: -179_999_500,
        hdop_e2: Some(100),
    };
    let east = Fix {
        longitude_e6: 179_999_500,
        ..west
    };
    // 0.001° apart across the line: ~111 m, not ~40 000 km.
    assert!(moved_at_least(west, east, 110));
    assert!(!moved_at_least(west, east, 120));
}

#[test]
fn the_poles_do_not_overflow_or_divide_by_zero() {
    let np = Fix {
        latitude_e6: 90_000_000,
        longitude_e6: 0,
        hdop_e2: Some(100),
    };
    let sp = Fix {
        latitude_e6: -90_000_000,
        longitude_e6: 180_000_000,
        hdop_e2: Some(100),
    };
    // Pole to pole is ~20 000 km; the point is that it answers at all.
    assert!(moved_at_least(np, sp, 1_000_000));
    // Two points on the pole, a degree of longitude apart, are the same
    // place: cos(90°) is zero.
    let np_east = Fix {
        longitude_e6: 1_000_000,
        ..np
    };
    assert!(!moved_at_least(np, np_east, 1));
}

#[test]
fn the_cosine_table_tracks_the_real_cosine() {
    // Positive control for the table itself: a table filled with the
    // wrong constant would still pass every distance test that only
    // compares against itself.
    for deg in 0..=90u32 {
        let want = (deg as f64).to_radians().cos();
        let got = cos_lat_q15(deg as i32 * 1_000_000) as f64 / 32768.0;
        assert!(
            (want - got).abs() < 1e-4,
            "cos({deg}) table {got} vs real {want}"
        );
    }
    // And halfway between entries, where the interpolation lives.
    let want = 53.5f64.to_radians().cos();
    let got = cos_lat_q15(53_500_000) as f64 / 32768.0;
    assert!((want - got).abs() < 1e-4, "cos(53.5) {got} vs {want}");
}

// ---------------------------------------------------------------------------
// Defensive paths
// ---------------------------------------------------------------------------

#[test]
fn a_ready_target_with_nothing_armed_starts_its_clock_at_the_first_poll() {
    // Reachable only if a caller never confirms the immediate report and
    // then disarms it by hand; kept so the heartbeat cannot fire off a
    // zero timestamp inherited from boot.
    let mut p = reporting_node();
    p.set_target(Profile::Station, true);
    p.note_sent(0, None);
    p.clear_target();
    p.set_target(Profile::Station, true);
    p.note_sent(1_000_000, None);
    assert_eq!(p.poll(1_000_001, None), None);
}

#[test]
fn expert_parameters_override_the_profile_preset() {
    let mut p = reporting_node();
    p.set_target(Profile::Station, true);
    p.note_sent(0, None);
    p.set_params(PolicyParams {
        max_interval_ms: 5_000,
        ..PolicyParams::STATION
    });
    assert_eq!(p.poll(4_999, None), None);
    assert_eq!(p.poll(5_000, None), Some(ReportReason::Heartbeat));
}

// ---------------------------------------------------------------------------
// The control-frame chain: profile id in, target state out
// ---------------------------------------------------------------------------

#[test]
fn a_hash_only_frame_lands_in_awaiting_key_and_sends_nothing() {
    let mut p = reporting_node();
    let command = command_from_wire(PROFILE_ID_STATION);
    assert_eq!(command, TargetCommand::Set(Profile::Station));
    assert_eq!(
        p.apply(command, false),
        TargetOutcome::Set(TargetState::AwaitingKey)
    );
    assert_eq!(p.state().as_str(), "awaiting-key");
    assert_eq!(p.poll(0, Some(good_fix())), None);
    assert_eq!(p.poll(24 * 60 * 60_000, Some(good_fix())), None);
}

#[test]
fn the_key_arriving_moves_it_to_ready_and_owes_one_report() {
    let mut p = reporting_node();
    p.apply(command_from_wire(PROFILE_ID_STATION), false);
    assert!(p.note_key_available());
    assert_eq!(p.state().as_str(), "ready");
    assert_eq!(p.poll(1_000, None), Some(ReportReason::Immediate));
}

#[test]
fn a_clear_frame_switches_telemetry_off() {
    let mut p = reporting_node();
    p.apply(command_from_wire(PROFILE_ID_TRACKER), true);
    p.note_sent(0, None);
    assert_eq!(command_from_wire(PROFILE_ID_OFF), TargetCommand::Clear);
    assert_eq!(
        p.apply(command_from_wire(PROFILE_ID_OFF), true),
        TargetOutcome::Cleared
    );
    assert_eq!(p.state(), TargetState::Off);
    assert_eq!(p.poll(24 * 60 * 60_000, Some(good_fix())), None);
}

#[test]
fn an_unknown_profile_id_runs_the_default_cadence_rather_than_losing_the_target() {
    let mut p = reporting_node();
    assert_eq!(
        command_from_wire(0x7F),
        TargetCommand::Set(Profile::DEFAULT)
    );
    assert_eq!(
        p.apply(command_from_wire(0x7F), true),
        TargetOutcome::Set(TargetState::Ready)
    );
    assert_eq!(p.profile(), Profile::DEFAULT);
    assert_eq!(p.params(), Profile::DEFAULT.params());
}

#[test]
fn a_clear_frame_on_a_node_that_had_no_target_is_still_off() {
    let mut p = reporting_node();
    assert_eq!(
        p.apply(command_from_wire(PROFILE_ID_OFF), false),
        TargetOutcome::Cleared
    );
    assert_eq!(p.state(), TargetState::Off);
}

// ---------------------------------------------------------------------------
// Dispatch settlement (#344): a report that was not sent is not "sent"
// ---------------------------------------------------------------------------

#[test]
fn a_lost_dispatch_leaves_the_cadence_unconsumed() {
    let t = 10_000;
    let mut p = ready(Profile::Station, t);
    let heartbeat = Profile::Station.params().max_interval_ms;

    // The heartbeat comes due and the report is built and handed over.
    let due = t + heartbeat;
    assert_eq!(p.poll(due, None), Some(ReportReason::Heartbeat));
    p.note_emitted(due, None);
    assert!(p.has_pending_report());

    // The dispatch loses it: full outbound queue, no interface, IFAC
    // failure — the reporter is not told which, only that it went nowhere.
    assert!(!p.note_dispatch(false));
    assert!(!p.has_pending_report());

    // The report is still owed — not deferred to another whole heartbeat
    // — but it is owed no sooner than the attempt floor allows. The next
    // few ticks are silent and the tick at the floor emits.
    let floor = Profile::Station.params().min_interval_ms;
    assert_eq!(p.poll(due + 5_000, None), None);
    assert_eq!(
        p.poll(due + floor, None),
        Some(ReportReason::Heartbeat),
        "a lost report consumed the cadence"
    );
}

#[test]
fn a_lost_dispatch_does_not_consume_an_armed_immediate_report() {
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    assert_eq!(p.poll(0, Some(good_fix())), Some(ReportReason::Immediate));
    p.note_emitted(0, Some(good_fix()));
    assert!(!p.note_dispatch(false));
    // The immediate is the one a user pressed a button for. Losing it to a
    // full queue must not silently disarm it — it is still armed once the
    // attempt floor has passed, and still armed as Immediate, not demoted
    // to whatever the cadence would have produced.
    assert_eq!(p.poll(1_000, Some(good_fix())), None);
    assert_eq!(
        p.poll(Profile::Tracker.params().min_interval_ms, Some(good_fix())),
        Some(ReportReason::Immediate)
    );
}

#[test]
fn a_lost_dispatch_does_not_move_the_movement_reference() {
    let t = 0;
    let mut p = ready(Profile::Tracker, t);
    let params = Profile::Tracker.params();
    let moved = north_of(good_fix(), 200);
    let when = t + params.min_interval_ms + params.settle_ms;
    assert_eq!(p.poll(when, Some(moved)), Some(ReportReason::Movement));
    p.note_emitted(when, Some(moved));
    assert!(!p.note_dispatch(false));
    // The position never went out, so the node has still not reported from
    // here: the same movement is still a movement.
    assert_eq!(
        p.poll(when + params.min_interval_ms, Some(moved)),
        Some(ReportReason::Movement)
    );
}

/// Control: the success path must consume the cadence, exactly once.
///
/// Without this, a "fix" that simply never marks anything sent passes
/// every test above and floods the mesh with a report per tick.
#[test]
fn control_a_delivered_dispatch_consumes_the_cadence_exactly_once() {
    let t = 10_000;
    let mut p = ready(Profile::Station, t);
    let heartbeat = Profile::Station.params().max_interval_ms;
    let due = t + heartbeat;

    assert_eq!(p.poll(due, None), Some(ReportReason::Heartbeat));
    p.note_emitted(due, None);
    assert!(
        p.note_dispatch(true),
        "a clean dispatch did not count as sent"
    );

    // Consumed: no report until the next heartbeat is due.
    assert_eq!(p.poll(due + 5_000, None), None);
    assert_eq!(p.poll(due + heartbeat - 1, None), None);
    assert_eq!(p.poll(due + heartbeat, None), Some(ReportReason::Heartbeat));
}

/// Control: settling twice must not count a second report. A double
/// settle is what a caller that dispatches an announce and a report in
/// two batches would produce.
#[test]
fn control_settling_twice_counts_one_report() {
    let t = 10_000;
    let mut p = ready(Profile::Station, t);
    let due = t + Profile::Station.params().max_interval_ms;
    assert_eq!(p.poll(due, None), Some(ReportReason::Heartbeat));
    p.note_emitted(due, None);
    assert!(p.note_dispatch(true));
    assert!(
        !p.note_dispatch(true),
        "a settle with nothing pending claimed a report"
    );
    assert!(!p.note_dispatch(false));
    assert_eq!(p.poll(due + 5_000, None), None);
}

/// Control: a settle for a report that was never emitted counts nothing.
#[test]
fn control_settling_without_an_emitted_report_counts_nothing() {
    let t = 10_000;
    let mut p = ready(Profile::Station, t);
    assert!(!p.note_dispatch(true));
    assert!(!p.has_pending_report());
    // The cadence is untouched: still no report due before the heartbeat.
    assert_eq!(p.poll(t + 5_000, None), None);
}

/// The cadence anchors at emission, not at settlement: a dispatch that
/// takes a moment to confirm must not stretch the next interval.
#[test]
fn the_cadence_anchors_at_emission_not_at_settlement() {
    let t = 10_000;
    let mut p = ready(Profile::Station, t);
    let heartbeat = Profile::Station.params().max_interval_ms;
    let due = t + heartbeat;
    assert_eq!(p.poll(due, None), Some(ReportReason::Heartbeat));
    p.note_emitted(due, None);
    assert!(p.note_dispatch(true));
    // Next heartbeat measured from `due`, not from whenever the settle ran.
    assert_eq!(p.poll(due + heartbeat, None), Some(ReportReason::Heartbeat));
}

// ---------------------------------------------------------------------------
// The attempt floor (#344): a failed send must not shorten the cadence
// ---------------------------------------------------------------------------

#[test]
fn two_failed_dispatches_cannot_emit_closer_together_than_the_minimum_interval() {
    let params = Profile::Tracker.params();
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);

    // The first attempt: the one report a newly usable target owes. It is
    // handed over and the dispatch loses it.
    assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
    p.note_emitted(0, None);
    assert!(!p.note_dispatch(false));

    // The main loop keeps turning. None of those turns may emit: the
    // reading is not consumed, but the *attempt* is held to the same floor
    // a successful report is held to. Without this the node re-emits at
    // the tick rate — 1.3 s of airtime every 6.5 s, measured on the bench.
    for t in (1_000..params.min_interval_ms).step_by(1_000) {
        assert_eq!(
            p.poll(t, None),
            None,
            "re-emitted {t} ms after a failed send, floor is {} ms",
            params.min_interval_ms
        );
    }

    // At the floor it tries again, and the reason is still the one it owed.
    assert_eq!(
        p.poll(params.min_interval_ms, None),
        Some(ReportReason::Immediate)
    );
}

/// Control for the property `2c9d8ac` established: a failed dispatch still
/// does not consume the reading. A "fix" that simply consumed it on
/// failure would pass the test above and reintroduce the defect.
#[test]
fn control_a_failed_dispatch_still_does_not_consume_the_reading() {
    let params = Profile::Tracker.params();
    let mut p = ready(Profile::Tracker, 0);
    let moved = north_of(good_fix(), 200);
    let when = params.min_interval_ms + params.settle_ms;

    assert_eq!(p.poll(when, Some(moved)), Some(ReportReason::Movement));
    p.note_emitted(when, Some(moved));
    assert!(!p.note_dispatch(false));

    // Held back by the floor rather than dropped...
    assert_eq!(p.poll(when + params.min_interval_ms - 1, Some(moved)), None);
    // ...and when the floor opens, the same movement is still a movement:
    // the position never went out, so the reference never moved with it.
    assert_eq!(
        p.poll(when + params.min_interval_ms, Some(moved)),
        Some(ReportReason::Movement)
    );
}

/// Control: the immediate report of a newly usable target is not held back.
///
/// The floor is a floor *between emissions*; a node that has emitted
/// nothing has nothing to be held back from, or a newly learned target
/// would wait a whole interval for its first reading.
#[test]
fn control_the_immediate_report_is_not_held_back_by_the_attempt_floor() {
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    assert_eq!(p.poll(0, Some(good_fix())), Some(ReportReason::Immediate));

    // Same on the hash-only path, where the key — and with it the arming —
    // arrives long after the target was set.
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, false);
    assert_eq!(p.poll(5_000, Some(good_fix())), None);
    assert!(p.note_key_available());
    assert_eq!(
        p.poll(5_001, Some(good_fix())),
        Some(ReportReason::Immediate)
    );
}

/// The floor is the policy's own `min_interval_ms` and not a second
/// constant beside it: change the parameter and the floor moves with it.
#[test]
fn the_attempt_floor_is_the_policys_own_minimum_interval() {
    let expert = PolicyParams {
        min_interval_ms: 5_000,
        ..Profile::Tracker.params()
    };
    for params in [Profile::Tracker.params(), Profile::Station.params(), expert] {
        let floor = params.min_interval_ms;
        let mut p = reporting_node();
        p.set_target(Profile::Tracker, true);
        p.set_params(params);

        assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
        p.note_emitted(0, None);
        assert!(!p.note_dispatch(false));

        assert_eq!(
            p.poll(floor - 1, None),
            None,
            "emitted below a {floor} ms floor"
        );
        assert_eq!(
            p.poll(floor, None),
            Some(ReportReason::Immediate),
            "still held at a {floor} ms floor"
        );
    }
}

/// A target change does not reset the floor.
///
/// Airtime is airtime whoever the recipient is, and an exception here is
/// an escape hatch: a host that re-sends its target frame on a timer would
/// drive exactly the storm the floor exists to stop. The cost is bounded
/// and legible — an operator who re-targets within the floor sees the
/// report withheld and then sent, at most one interval later.
#[test]
fn a_new_target_does_not_reset_the_attempt_floor() {
    let floor = Profile::Tracker.params().min_interval_ms;
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
    p.note_emitted(0, None);
    assert!(!p.note_dispatch(false));

    p.set_target(Profile::Tracker, true);
    assert_eq!(p.poll(1_000, None), None);
    assert_eq!(p.poll(floor, None), Some(ReportReason::Immediate));
}

/// A second emission without a settle in between keeps the later report:
/// the earlier one is gone regardless, and the cadence belongs to the one
/// actually in flight.
#[test]
fn a_second_emission_supersedes_an_unsettled_one() {
    let t = 10_000;
    let mut p = ready(Profile::Station, t);
    let due = t + Profile::Station.params().max_interval_ms;
    assert_eq!(p.poll(due, None), Some(ReportReason::Heartbeat));
    p.note_emitted(due, None);
    p.note_emitted(due + 5_000, None);
    assert!(p.note_dispatch(true));
    // Anchored at the later emission.
    assert_eq!(
        p.poll(
            due + 5_000 + Profile::Station.params().max_interval_ms - 1,
            None
        ),
        None
    );
    assert_eq!(
        p.poll(
            due + 5_000 + Profile::Station.params().max_interval_ms,
            None
        ),
        Some(ReportReason::Heartbeat)
    );
}

// ---------------------------------------------------------------------------
// #348: a report that went out counts as gone out
// ---------------------------------------------------------------------------
//
// The board of #348: telemetry over LoRa, BLE advertised with no phone
// attached. Every dispatch carries the announce broadcast and the report,
// LoRa takes both, and the BLE queue refuses the broadcast with
// `BufferFull`. The dispatch is therefore not *clean* — and the call site
// used to read "not clean" as "not emitted".

/// The interface the report's own frame was addressed to.
const REPORT_IFACE: usize = 1;
/// The interface that refuses everything for as long as no phone is
/// attached. It carried none of this report's frames.
const REFUSING_IFACE: usize = 2;

/// How often the firmware asks the policy whether a report is due.
const TICK_MS: u64 = 5_000;

/// The route the core chose for one report: its single frame, on the
/// interface the path table named.
fn report_route() -> EmissionRoute {
    let mut route = EmissionRoute::new();
    route.add(REPORT_IFACE);
    route
}

/// A stationary `TRACKER` node with no fix, polled on the firmware's tick,
/// whose every dispatch loses something on `losses`. Returns the times at
/// which it emitted a report.
fn emission_times(window_ms: u64, losses: &[usize]) -> Vec<u64> {
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    let mut times = Vec::new();
    let mut t = 0;
    while t <= window_ms {
        if p.poll(t, None).is_some() {
            p.note_emitted(t, None);
            p.note_dispatch(report_route().went_out(losses.iter().copied()));
            times.push(t);
        }
        t += TICK_MS;
    }
    times
}

/// The reproducer. A dispatch that lost a frame on an interface this
/// report never used is still an emission, so the cadence advances and a
/// stationary tracker reports on its heartbeat — not on the attempt floor,
/// which is the storm #348 measured.
#[test]
fn a_loss_on_an_interface_the_report_did_not_use_is_still_an_emission() {
    let heartbeat = Profile::Tracker.params().max_interval_ms;
    let times = emission_times(2 * heartbeat, &[REFUSING_IFACE]);
    assert_eq!(
        times,
        vec![0, heartbeat, 2 * heartbeat],
        "a stationary TRACKER must emit on the heartbeat, not on the attempt floor"
    );
}

/// Control, and the one that keeps the fix honest: a dispatch that placed
/// nothing anywhere is not an emission, so a board whose radio is wedged
/// still retries — at the attempt floor, which is what the floor is for.
#[test]
fn control_a_dispatch_that_placed_nothing_is_not_an_emission() {
    let floor = Profile::Tracker.params().min_interval_ms;
    let times = emission_times(3 * floor, &[REPORT_IFACE, REFUSING_IFACE]);
    assert_eq!(times, vec![0, floor, 2 * floor, 3 * floor]);
}

/// Control: the report's own interface refusing it is not an emission even
/// when every other interface in the dispatch was happy.
#[test]
fn control_a_loss_on_the_reports_own_interface_is_not_an_emission() {
    assert!(!report_route().went_out([REPORT_IFACE]));
}

/// A dispatch that lost nothing at all is an emission, which is the case
/// that must not regress while the predicate is loosened.
#[test]
fn a_dispatch_that_lost_nothing_is_an_emission() {
    assert!(report_route().went_out([]));
}

/// A report that never became a frame on any interface is not an emission,
/// whatever the dispatch did or did not lose.
#[test]
fn a_report_addressed_nowhere_is_not_an_emission() {
    assert!(EmissionRoute::new().is_empty());
    assert!(!EmissionRoute::new().went_out([]));
    assert!(!EmissionRoute::new().went_out([REFUSING_IFACE]));
}

/// An interface id no bit can hold falls back to the conservative answer:
/// every loss in the dispatch counts as this report's.
#[test]
fn an_interface_id_beyond_the_mask_treats_every_loss_as_its_own() {
    let mut route = EmissionRoute::new();
    route.add(u32::BITS as usize + 7);
    assert!(!route.is_empty());
    assert!(route.went_out([]));
    assert!(!route.went_out([REFUSING_IFACE]));
}

/// Duplicate and out-of-order loss entries decide the same way: the caller
/// chains the dispatch's three lists in without deduplicating them, and
/// `retries` today repeats what `errors` already recorded.
#[test]
fn repeated_loss_entries_decide_the_same_way() {
    assert!(report_route().went_out([REFUSING_IFACE, REFUSING_IFACE]));
    assert!(!report_route().went_out([REFUSING_IFACE, REPORT_IFACE, REFUSING_IFACE]));
}

// ---------------------------------------------------------------------------
// #370: what a reboot leaves the policy to work with
// ---------------------------------------------------------------------------
//
// Walk 6 (2026-09-07): a Pocket V2 with a persisted tracker target was power
// cycled, then walked 100 m and stood outside for 30 minutes with a good fix,
// and sent nothing at all until the target was re-applied. The two tests
// named `..reboot..` model the two boots a node can wake into: one where the
// key came back with the flash record, and one where it did not — because the
// record was hash-only and the identity store is RAM, which is what every
// lnflash-configured board woke into before the record started carrying the
// resolved key.

/// The walk of the field scenario: first usable fix at `t=0`, 100 m due
/// north over the next five minutes, standing still from then on.
fn walk_fix(t_ms: u64) -> Option<Fix> {
    let metres = (t_ms.min(300_000) / 3_000) as i64;
    Some(north_of(good_fix(), metres))
}

/// Drive the policy exactly as the firmware's main loop does — one poll per
/// tick, every emission delivered — and collect the reports it produces.
fn reports_over(
    p: &mut SendPolicy,
    to_ms: u64,
    fix_at: impl Fn(u64) -> Option<Fix>,
) -> Vec<(u64, ReportReason)> {
    let mut out = Vec::new();
    let mut t = 0;
    while t <= to_ms {
        let fix = fix_at(t);
        if let Some(reason) = p.poll(t, fix) {
            p.note_emitted(t, fix.filter(|f| p.position_is_reportable(*f)));
            assert!(p.note_dispatch(true));
            out.push((t, reason));
        }
        t += TICK_MS;
    }
    out
}

/// A reboot whose flash record carried the key restores a usable target,
/// and a usable target owes the immediate report at the first poll — the
/// observability rule does not wait for movement or the heartbeat.
#[test]
fn a_reboot_that_restores_the_key_owes_the_immediate_report_at_once() {
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    let reports = reports_over(&mut p, 25 * 60_000, walk_fix);
    assert_eq!(reports.first(), Some(&(0, ReportReason::Immediate)));
    assert!(
        reports.iter().any(|(_, r)| *r == ReportReason::Movement),
        "the walk itself must also report: {reports:?}"
    );
}

/// **The #370 mechanism.** A reboot that loses the key lands in
/// awaiting-key with nothing armed, and the policy is silent there by
/// design — nothing can be encrypted to a target whose key is unknown, so
/// no immediate, no movement, no heartbeat, however far the node walks and
/// however good its fix. The walk-6 silence is this state: the flash
/// record was hash-only (lnflash sends no key) and the identity store is
/// RAM, so the power cycle forgot the key the target had already resolved.
/// The repair is therefore not in this crate: the firmware persists the
/// resolved key with the target record, so the next boot takes the test
/// above's path instead of this one's.
#[test]
fn a_reboot_that_loses_the_key_reports_nothing_however_far_it_walks() {
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, false);
    let reports = reports_over(&mut p, 25 * 60_000, walk_fix);
    assert_eq!(
        reports,
        vec![],
        "awaiting-key must stay silent — a report here would be unencryptable"
    );
    assert_eq!(p.state(), TargetState::AwaitingKey);
}

/// The second field boot: the fix arrives only after the walk. The
/// immediate report goes out position-less at once, and the node proves it
/// is alive within one heartbeat of the fix arriving — the report the
/// operator waits for outdoors.
#[test]
fn a_first_fix_arriving_after_the_walk_still_heartbeats_within_the_maximum_interval() {
    let fix_at_ms: u64 = 300_000;
    let mut p = reporting_node();
    p.set_target(Profile::Tracker, true);
    let reports = reports_over(&mut p, 25 * 60_000, |t| (t >= fix_at_ms).then(good_fix));
    assert_eq!(reports.first(), Some(&(0, ReportReason::Immediate)));
    let max = PolicyParams::TRACKER.max_interval_ms;
    assert!(
        reports
            .iter()
            .any(|(t, _)| *t > fix_at_ms && *t <= fix_at_ms + max),
        "no report within {max} ms of the first fix: {reports:?}"
    );
}

/// Q4 of the #370 audit, pinned: losing the fix does not restart the
/// settle window. A receiver flickering in a pocket cannot hold the
/// movement path shut — the anchor is the first usable fix of the boot,
/// set once.
#[test]
fn the_settle_window_does_not_restart_when_the_fix_flickers() {
    let mut p = ready(Profile::Tracker, 0);
    let settle = PolicyParams::TRACKER.settle_ms;
    let min = PolicyParams::TRACKER.min_interval_ms;
    // The fix goes away for a moment right at the end of the window...
    assert_eq!(p.poll(min.max(settle) + 5_000, None), None);
    // ...and the next usable fix decides against the original anchor, not
    // against a restarted one.
    assert_eq!(
        p.poll(min.max(settle) + 10_000, Some(north_of(good_fix(), 500))),
        Some(ReportReason::Movement)
    );
}

// ---------------------------------------------------------------------------
// Fixed position (user-set position as telemetry source)
// ---------------------------------------------------------------------------

/// A user-set fixed position in the policy's units, with the HDOP the
/// firmware hands it ([`FIXED_POSITION_HDOP_E2`]).
fn fixed_fix() -> Fix {
    Fix {
        latitude_e6: 52_520_008,
        longitude_e6: 13_404_954,
        hdop_e2: Some(FIXED_POSITION_HDOP_E2),
    }
}

/// The decided semantics: while set, the fixed position replaces the
/// sensor entirely — even a good sensor fix is not consulted.
#[test]
fn a_set_fixed_position_beats_the_sensor() {
    let (fix, source) = choose_position(Some(fixed_fix()), Some(good_fix()));
    assert_eq!(fix, Some(fixed_fix()));
    assert_eq!(source, PositionSource::Fixed);
    // With no sensor fix at all the answer is the same one.
    let (fix, source) = choose_position(Some(fixed_fix()), None);
    assert_eq!(fix, Some(fixed_fix()));
    assert_eq!(source, PositionSource::Fixed);
}

/// Clearing returns the node to sensor reporting — which for a
/// sensor-less board means no position, honestly.
#[test]
fn clearing_the_fixed_position_returns_to_the_sensor() {
    let (fix, source) = choose_position(None, Some(good_fix()));
    assert_eq!(fix, Some(good_fix()));
    assert_eq!(source, PositionSource::Gnss);
    let (fix, source) = choose_position(None, None);
    assert_eq!(fix, None);
    assert_eq!(source, PositionSource::Gnss);
}

/// The fixed position passes the accuracy gate in EVERY profile by
/// construction: the gate keeps untrusted sensor fixes off the air, and a
/// user assertion has no dilution to gate on.
#[test]
fn a_fixed_position_is_reportable_in_every_profile() {
    for profile in [Profile::Tracker, Profile::Station] {
        let mut p = reporting_node();
        p.set_target(profile, true);
        assert!(
            p.position_is_reportable(fixed_fix()),
            "{} refused the fixed position",
            profile.as_str()
        );
    }
}

/// Setting or clearing a fixed position on a usable target re-arms the
/// immediate report: the operator is owed the confirmation, and only the
/// attempt floor stands between them and it.
#[test]
fn a_position_config_change_re_arms_the_immediate_report() {
    let mut p = ready(Profile::Station, 0);
    // Well before the hourly heartbeat: nothing is due on its own.
    assert_eq!(p.poll(120_000, Some(fixed_fix())), None);
    p.note_position_config_changed();
    assert_eq!(
        p.poll(120_000, Some(fixed_fix())),
        Some(ReportReason::Immediate)
    );
}

/// In any state but ready the change arms nothing: off has nobody to
/// confirm to, and awaiting-key fires its immediate on key arrival anyway
/// — arming here would let a cleared target's confirmation fire later.
#[test]
fn a_position_config_change_arms_nothing_off_or_awaiting() {
    let mut p = reporting_node();
    p.note_position_config_changed();
    assert_eq!(p.poll(0, Some(fixed_fix())), None);

    let mut p = reporting_node();
    p.set_target(Profile::Station, false);
    p.note_position_config_changed();
    assert_eq!(p.poll(0, Some(fixed_fix())), None);
    // The immediate that does fire is the key-arrival one, once.
    assert!(p.note_key_available());
    assert_eq!(p.poll(1_000, None), Some(ReportReason::Immediate));
}

/// The attempt floor binds the re-armed immediate exactly like every
/// other emission: a config change cannot become a transmit storm.
#[test]
fn the_attempt_floor_holds_the_re_armed_immediate_back() {
    let mut p = ready(Profile::Station, 0);
    p.note_emitted(1_000_000, None);
    let _ = p.note_dispatch(true);
    p.note_position_config_changed();
    // Inside the floor (station: min_interval == 1 h): nothing.
    assert_eq!(p.poll(1_030_000, Some(fixed_fix())), None);
    // Past it: the confirmation fires.
    assert_eq!(
        p.poll(1_000_000 + 60 * 60_000, Some(fixed_fix())),
        Some(ReportReason::Immediate)
    );
}

// ---------------------------------------------------------------------------
// The position-source clause of the send condition (Lew, 2026-08-30)
// ---------------------------------------------------------------------------

/// A target and nothing that answers "where am I": the node sends nothing
/// at all, and the state says why instead of leaving the operator to guess.
#[test]
fn a_target_without_a_position_source_sends_nothing_and_says_why() {
    let mut p = SendPolicy::new();
    assert_eq!(
        p.set_target(Profile::Station, true),
        TargetState::NoPositionSource
    );
    assert_eq!(p.state().as_str(), "no-position-source");
    // Not the immediate report, and not the heartbeat either — an hour
    // later, two hours later, still nothing. "Sends nothing" is literal.
    assert_eq!(p.poll(0, None), None);
    assert_eq!(p.poll(60 * 60_000, None), None);
    assert_eq!(p.poll(2 * 60 * 60_000, None), None);
}

/// **The positive control for the test above.** The same target, the same
/// clock, on a node that does have a position source: it reports at once.
/// Without this the first test would pass just as well against a policy
/// that had stopped reporting for some other reason.
#[test]
fn control_the_same_target_reports_when_a_position_source_exists() {
    let mut p = reporting_node();
    assert_eq!(p.set_target(Profile::Station, true), TargetState::Ready);
    assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
}

/// A node whose receiver has no fix keeps reporting: the switch is intent,
/// not possession. The tracker in the garage sends its heartbeat with no
/// position and a fresh battery reading, which is the designed behaviour.
#[test]
fn a_gnss_node_without_a_fix_keeps_reporting_position_less() {
    let mut p = reporting_node();
    p.set_target(Profile::Station, true);
    assert_eq!(p.state(), TargetState::Ready);
    // No fix at all, ever — and the immediate plus the heartbeat still fire.
    assert_eq!(p.poll(0, None), Some(ReportReason::Immediate));
    p.note_sent(0, None);
    assert_eq!(
        p.poll(60 * 60_000, None),
        Some(ReportReason::Heartbeat),
        "a receiver without sky is not a node without telemetry"
    );
}

/// Setting a fixed position on a board that had no source flips it to the
/// ordinary lifecycle at once — the runtime path, no reboot.
#[test]
fn a_position_source_appearing_resumes_the_normal_lifecycle() {
    let mut p = SendPolicy::new();
    p.set_target(Profile::Station, true);
    assert_eq!(p.poll(0, None), None);

    assert_eq!(p.set_position_source(true), TargetState::Ready);
    // The immediate the target armed was never spent, so it is still owed.
    assert_eq!(p.poll(1_000, None), Some(ReportReason::Immediate));
}

/// And back the other way: clearing the last source on a running node
/// stops it and names the reason, without disturbing the key lifecycle
/// underneath — which is what makes the return trip free.
#[test]
fn a_position_source_disappearing_stops_the_node_and_names_the_reason() {
    let mut p = ready(Profile::Station, 0);
    assert_eq!(p.set_position_source(false), TargetState::NoPositionSource);
    assert_eq!(p.poll(60 * 60_000, Some(good_fix())), None);

    assert_eq!(p.set_position_source(true), TargetState::Ready);
    assert_eq!(
        p.poll(60 * 60_000, Some(good_fix())),
        Some(ReportReason::Heartbeat),
        "the key was never re-resolved, so the heartbeat picks up where it was"
    );
}

/// The key lifecycle keeps running underneath, but it is not what the
/// operator is shown: "no position source" is the reason nothing will be
/// sent, and the key is not.
#[test]
fn no_position_source_outranks_awaiting_key_in_the_reported_state() {
    let mut p = SendPolicy::new();
    assert_eq!(
        p.set_target(Profile::Station, false),
        TargetState::NoPositionSource
    );
    assert!(p.note_key_available(), "the key lifecycle still advances");
    assert_eq!(p.state(), TargetState::NoPositionSource);
    assert_eq!(p.set_position_source(true), TargetState::Ready);
}

/// No target beats no position source: a node nobody asked to report is
/// simply off, and telling its operator to set a pin would be nonsense.
#[test]
fn off_outranks_no_position_source() {
    let mut p = SendPolicy::new();
    assert_eq!(p.state(), TargetState::Off);
    p.set_target(Profile::Station, true);
    assert_eq!(p.state(), TargetState::NoPositionSource);
    p.clear_target();
    assert_eq!(p.state(), TargetState::Off);
    // Clearing the target does not un-declare the node's own hardware.
    assert!(!p.has_position_source());
    p.set_position_source(true);
    assert_eq!(p.state(), TargetState::Off);
    assert!(p.has_position_source());
}

// ---------------------------------------------------------------------------
// On-air report requests (#371)
// ---------------------------------------------------------------------------

/// The concept's on-demand rule: an allowed peer's request in state
/// `ready` arms the same immediate report a target write arms.
#[test]
fn a_request_in_ready_arms_the_immediate_report() {
    let mut p = ready(Profile::Station, 0);
    let t = PolicyParams::STATION.min_interval_ms + 1;
    assert_eq!(p.note_report_request(t), RequestOutcome::Armed);
    assert_eq!(p.poll(t, Some(good_fix())), Some(ReportReason::Immediate));
}

/// In awaiting-key nothing is armed and nothing banked: a report that
/// fired on key arrival hours later would answer a question nobody is
/// still asking — and key arrival owes an immediate report anyway.
#[test]
fn a_request_in_awaiting_key_arms_nothing() {
    let mut p = reporting_node();
    p.set_target(Profile::Station, false);
    assert_eq!(p.note_report_request(1_000), RequestOutcome::NotReady);
    assert_eq!(p.poll(1_100, Some(good_fix())), None);
    // The refusal did not consume the request window: the first request
    // after the key arrives is accepted at once.
    assert!(p.note_key_available());
    p.note_sent(2_000, Some(good_fix()));
    let t = 2_000 + PolicyParams::STATION.min_interval_ms;
    assert_eq!(p.note_report_request(t), RequestOutcome::Armed);
}

/// Off and no-position-source refuse the same way — the folded state
/// decides, not the key lifecycle alone.
#[test]
fn a_request_with_no_target_or_no_position_source_is_not_ready() {
    let mut p = reporting_node();
    assert_eq!(p.note_report_request(0), RequestOutcome::NotReady);
    let mut p = SendPolicy::new();
    p.set_target(Profile::Station, true);
    assert_eq!(p.state(), TargetState::NoPositionSource);
    assert_eq!(p.note_report_request(0), RequestOutcome::NotReady);
}

/// One request-triggered report per `min_interval_ms` of the active
/// profile: the second request inside the window is dropped, the first
/// one past it is accepted.
#[test]
fn a_second_request_inside_the_window_is_dropped() {
    let mut p = ready(Profile::Tracker, 0);
    let window = PolicyParams::TRACKER.min_interval_ms;
    assert_eq!(p.note_report_request(window), RequestOutcome::Armed);
    assert_eq!(
        p.note_report_request(2 * window - 1),
        RequestOutcome::RateLimited
    );
    // A dropped request is dropped, not queued: it moved no clock.
    assert_eq!(p.note_report_request(2 * window), RequestOutcome::Armed);
}

/// The accepted request rides the ordinary immediate machinery, so the
/// attempt floor bounds it exactly as it bounds a target write's report.
#[test]
fn a_request_triggered_report_still_honours_the_attempt_floor() {
    let mut p = ready(Profile::Tracker, 0);
    let window = PolicyParams::TRACKER.min_interval_ms;
    p.note_emitted(window, Some(good_fix()));
    assert!(p.note_dispatch(true));
    assert_eq!(p.note_report_request(window + 1), RequestOutcome::Armed);
    assert_eq!(p.poll(window + 2, Some(good_fix())), None, "floor holds");
    assert_eq!(
        p.poll(2 * window, Some(good_fix())),
        Some(ReportReason::Immediate),
        "owed until sent, emitted once the floor clears"
    );
}

// ---------------------------------------------------------------------------
// Proof-driven retransmission (#365/#373)
// ---------------------------------------------------------------------------

/// A distinct packet hash per send, so the tests can tell the first
/// emission from the retransmission the way the transport does.
fn pkt(n: u8) -> PacketHash {
    [n; 16]
}

/// The common case: the proof arrives inside the receipt timeout. One
/// success, measured from the send, and nothing owed.
#[test]
fn a_proof_inside_the_timeout_is_success_and_owes_nothing() {
    let mut t = ProofTracker::new();
    t.note_report_sent(pkt(1), 1_000);
    assert_eq!(
        t.note_proof(&pkt(1), 1_160),
        Some(ProvenReport {
            first: pkt(1),
            after_ms: 160
        })
    );
    assert_eq!(t.awaiting_retry(), None);
    // Counted once: the same proof again matches nothing.
    assert_eq!(t.note_proof(&pkt(1), 1_161), None);
}

/// Sent, no proof within the timeout: exactly one resend is owed, and a
/// second failure report for the same first send does not owe another.
#[test]
fn no_proof_within_the_timeout_owes_exactly_one_resend() {
    let mut t = ProofTracker::new();
    t.note_report_sent(pkt(1), 0);
    assert_eq!(
        t.note_failure(&pkt(1), 22_098),
        FailureVerdict::RetryDue { first: pkt(1) }
    );
    assert_eq!(t.awaiting_retry(), Some(pkt(1)), "the debt is held");
    // The debt is one deep: a duplicate failure changes nothing.
    assert_eq!(t.note_failure(&pkt(1), 22_099), FailureVerdict::NotTracked);
    assert_eq!(t.awaiting_retry(), Some(pkt(1)));
}

/// Proof after the resend: success, counted once, measured from the
/// FIRST send.
#[test]
fn a_proof_for_the_resend_is_one_success() {
    let mut t = ProofTracker::new();
    t.note_report_sent(pkt(1), 0);
    assert_eq!(
        t.note_failure(&pkt(1), 22_000),
        FailureVerdict::RetryDue { first: pkt(1) }
    );
    t.note_retry_sent(pkt(2));
    assert_eq!(t.awaiting_retry(), None, "the debt is settled by the send");
    assert_eq!(
        t.note_proof(&pkt(2), 24_300),
        Some(ProvenReport {
            first: pkt(1),
            after_ms: 24_300
        })
    );
    assert_eq!(t.note_proof(&pkt(2), 24_301), None, "counted once");
}

/// No proof after the resend either: the report is given up, and no
/// third attempt exists in any state the machine can reach from here.
#[test]
fn no_proof_after_the_resend_gives_up_with_no_third_attempt() {
    let mut t = ProofTracker::new();
    t.note_report_sent(pkt(1), 0);
    assert_eq!(
        t.note_failure(&pkt(1), 22_000),
        FailureVerdict::RetryDue { first: pkt(1) }
    );
    t.note_retry_sent(pkt(2));
    assert_eq!(
        t.note_failure(&pkt(2), 44_000),
        FailureVerdict::GaveUp {
            first: pkt(1),
            after_ms: 44_000
        }
    );
    // Nothing survives the give-up: no retry owed, later events for
    // either hash fall on the floor.
    assert_eq!(t.awaiting_retry(), None);
    assert_eq!(t.note_failure(&pkt(2), 44_001), FailureVerdict::NotTracked);
    assert_eq!(t.note_failure(&pkt(1), 44_002), FailureVerdict::NotTracked);
    assert_eq!(t.note_proof(&pkt(1), 44_003), None);
}

/// The proof of the FIRST send arriving after the resend went out: one
/// success, and the retry's own timeout later is not a loss — the report
/// was delivered.
#[test]
fn a_late_proof_for_the_first_send_counts_once_and_ends_the_chain() {
    let mut t = ProofTracker::new();
    t.note_report_sent(pkt(1), 0);
    assert_eq!(
        t.note_failure(&pkt(1), 22_000),
        FailureVerdict::RetryDue { first: pkt(1) }
    );
    t.note_retry_sent(pkt(2));
    assert_eq!(
        t.note_proof(&pkt(1), 23_000),
        Some(ProvenReport {
            first: pkt(1),
            after_ms: 23_000
        })
    );
    // The retransmission's receipt will still time out; that is not a
    // second loss and must not say "gave up" about a delivered report.
    assert_eq!(t.note_failure(&pkt(2), 45_000), FailureVerdict::NotTracked);
    assert_eq!(t.note_proof(&pkt(2), 45_001), None, "counted once");
}

/// A proof that lands between the timeout and the resend cancels the
/// owed retry: a proven report is not retransmitted.
#[test]
fn a_proof_while_the_retry_is_owed_cancels_it() {
    let mut t = ProofTracker::new();
    t.note_report_sent(pkt(1), 0);
    assert_eq!(
        t.note_failure(&pkt(1), 22_000),
        FailureVerdict::RetryDue { first: pkt(1) }
    );
    assert_eq!(
        t.note_proof(&pkt(1), 22_500),
        Some(ProvenReport {
            first: pkt(1),
            after_ms: 22_500
        })
    );
    assert_eq!(
        t.awaiting_retry(),
        None,
        "no retransmission of a proven report"
    );
}

/// A new report supersedes the old wait entirely — pending retry
/// included: the next scheduled reading says everything the lost one
/// did, fresher.
#[test]
fn a_new_report_supersedes_the_old_wait_and_its_retry() {
    let mut t = ProofTracker::new();
    t.note_report_sent(pkt(1), 0);
    assert_eq!(
        t.note_failure(&pkt(1), 22_000),
        FailureVerdict::RetryDue { first: pkt(1) }
    );
    t.note_report_sent(pkt(3), 60_000);
    assert_eq!(
        t.awaiting_retry(),
        None,
        "the old debt died with the report"
    );
    assert_eq!(t.note_proof(&pkt(1), 60_100), None, "old proof is nobody's");
    assert_eq!(
        t.note_proof(&pkt(3), 60_200),
        Some(ProvenReport {
            first: pkt(3),
            after_ms: 200
        })
    );
}

/// Somebody else's packets — a hash never tracked — are none of this
/// tracker's business in any state.
#[test]
fn untracked_hashes_are_ignored() {
    let mut t = ProofTracker::new();
    assert_eq!(t.note_failure(&pkt(9), 1), FailureVerdict::NotTracked);
    assert_eq!(t.note_proof(&pkt(9), 2), None);
    t.note_report_sent(pkt(1), 0);
    assert_eq!(t.note_failure(&pkt(9), 3), FailureVerdict::NotTracked);
    assert_eq!(t.note_proof(&pkt(9), 4), None);
    assert_eq!(t.awaiting_retry(), None);
}

/// Clearing the target forgets the tracked report and its debt.
#[test]
fn clear_forgets_the_tracked_report() {
    let mut t = ProofTracker::new();
    t.note_report_sent(pkt(1), 0);
    assert_eq!(
        t.note_failure(&pkt(1), 22_000),
        FailureVerdict::RetryDue { first: pkt(1) }
    );
    t.clear();
    assert_eq!(t.awaiting_retry(), None);
    assert_eq!(t.note_proof(&pkt(1), 23_000), None);
}
