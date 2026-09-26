//! Telemetry send policy and target lifecycle (Codeberg #236).
//!
//! A telemetry producer states *when it wants to report* — a minimum
//! interval, a minimum distance moved, a maximum interval as a heartbeat,
//! an accuracy threshold and a settle time — and it holds no airtime
//! figure of its own. Duty cycle, spacing and back-off belong to the
//! interface (`docs/src/concepts/interface-isolation.md`,
//! `docs/src/concepts/regulatory-airtime.md`). Nothing in this crate
//! knows what a channel is, how long a frame takes to send, or that LoRa
//! exists.
//!
//! It is the pure part, next to `leviculum-gnss-time` and
//! `leviculum-gnss-presence` and for the same reason: the decision "report
//! now, and why" is a state machine over time, position and target state,
//! and a state machine that needs a radio to be exercised is a state
//! machine that is never exercised.
//!
//! Two things live here, because they are one decision:
//!
//! * **The cadence policy** — [`Profile`] bundles the parameter defaults,
//!   [`SendPolicy::poll`] answers "report now?" with a [`ReportReason`].
//! * **The target lifecycle** — a target is set by hash alone (the
//!   2026-08-22 UX decision on #236: users know the LXMF address, not the
//!   key), so the node must resolve the key over the air before it can
//!   encrypt anything. [`TargetState::AwaitingKey`] is that wait, stated
//!   rather than hidden, and the immediate report of the concept's
//!   observability rule fires on key arrival rather than on target
//!   setting.
//!
//! The configured target *is* the on-switch: no target is
//! [`TargetState::Off`] and that is the default, which removes the
//! on-without-target and off-with-target states entirely.
//!
//! A second switch sits beside it, decided by Lew on 2026-08-30: **sending
//! the position is what turns on sending everything else**, so a node with
//! a target and no *position source* reports nothing at all
//! ([`TargetState::NoPositionSource`]). The reading is intent, not
//! possession — see [`SendPolicy::set_position_source`].

#![cfg_attr(not(test), no_std)]

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

/// Wire id of [`Profile::Tracker`], shared with the #238 control envelope
/// (`leviculum_core::envelope::TELEMETRY_PROFILE_TRACKER`).
pub const PROFILE_ID_TRACKER: u8 = 0x01;
/// Wire id of [`Profile::Station`]
/// (`leviculum_core::envelope::TELEMETRY_PROFILE_STATION`).
pub const PROFILE_ID_STATION: u8 = 0x02;
/// Wire id that clears the target instead of setting one
/// (`leviculum_core::envelope::TELEMETRY_PROFILE_OFF`). It is not a
/// [`Profile`]: "off" is the absence of a target, not a cadence.
pub const PROFILE_ID_OFF: u8 = 0x00;

/// Which cadence policy a target was configured with.
///
/// Profiles rather than individual knobs, because the two deployments
/// differ in kind and not in degree: a tracker is interesting when it
/// moves, a station is interesting when it is still alive. Both sets of
/// parameters remain individually addressable underneath
/// ([`PolicyParams`]) for the expert flags #236 describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Movement-driven: report when the node has moved far enough and not
    /// too recently, with a heartbeat as the fallback.
    Tracker,
    /// Slow heartbeat only, for a node that does not move. The default.
    Station,
}

impl Profile {
    /// The default profile: most users configure nothing beyond the
    /// address, and a node that does not move is the common case (a
    /// relay on a roof). A `station` misconfigured as a tracker reports
    /// nothing extra; a tracker misconfigured as a station under-reports
    /// but stays alive on the heartbeat — the failure is legible either
    /// way, and the cheaper one is the default.
    pub const DEFAULT: Self = Self::Station;

    /// Decode a wire profile id. [`PROFILE_ID_OFF`] is not a profile and
    /// yields `None`; the caller reads it as "clear the target".
    pub const fn from_wire(id: u8) -> Option<Self> {
        match id {
            PROFILE_ID_TRACKER => Some(Self::Tracker),
            PROFILE_ID_STATION => Some(Self::Station),
            _ => None,
        }
    }

    /// The wire id of this profile.
    pub const fn to_wire(self) -> u8 {
        match self {
            Self::Tracker => PROFILE_ID_TRACKER,
            Self::Station => PROFILE_ID_STATION,
        }
    }

    /// Short name for the structured `[TELEMETRY]` events.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tracker => "tracker",
            Self::Station => "station",
        }
    }

    /// The parameter defaults this profile bundles.
    pub const fn params(self) -> PolicyParams {
        match self {
            Self::Tracker => PolicyParams::TRACKER,
            Self::Station => PolicyParams::STATION,
        }
    }
}

/// The five cadence parameters, all of them policy and none of them
/// airtime.
///
/// Every duration is milliseconds because the firmware's monotonic clock
/// is; the distance is metres and the accuracy threshold is HDOP × 100,
/// which is what a GNSS receiver actually reports (see
/// [`Fix::hdop_e2`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyParams {
    /// Floor between two consecutive reports of any kind. A movement
    /// report that would land inside this window waits for it.
    pub min_interval_ms: u64,
    /// Ceiling between two consecutive reports: the heartbeat that keeps
    /// "stationary" distinguishable from "dead". Fires whether or not
    /// there is a position to report.
    pub max_interval_ms: u64,
    /// Metres the node must have moved since the last *reported* position
    /// before movement alone justifies a report. `0` disables the
    /// movement path entirely, which is what makes a station a station.
    pub min_distance_m: u32,
    /// Worst HDOP × 100 that still counts as a usable position. A fix
    /// above it is treated as no position at all: it is not reported, and
    /// it does not move the movement reference.
    pub max_hdop_e2: u16,
    /// How long after the first usable fix the movement path stays shut,
    /// so a cold start does not spend the channel on a drifting first
    /// fix. The heartbeat and the immediate report are exempt — the
    /// former carries no promise of precision, the latter is the
    /// operator's confirmation and the concept makes it unconditional.
    pub settle_ms: u64,
}

impl PolicyParams {
    /// Tracker defaults.
    ///
    /// One minute between reports and 50 m of movement is the shape of a
    /// person walking: at 1.4 m/s the distance gate opens after ~35 s and
    /// the interval gate then decides, so a walk reports about once a
    /// minute and a stationary rucksack falls back to the 15-minute
    /// heartbeat. HDOP 3.0 is the usual "good fix" line for consumer
    /// receivers; 60 s of settle covers a warm start's initial drift.
    pub const TRACKER: Self = Self {
        min_interval_ms: 60_000,
        max_interval_ms: 15 * 60_000,
        min_distance_m: 50,
        max_hdop_e2: 300,
        settle_ms: 60_000,
    };

    /// Station defaults.
    ///
    /// Hourly heartbeat and no movement path: `min_distance_m == 0`
    /// switches movement off, so a station reports on the clock alone
    /// even if it has a GNSS receiver and that receiver wanders. The
    /// interval floor equals the heartbeat, which is the honest way to
    /// say "this profile has exactly one cadence".
    pub const STATION: Self = Self {
        min_interval_ms: 60 * 60_000,
        max_interval_ms: 60 * 60_000,
        min_distance_m: 0,
        max_hdop_e2: 500,
        settle_ms: 30_000,
    };
}

// ---------------------------------------------------------------------------
// Target lifecycle
// ---------------------------------------------------------------------------

/// Where the configured target stands, and therefore whether anything can
/// be sent at all.
///
/// This is the honest three-state answer the 2026-08-22 UX decision
/// requires: hash-only configuration means the node may hold a perfectly
/// valid target it cannot yet encrypt to, and saying so beats waiting
/// silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetState {
    /// No target configured. Telemetry is off; this is the default.
    Off,
    /// A target is configured but no position source is
    /// ([`SendPolicy::set_position_source`]). Nothing is sent — not a
    /// heartbeat, not a battery reading — and the node says so instead of
    /// going quiet without a word.
    ///
    /// Ahead of [`AwaitingKey`](Self::AwaitingKey) on purpose: a node that
    /// will not report anyway must not spend airtime resolving a key it has
    /// no use for, and "no position source" is the honest headline while it
    /// holds. Setting a fixed position (or running a GNSS build) leaves
    /// this state at once, with no reboot.
    NoPositionSource,
    /// A target hash is configured but its public key is not known yet.
    /// The node resolves it over the air (path request, or simply hearing
    /// the target's announce). Nothing can be sent from here.
    AwaitingKey,
    /// Key known: reports can be built and encrypted.
    Ready,
}

impl TargetState {
    /// Short name for the structured `[TELEMETRY]` events and the boot
    /// banner line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::NoPositionSource => "no-position-source",
            Self::AwaitingKey => "awaiting-key",
            Self::Ready => "ready",
        }
    }
}

/// Why a report is due. Carried into the `[TELEMETRY]` event so a log
/// tail answers "why did it send just then" without a second tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportReason {
    /// The one report a newly usable target owes the operator, whatever
    /// the cadence says. Fires when the key becomes known, which for a
    /// hash-only target is later than the moment it was set.
    Immediate,
    /// The node moved at least `min_distance_m` since the last reported
    /// position, and `min_interval_ms` has passed.
    Movement,
    /// `max_interval_ms` has passed since the last report. Fires with or
    /// without a position.
    Heartbeat,
}

impl ReportReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Movement => "movement",
            Self::Heartbeat => "heartbeat",
        }
    }
}

// ---------------------------------------------------------------------------
// Position
// ---------------------------------------------------------------------------

/// One position offered to the policy, in the same scaled-integer domain
/// the telemetry codec packs.
///
/// Degrees × 1e6 rather than floats: GNSS receivers deliver scaled
/// integers, the wire carries scaled integers, and a policy that decides
/// on floats would decide differently from the value it caused to be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fix {
    pub latitude_e6: i32,
    pub longitude_e6: i32,
    /// Horizontal dilution of precision × 100, as the receiver reported
    /// it. `None` when no HDOP-bearing sentence has been seen; the
    /// accuracy gate then cannot be applied and the fix is refused,
    /// because "we do not know how good this is" is not "it is good".
    pub hdop_e2: Option<u16>,
}

/// The HDOP a user-set fixed position carries into the policy
/// ([`Fix::hdop_e2`]).
///
/// Zero, by construction and not as a measurement: the accuracy gate
/// exists to keep an untrusted *sensor* fix off the air, and a user's
/// "this is where this node is" is an assertion with no dilution to
/// gate on. Zero passes every profile's `max_hdop_e2` whatever it is
/// set to, which is what the decided semantics require — the fixed
/// position replaces the sensor entirely, in every profile.
pub const FIXED_POSITION_HDOP_E2: u16 = 0;

/// Which source a reported position came from, for the `possrc=` slot of
/// the `[TELEMETRY]` report line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionSource {
    /// A user-set fixed position ([`choose_position`] preferred it).
    Fixed,
    /// The GNSS sensor (or nothing at all — an absent position is still
    /// the sensor's answer while no fixed position is set).
    Gnss,
}

impl PositionSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Gnss => "gnss",
        }
    }
}

/// The position-source precedence, stated once and host-tested: a set
/// fixed position REPLACES the sensor entirely while set — no blending,
/// no fallback surprises — and clearing it returns the node to sensor
/// reporting. The sensor fix is not consulted, not even when the fixed
/// position is somehow unusable: the user's assertion beats a wandering
/// fix, and "unusable" cannot happen by construction
/// ([`FIXED_POSITION_HDOP_E2`]).
pub const fn choose_position(
    fixed: Option<Fix>,
    sensor: Option<Fix>,
) -> (Option<Fix>, PositionSource) {
    match fixed {
        Some(fix) => (Some(fix), PositionSource::Fixed),
        None => (sensor, PositionSource::Gnss),
    }
}

/// Metres per degree of latitude on WGS84, at the mean radius. The value
/// varies by about 1 % between equator and pole; the movement gate is a
/// threshold on a distance of tens of metres, so a 1 % model error is
/// three orders of magnitude below anything it decides.
const METRES_PER_DEGREE_LAT: i64 = 111_320;

/// cos(latitude) in Q15 at one-degree steps, 0°..=90°. Used to shrink a
/// longitude difference to metres. A table plus linear interpolation
/// rather than a trigonometric call: this crate has no `libm` and the
/// firmware has no FPU library beyond what the core provides, and the
/// interpolation error (< 1e-4 of the value) is again far below the
/// threshold's own resolution.
const COS_Q15: [i32; 91] = [
    32768, 32763, 32748, 32723, 32688, 32643, 32588, 32524, 32449, 32365, 32270, 32166, 32052,
    31928, 31795, 31651, 31499, 31336, 31164, 30983, 30792, 30592, 30382, 30163, 29935, 29698,
    29452, 29197, 28932, 28660, 28378, 28088, 27789, 27482, 27166, 26842, 26510, 26170, 25822,
    25466, 25102, 24730, 24351, 23965, 23571, 23170, 22763, 22348, 21926, 21498, 21063, 20622,
    20174, 19720, 19261, 18795, 18324, 17847, 17364, 16877, 16384, 15886, 15384, 14876, 14365,
    13848, 13328, 12803, 12275, 11743, 11207, 10668, 10126, 9580, 9032, 8481, 7927, 7371, 6813,
    6252, 5690, 5126, 4560, 3993, 3425, 2856, 2286, 1715, 1144, 572, 0,
];

/// cos(latitude) in Q15 for a latitude in degrees × 1e6, interpolated
/// between whole-degree table entries. Symmetric in the hemisphere and
/// clamped at the poles.
fn cos_lat_q15(latitude_e6: i32) -> i64 {
    let abs_e6 = (latitude_e6 as i64).abs().min(90_000_000);
    let whole = (abs_e6 / 1_000_000) as usize;
    if whole >= 90 {
        return 0;
    }
    let frac = abs_e6 % 1_000_000;
    let lo = COS_Q15[whole] as i64;
    let hi = COS_Q15[whole + 1] as i64;
    lo + (hi - lo) * frac / 1_000_000
}

/// Longitude difference in degrees × 1e6, taken the short way round so a
/// step across the antimeridian is one degree and not 359.
fn delta_lon_e6(a: i32, b: i32) -> i64 {
    let mut d = a as i64 - b as i64;
    if d > 180_000_000 {
        d -= 360_000_000;
    } else if d < -180_000_000 {
        d += 360_000_000;
    }
    d
}

/// Whether `a` and `b` are at least `min_m` metres apart.
///
/// Equirectangular approximation in millimetres: over the tens to
/// hundreds of metres a movement gate is set to, the error against the
/// great-circle distance is below the GNSS noise the gate exists to ride
/// out. Compares squares only after establishing that each component is
/// itself below the threshold, so the squares cannot overflow `i64` for
/// any threshold a firmware could hold.
fn moved_at_least(a: Fix, b: Fix, min_m: u32) -> bool {
    let threshold_mm = min_m as i64 * 1000;
    let dy_mm = (a.latitude_e6 as i64 - b.latitude_e6 as i64) * METRES_PER_DEGREE_LAT / 1000;
    let cos = cos_lat_q15((a.latitude_e6 / 2).saturating_add(b.latitude_e6 / 2));
    let dx_mm =
        delta_lon_e6(a.longitude_e6, b.longitude_e6) * METRES_PER_DEGREE_LAT / 1000 * cos / 32768;
    if dy_mm.abs() >= threshold_mm || dx_mm.abs() >= threshold_mm {
        return true;
    }
    dy_mm * dy_mm + dx_mm * dx_mm >= threshold_mm * threshold_mm
}

// ---------------------------------------------------------------------------
// The policy
// ---------------------------------------------------------------------------

/// What a telemetry-target control frame asks for, once its profile slot
/// has been read.
///
/// The wire form lives in `leviculum_core::envelope`; this is the same
/// decision expressed without a dependency on it, so the whole chain
/// "profile id in a frame → target state" is testable in one crate that
/// links nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetCommand {
    /// [`PROFILE_ID_OFF`]: forget the target. Telemetry off.
    Clear,
    /// Any other id: report to this target with that cadence. An id this
    /// firmware does not know arrives as [`Profile::DEFAULT`] — a newer
    /// host's cadence preference is not worth losing the destination
    /// over, and the state the node reports says which profile it runs.
    Set(Profile),
}

/// Read a control frame's profile slot.
pub const fn command_from_wire(profile_id: u8) -> TargetCommand {
    if profile_id == PROFILE_ID_OFF {
        TargetCommand::Clear
    } else {
        match Profile::from_wire(profile_id) {
            Some(profile) => TargetCommand::Set(profile),
            None => TargetCommand::Set(Profile::DEFAULT),
        }
    }
}

/// What applying a [`TargetCommand`] did, for the caller's event line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOutcome {
    /// A target was set or replaced; the state says whether it is usable
    /// yet ([`TargetState::Ready`]) or still resolving its key
    /// ([`TargetState::AwaitingKey`]).
    Set(TargetState),
    /// The target was cleared: telemetry off.
    Cleared,
}

/// What became of an on-air telemetry request (Codeberg #371) — the
/// caller's log line names this outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOutcome {
    /// Accepted: the immediate report is armed and goes out on the next
    /// poll past the attempt floor, exactly like a target write's.
    Armed,
    /// Inside the request window: less than `min_interval_ms` since the
    /// last accepted request. Logged and dropped, nothing armed.
    RateLimited,
    /// The target is not [`TargetState::Ready`] — off, awaiting its key,
    /// or without a position source. Nothing is armed and nothing is
    /// owed: a node that cannot answer must not bank a report that would
    /// fire long after the requester stopped waiting.
    NotReady,
}

impl RequestOutcome {
    /// Stable log token for the `[TELEMETRY] request` event.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Armed => "immediate",
            Self::RateLimited => "rate-limited",
            Self::NotReady => "not-ready",
        }
    }
}

/// The send policy and target lifecycle of one node.
///
/// Drive it with [`set_target`](Self::set_target) /
/// [`note_key_available`](Self::note_key_available) /
/// [`clear_target`](Self::clear_target) from the control channel, with
/// [`poll`](Self::poll) from the main loop, and confirm every report the
/// radio actually took with [`note_emitted`](Self::note_emitted) followed
/// by [`note_dispatch`](Self::note_dispatch).
///
/// The split between `poll` and the confirmation is deliberate: a report
/// that could not be built, could not be handed to transport, or was
/// handed over and then lost by the dispatch must not consume the
/// cadence, or a node would go quiet for a whole interval after one
/// failed attempt. The converse is just as deliberate and is enforced by
/// the attempt floor in [`poll`](Self::poll): a node that cannot deliver
/// must not retry faster than it would have reported, or one unreachable
/// target turns a reporter into a transmitter.
#[derive(Debug, Clone)]
pub struct SendPolicy {
    profile: Profile,
    params: PolicyParams,
    /// The *key* lifecycle alone: `Off`, `AwaitingKey` or `Ready`. Never
    /// [`TargetState::NoPositionSource`] — that one is a second axis, and
    /// [`state`](Self::state) is where the two are combined. Keeping them
    /// apart is what makes the position source appearing or disappearing a
    /// pure toggle: the key lifecycle underneath is not disturbed, so a
    /// node that had already resolved its key does not resolve it again.
    key_state: TargetState,
    /// Whether a position source is configured — see
    /// [`set_position_source`](Self::set_position_source).
    position_source: bool,
    /// Armed when the target became usable and not yet spent.
    immediate_pending: bool,
    /// When the first fix passing the accuracy gate was seen; the settle
    /// window is measured from here.
    first_fix_ms: Option<u64>,
    last_report_ms: Option<u64>,
    /// When a report was last *handed to transport*, whatever became of
    /// it. Separate from `last_report_ms` because the two answer different
    /// questions — "when did a report actually go out" versus "when did
    /// this node last spend airtime trying" — and only the second one can
    /// bound a retry. See [`poll`](SendPolicy::poll).
    last_attempt_ms: Option<u64>,
    last_reported_fix: Option<Fix>,
    /// A report handed to transport whose dispatch has not been settled
    /// yet. See [`note_emitted`](Self::note_emitted).
    pending: Option<PendingReport>,
    /// When the last on-air request was accepted
    /// ([`note_report_request`](Self::note_report_request)); the request
    /// window is measured from here. Like the attempt floor it survives
    /// target changes: airtime is airtime whoever asks, and a requester
    /// cannot widen its budget by having the operator retarget the node.
    last_request_ms: Option<u64>,
}

/// A report that is out of the reporter's hands but not yet on the air.
#[derive(Debug, Clone, Copy)]
struct PendingReport {
    /// When it was emitted. The cadence anchors here rather than at
    /// settle time, so the confirmation's own latency does not shorten
    /// the next interval.
    now_ms: u64,
    /// The position it carried, `None` if it carried none.
    fix: Option<Fix>,
}

impl Default for SendPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl SendPolicy {
    /// A node with no target: telemetry off, default profile parameters
    /// loaded so a `set_target` that names no profile still has them.
    pub const fn new() -> Self {
        Self {
            profile: Profile::DEFAULT,
            params: Profile::DEFAULT.params(),
            key_state: TargetState::Off,
            position_source: false,
            immediate_pending: false,
            first_fix_ms: None,
            last_report_ms: None,
            last_attempt_ms: None,
            last_reported_fix: None,
            pending: None,
            last_request_ms: None,
        }
    }

    /// The one state an operator is shown, both axes folded together.
    ///
    /// With no target there is nothing to say beyond [`TargetState::Off`].
    /// With a target and no position source the answer is
    /// [`TargetState::NoPositionSource`] whatever the key lifecycle is
    /// doing underneath, because that is the reason nothing will be sent.
    pub const fn state(&self) -> TargetState {
        match self.key_state {
            TargetState::Off => TargetState::Off,
            _ if !self.position_source => TargetState::NoPositionSource,
            other => other,
        }
    }

    /// Declare whether this node has a **position source configured**: a
    /// user-set fixed position, or a GNSS receiver that is built into this
    /// firmware and not switched off.
    ///
    /// Returns the state the node is now in, so a caller logs the answer
    /// rather than predicting it.
    ///
    /// # Intent, not possession (Lew, 2026-08-30)
    ///
    /// The switch is "does this node mean to report where it is", not "does
    /// it know where it is right now". A GNSS node sitting in a garage with
    /// no fix keeps reporting on the heartbeat, position absent — a tracker
    /// that goes silent the moment it loses sky is a tracker that is
    /// indistinguishable from a dead one, and the aging pin plus fresh
    /// battery and temperature data is the designed behaviour, not a
    /// degraded one. What is refused is the *unconfigured* node: a target
    /// and no answer at all to "where", which is telemetry nobody asked
    /// for.
    ///
    /// It is a runtime path, deliberately: a `--set-position` on a board
    /// that had none flips it to the ordinary lifecycle without a reboot,
    /// exactly as applying a target does.
    pub fn set_position_source(&mut self, present: bool) -> TargetState {
        self.position_source = present;
        self.state()
    }

    /// Whether a position source is configured.
    pub const fn has_position_source(&self) -> bool {
        self.position_source
    }

    pub const fn profile(&self) -> Profile {
        self.profile
    }

    pub const fn params(&self) -> PolicyParams {
        self.params
    }

    /// Override individual parameters (the expert flags under the
    /// profile presets, #236). Applied on top of whatever profile is
    /// configured; a later [`set_target`](Self::set_target) reloads the
    /// profile defaults over them.
    pub fn set_params(&mut self, params: PolicyParams) {
        self.params = params;
    }

    /// Configure a target. `key_known` is the caller's answer to "do we
    /// already hold this destination's public key" — for a hash-only
    /// frame it is `false` and the node enters
    /// [`TargetState::AwaitingKey`].
    ///
    /// Returns the state it entered. Setting a target always restarts the
    /// cadence: the last report went to somebody else.
    ///
    /// It does *not* restart the attempt floor. Airtime is airtime whoever
    /// the recipient is, and an exception here would be an escape hatch —
    /// a host that re-sends its target frame on a timer would drive
    /// exactly the storm the floor exists to stop.
    pub fn set_target(&mut self, profile: Profile, key_known: bool) -> TargetState {
        self.profile = profile;
        self.params = profile.params();
        self.last_report_ms = None;
        self.last_reported_fix = None;
        if key_known {
            self.key_state = TargetState::Ready;
            self.immediate_pending = true;
        } else {
            self.key_state = TargetState::AwaitingKey;
            // Not armed yet: the immediate report cannot be sent without
            // the key, and arming it here would let it fire on a target
            // that is later cleared before the key ever arrives.
            self.immediate_pending = false;
        }
        self.state()
    }

    /// Apply a control frame's decision in one call: the set/clear split
    /// and the key lookup that decides ready versus awaiting-key.
    ///
    /// `key_known` is the caller's answer to "do we already hold this
    /// destination's public key". It is ignored for
    /// [`TargetCommand::Clear`].
    pub fn apply(&mut self, command: TargetCommand, key_known: bool) -> TargetOutcome {
        match command {
            TargetCommand::Clear => {
                self.clear_target();
                TargetOutcome::Cleared
            }
            TargetCommand::Set(profile) => TargetOutcome::Set(self.set_target(profile, key_known)),
        }
    }

    /// The target's public key arrived over the air. Returns `true` if
    /// this was the transition out of [`TargetState::AwaitingKey`], which
    /// is the moment the immediate report is armed and the event worth
    /// logging.
    pub fn note_key_available(&mut self) -> bool {
        if self.key_state == TargetState::AwaitingKey {
            self.key_state = TargetState::Ready;
            self.immediate_pending = true;
            true
        } else {
            false
        }
    }

    /// The position configuration changed: a fixed position was set or
    /// cleared. Re-arms the immediate report when the target is usable —
    /// the operator who just changed what the node claims about itself is
    /// owed the same confirmation report a newly usable target grants
    /// (the observability rule), and the attempt floor in
    /// [`poll`](Self::poll) bounds it like every other emission. In any
    /// other state this is a no-op: off has nobody to confirm to, and
    /// awaiting-key arms the immediate on key arrival anyway.
    pub fn note_position_config_changed(&mut self) {
        if self.key_state == TargetState::Ready {
            self.immediate_pending = true;
        }
    }

    /// An authenticated peer asked for a report over the air (Sideband's
    /// `TELEMETRY_REQUEST`, Codeberg #371). The caller has already decided
    /// the sender is allowed and the signature holds — this method owns
    /// only the policy half: state and rate.
    ///
    /// An accepted request arms the same immediate report a target write
    /// arms, so everything already true of that report stays true here —
    /// it is owed until actually sent, and the attempt floor in
    /// [`poll`](Self::poll) bounds it. On top of that sits the request
    /// window: at most one accepted request per `min_interval_ms` of the
    /// active profile, so a peer that asks in a loop costs the channel no
    /// more than the profile already allows. A request inside the window
    /// is dropped, not queued — the next report answers the next request.
    ///
    /// Anything short of [`TargetState::Ready`] (the folded state, so
    /// `no-position-source` included) is [`RequestOutcome::NotReady`]:
    /// nothing armed, nothing owed, and the window untouched.
    pub fn note_report_request(&mut self, now_ms: u64) -> RequestOutcome {
        if self.state() != TargetState::Ready {
            return RequestOutcome::NotReady;
        }
        if let Some(last) = self.last_request_ms {
            if now_ms.saturating_sub(last) < self.params.min_interval_ms {
                return RequestOutcome::RateLimited;
            }
        }
        self.last_request_ms = Some(now_ms);
        self.immediate_pending = true;
        RequestOutcome::Armed
    }

    /// The key went away again (a target changed to one we do not hold).
    /// Only meaningful from [`TargetState::Ready`].
    pub fn note_key_lost(&mut self) -> bool {
        if self.key_state == TargetState::Ready {
            self.key_state = TargetState::AwaitingKey;
            self.immediate_pending = false;
            true
        } else {
            false
        }
    }

    /// Clear the target: telemetry off. The cadence state goes with it,
    /// so a later target does not inherit a stale "last reported" from a
    /// different recipient.
    pub fn clear_target(&mut self) {
        self.key_state = TargetState::Off;
        self.immediate_pending = false;
        self.last_report_ms = None;
        self.last_reported_fix = None;
    }

    /// Whether this fix is good enough to be reported as a position.
    ///
    /// A fix with no HDOP at all is refused: the concept's "no fix, no
    /// position" rule extends to "no idea how good the fix is". The
    /// heartbeat still reports without a position, which is exactly the
    /// absence encoding the concept fixes.
    pub const fn position_is_reportable(&self, fix: Fix) -> bool {
        match fix.hdop_e2 {
            Some(hdop) => hdop <= self.params.max_hdop_e2,
            None => false,
        }
    }

    /// Ask whether a report is due now.
    ///
    /// `fix` is the current GNSS answer: `None` when the node has no fix
    /// (presence is not `Fix`, or the receiver is absent). A fix that
    /// fails the accuracy gate is treated exactly like `None` — it is not
    /// a position, so it neither reports nor moves the movement
    /// reference.
    ///
    /// Mutates only the settle anchor: the first usable fix starts the
    /// settle window whether or not anything is sent.
    ///
    /// # The attempt floor
    ///
    /// The first gate is not the cadence but `min_interval_ms` since the
    /// last *emission*, successful or not, and it sits ahead of every
    /// other path including the immediate report. It states one invariant:
    ///
    /// > Between any two emissions of a telemetry report, successful or
    /// > not, at least `min_interval_ms` of clock has passed.
    ///
    /// A lost dispatch consumes no cadence and no reading — that is
    /// deliberate and stays — but without a second clock the node then
    /// finds the same interval elapsed on the very next tick and re-emits
    /// at the tick rate. Measured on the bench: an announce-plus-report
    /// pair every 6.5 s against a 60 s policy, ~20 % channel occupancy
    /// from one node.
    ///
    /// A node that has emitted nothing yet has no floor to clear, which is
    /// what keeps the immediate report of a newly usable target immediate.
    pub fn poll(&mut self, now_ms: u64, fix: Option<Fix>) -> Option<ReportReason> {
        // `state()` and not `key_state`: a node with no position source is
        // not [`TargetState::Ready`] however far its key lifecycle got, and
        // that is the whole of the send condition's second clause. The
        // heartbeat is included — "sends nothing" means nothing.
        if self.state() != TargetState::Ready {
            return None;
        }
        let usable = fix.filter(|f| self.position_is_reportable(*f));
        if usable.is_some() && self.first_fix_ms.is_none() {
            self.first_fix_ms = Some(now_ms);
        }
        if let Some(attempt) = self.last_attempt_ms {
            if now_ms.saturating_sub(attempt) < self.params.min_interval_ms {
                return None;
            }
        }
        if self.immediate_pending {
            return Some(ReportReason::Immediate);
        }
        let last = match self.last_report_ms {
            // Ready, nothing armed, nothing sent yet: the heartbeat
            // clock starts at the first poll rather than at boot, so a
            // target set long after boot does not fire instantly through
            // the heartbeat path.
            None => {
                self.last_report_ms = Some(now_ms);
                return None;
            }
            Some(last) => last,
        };
        let since = now_ms.saturating_sub(last);
        if since >= self.params.max_interval_ms {
            return Some(ReportReason::Heartbeat);
        }
        if self.params.min_distance_m == 0 || since < self.params.min_interval_ms {
            return None;
        }
        if let Some(first) = self.first_fix_ms {
            if now_ms.saturating_sub(first) < self.params.settle_ms {
                return None;
            }
        }
        match (usable, self.last_reported_fix) {
            // Nothing to compare against yet: the first usable fix after
            // the target became ready is itself the movement.
            (Some(_), None) => Some(ReportReason::Movement),
            (Some(now), Some(then)) if moved_at_least(now, then, self.params.min_distance_m) => {
                Some(ReportReason::Movement)
            }
            _ => None,
        }
    }

    /// Confirm that a report actually went out, with the position it
    /// carried (`None` when it carried none). Only this consumes the
    /// cadence and the armed immediate report.
    pub fn note_sent(&mut self, now_ms: u64, reported: Option<Fix>) {
        self.immediate_pending = false;
        self.last_report_ms = Some(now_ms);
        if reported.is_some() {
            self.last_reported_fix = reported;
        }
    }

    /// Note that a report has been *handed to transport* — built,
    /// encrypted, turned into actions — and is awaiting dispatch.
    ///
    /// This consumes nothing. Handing a packet to the core is not the
    /// same event as the interface taking it: on a board whose outbound
    /// queue is full, `send_single_packet` succeeds and the dispatch that
    /// follows drops the frame. Counting the first event as "sent" is how
    /// a report that never left the board still cost a whole cadence
    /// interval of silence (#344).
    ///
    /// It does, however, start the attempt floor: this is the moment
    /// airtime was spent, and [`poll`](Self::poll) refuses to emit again
    /// for `min_interval_ms` from here whatever the dispatch decides. That
    /// is the whole of the rate limit — the failure path adds nothing,
    /// because a floor that only the failure path raised would be a floor
    /// a caller could forget to raise.
    ///
    /// Pair it with [`note_dispatch`](Self::note_dispatch). Two
    /// `note_emitted` calls without a settle in between keep only the
    /// later one: the earlier report is gone either way, and the cadence
    /// belongs to the report that is actually in flight.
    pub fn note_emitted(&mut self, now_ms: u64, reported: Option<Fix>) {
        self.last_attempt_ms = Some(now_ms);
        self.pending = Some(PendingReport {
            now_ms,
            fix: reported,
        });
    }

    /// Settle the pending report against what the dispatch did with it.
    ///
    /// `delivered` is the dispatch's own verdict, not a guess from a log
    /// line: `true` consumes the cadence exactly as
    /// [`note_sent`](Self::note_sent) does, `false` consumes neither the
    /// cadence nor the reading, so the reading is still owed and goes out
    /// on the first tick past the attempt floor
    /// ([`poll`](Self::poll)) rather than on the very next one.
    ///
    /// Deliberately *not* a retry: nothing is re-sent here, nothing is
    /// queued, and the next attempt happens on the ordinary tick that was
    /// going to run anyway — the *reading* it carries is whatever the
    /// sensors say then, not the one that failed.
    ///
    /// Returns whether the report counted as sent — `false` also for a
    /// settle with nothing pending, so a caller cannot report success for
    /// a report it never emitted.
    pub fn note_dispatch(&mut self, delivered: bool) -> bool {
        match self.pending.take() {
            Some(report) if delivered => {
                self.note_sent(report.now_ms, report.fix);
                true
            }
            _ => false,
        }
    }

    /// Whether a report is emitted and not yet settled.
    pub const fn has_pending_report(&self) -> bool {
        self.pending.is_some()
    }
}

// ---------------------------------------------------------------------------
// What "the report went out" means (#348)
// ---------------------------------------------------------------------------

/// The interfaces one report's own frames were handed to, and with them the
/// verdict [`SendPolicy::note_dispatch`] wants.
///
/// **The rule: a report went out when every interface its own frames were
/// addressed to accepted them; a loss the same dispatch recorded against any
/// other interface belongs to that interface's traffic, not to this report.**
///
/// This is deliberately not "did the dispatch lose anything anywhere", which
/// is the right question for a `[DISPATCH_LOSS]` line and the wrong one here:
/// a board with BLE advertised and no phone attached refuses the announce
/// broadcast on BLE while LoRa puts the report on the air, and reading that
/// refusal as "not emitted" left the cadence unconsumed for ever (#348). It is
/// equally not "did LoRa take it" — a report only BLE accepted has gone out
/// too, energy was spent on it and a peer may hold it — so the question is
/// asked of the route the core actually chose and never of a named medium.
///
/// A route with no interface in it is a report that never became a frame on
/// any interface, and that is not an emission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmissionRoute {
    /// One bit per interface id, which covers every id a firmware board hands
    /// out (three, on the largest board we build).
    mask: u32,
    /// An id of 32 or above, which no bit can hold. It makes every loss in
    /// the dispatch count as this report's — the conservative answer, which
    /// costs a re-sent reading rather than a silently swallowed one.
    beyond_mask: bool,
}

impl EmissionRoute {
    /// A report that has been addressed nowhere yet.
    pub const fn new() -> Self {
        Self {
            mask: 0,
            beyond_mask: false,
        }
    }

    /// Record that one of this report's frames was addressed to `iface`.
    pub fn add(&mut self, iface: usize) {
        if iface < u32::BITS as usize {
            self.mask |= 1 << iface;
        } else {
            self.beyond_mask = true;
        }
    }

    /// Whether this report reached an interface at all.
    pub const fn is_empty(&self) -> bool {
        self.mask == 0 && !self.beyond_mask
    }

    /// Whether `iface` carried one of this report's frames.
    const fn carried(&self, iface: usize) -> bool {
        self.beyond_mask || (iface < u32::BITS as usize && self.mask & (1 << iface) != 0)
    }

    /// Apply the rule to one dispatch.
    ///
    /// `losses` names every interface that dispatch recorded a retry, an
    /// interface error or an unroutable drop against. Order does not matter
    /// and duplicates are fine — a caller chains the three lists straight in
    /// rather than deduplicating them.
    pub fn went_out(&self, losses: impl IntoIterator<Item = usize>) -> bool {
        !self.is_empty() && !losses.into_iter().any(|iface| self.carried(iface))
    }
}

// ---------------------------------------------------------------------------
// Proof-driven retransmission (#365/#373)
// ---------------------------------------------------------------------------

/// The truncated packet hash the transport tracks receipts under
/// (`leviculum_core::constants::TRUNCATED_HASHBYTES`). Spelled out here
/// because this crate deliberately links nothing.
pub type PacketHash = [u8; 16];

/// A tracked report was proven delivered. The caller writes the
/// `[TELEMETRY] proof` line from this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvenReport {
    /// The FIRST send's packet hash — the report's identity in every log
    /// line, whether the proof answered the first send or the
    /// retransmission.
    pub first: PacketHash,
    /// Milliseconds from the first send to the proof.
    pub after_ms: u64,
}

/// What a delivery failure means for the tracked report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureVerdict {
    /// Not the packet this tracker is watching — some other sender's
    /// business, nothing to do.
    NotTracked,
    /// The first send got no proof: the caller owes ONE retransmission of
    /// the SAME payload. The tracker holds the debt
    /// ([`ProofTracker::awaiting_retry`]) until
    /// [`ProofTracker::note_retry_sent`] settles it.
    RetryDue {
        /// The first send's packet hash.
        first: PacketHash,
    },
    /// The retransmission got no proof either: the report is given up.
    /// The caller writes the `gave up` line and drops the payload; the
    /// next scheduled report carries on unbothered.
    GaveUp {
        first: PacketHash,
        /// Milliseconds from the first send to this final loss.
        after_ms: u64,
    },
}

/// Where the one in-flight report stands between emission and proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProofState {
    /// Nothing in flight.
    Idle,
    /// The report went out; its receipt is ticking.
    AwaitingProof { first: PacketHash, sent_ms: u64 },
    /// The receipt timed out; one retransmission is owed and not yet out.
    RetryOwed { first: PacketHash, sent_ms: u64 },
    /// The retransmission went out; its receipt is ticking. `retry` is a
    /// different hash from `first` — the ordinary send path re-encrypts
    /// with a fresh ephemeral key — so both are watched.
    AwaitingRetryProof {
        first: PacketHash,
        retry: PacketHash,
        sent_ms: u64,
    },
}

/// The proof-driven retransmission state machine (#365/#373): a telemetry
/// report that gets no proof is sent ONCE more with the same payload,
/// then given up.
///
/// The field case this pays for: the phone drops one relayed report in
/// three, silently, after the relay's BLE forward (#373, ledger 365
/// HISTORY 19:30-19:45). We cannot fix the phone; one retransmission
/// buys back the position at the cost of one packet.
///
/// Python-side reference for the semantics: LXMF's own delivery resends
/// an unproven opportunistic message from its `process_outbound` loop
/// (`reference/LXMF/LXMF/LXMRouter.py:2736-2757`) up to
/// `MAX_DELIVERY_ATTEMPTS = 5` (LXMRouter.py:30). We take the mechanism
/// and not the count: a tracker's next reading is minutes away and
/// airtime is the scarce resource, so exactly one retry.
///
/// Boundaries, deliberate:
///
/// * This crate holds hashes and clocks, never the payload — keeping the
///   exact bytes for the retransmission is the caller's half, which is
///   what makes "identical resend" true by construction rather than by
///   re-building.
/// * The tracker never touches [`SendPolicy`]'s cadence: the retry is a
///   retransmission of a report already counted as sent, so it neither
///   consumes a reading nor moves `min_interval_ms` for the next one.
/// * One report in flight: a new report supersedes the old wait entirely
///   ([`note_report_sent`](Self::note_report_sent)), because the next
///   scheduled reading says everything the lost one did, fresher.
#[derive(Debug, Clone)]
pub struct ProofTracker {
    state: ProofState,
}

impl Default for ProofTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProofTracker {
    pub const fn new() -> Self {
        Self {
            state: ProofState::Idle,
        }
    }

    /// A report went out (the dispatch settled as sent): watch its
    /// receipt. Replaces whatever was tracked before — an old report's
    /// pending retry included.
    pub fn note_report_sent(&mut self, first: PacketHash, now_ms: u64) {
        self.state = ProofState::AwaitingProof {
            first,
            sent_ms: now_ms,
        };
    }

    /// The first send's hash while a retransmission is owed and not yet
    /// out; the caller's cue to resend the kept payload.
    pub const fn awaiting_retry(&self) -> Option<PacketHash> {
        match self.state {
            ProofState::RetryOwed { first, .. } => Some(first),
            _ => None,
        }
    }

    /// The retransmission went out under `retry`'s hash. Both hashes are
    /// watched from here: the late proof of the FIRST send is as good as
    /// the retry's own ([`note_proof`](Self::note_proof)).
    pub fn note_retry_sent(&mut self, retry: PacketHash) {
        if let ProofState::RetryOwed { first, sent_ms } = self.state {
            self.state = ProofState::AwaitingRetryProof {
                first,
                retry,
                sent_ms,
            };
        }
    }

    /// A delivery confirmation arrived. `Some` exactly once per tracked
    /// report — success is counted once however many sends it took and
    /// however late the proof came — and it cancels any owed or in-flight
    /// retransmission: a proven report needs no third attempt.
    pub fn note_proof(&mut self, hash: &PacketHash, now_ms: u64) -> Option<ProvenReport> {
        let (first, sent_ms) = match self.state {
            ProofState::AwaitingProof { first, sent_ms }
            | ProofState::RetryOwed { first, sent_ms }
                if first == *hash =>
            {
                (first, sent_ms)
            }
            ProofState::AwaitingRetryProof {
                first,
                retry,
                sent_ms,
            } if first == *hash || retry == *hash => (first, sent_ms),
            _ => return None,
        };
        self.state = ProofState::Idle;
        Some(ProvenReport {
            first,
            after_ms: now_ms.saturating_sub(sent_ms),
        })
    }

    /// A delivery failure (receipt timeout, or a proof that did not
    /// verify) arrived. First loss owes the one retry; the retry's loss
    /// is the end of the report. A failure for the first hash while the
    /// retransmission is in flight is NOT a second loss — that receipt
    /// already timed out to get here — and changes nothing.
    pub fn note_failure(&mut self, hash: &PacketHash, now_ms: u64) -> FailureVerdict {
        match self.state {
            ProofState::AwaitingProof { first, sent_ms } if first == *hash => {
                self.state = ProofState::RetryOwed { first, sent_ms };
                FailureVerdict::RetryDue { first }
            }
            ProofState::AwaitingRetryProof {
                first,
                retry,
                sent_ms,
            } if retry == *hash => {
                self.state = ProofState::Idle;
                FailureVerdict::GaveUp {
                    first,
                    after_ms: now_ms.saturating_sub(sent_ms),
                }
            }
            _ => FailureVerdict::NotTracked,
        }
    }

    /// Forget the tracked report — the target changed, so the old
    /// report's proof belongs to nobody.
    pub fn clear(&mut self) {
        self.state = ProofState::Idle;
    }
}

#[cfg(test)]
mod tests;
