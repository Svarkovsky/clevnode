//! The host side of the #238 control envelope.
//!
//! One speaker for every control conversation on the transport CDC: write
//! an HDLC-framed request, wait for the frame that answers it, retry a
//! bounded number of times. The wire format lives in
//! [`leviculum_core::envelope`]; this module adds only the serial-port
//! choreography, shared by the radio-config sender ([`crate::radio`]) and
//! the wall-time session — the framing is written exactly once.
//!
//! The capability probe replaces guess-by-timeout: one
//! [`TYPE_CAPABILITIES`](leviculum_core::envelope::TYPE_CAPABILITIES)
//! query tells the host which frame types the firmware accepts. Firmware
//! older than the envelope answers nothing — the probe's bounded silence
//! is the one place the old guessing survives, and it decides the legacy
//! fallback for the transition window (see
//! `docs/src/firmware/usb-control-envelope.md` for how that retires).

use std::io;
use std::time::{Duration, Instant};

use leviculum_core::envelope::{
    decode_ack_payload, decode_capability_report_payload, decode_frame,
    decode_identity_report_payload, decode_media_report_payload, decode_node_name_report_payload,
    decode_position_source_report_payload, decode_refusal_payload, encode_capability_query,
    encode_fixed_position, encode_identity_query, encode_media_profile, encode_media_query,
    encode_node_name, encode_node_name_query, encode_position_source_query, encode_radio_config,
    encode_telemetry_target, encode_tx_spacing, encode_wall_time, FixedPositionWire,
    IdentityReportWire, MediaProfileWire, NodeNameState, TelemetryTargetWire, REFUSE_BUSY,
    REFUSE_MALFORMED, REFUSE_NO_CLOCK, REFUSE_PERSIST, REFUSE_UNKNOWN_TYPE, REFUSE_UNSUPPORTED,
    REFUSE_VALUE, TYPE_ACK, TYPE_CAPABILITY_REPORT, TYPE_IDENTITY_QUERY, TYPE_IDENTITY_REPORT,
    TYPE_MEDIA_PROFILE, TYPE_MEDIA_QUERY, TYPE_MEDIA_REPORT, TYPE_NODE_NAME, TYPE_NODE_NAME_QUERY,
    TYPE_NODE_NAME_REPORT, TYPE_POSITION_SOURCE_QUERY, TYPE_POSITION_SOURCE_REPORT, TYPE_REFUSAL,
};
use leviculum_core::framing::hdlc::{frame, DeframeResult, Deframer};
use leviculum_core::node_name::NodeName;
use leviculum_core::rnode::RadioConfigWire;

use crate::sys::Fd;

/// How long one write may take before the port counts as gone.
const WRITE_WITHIN: Duration = Duration::from_secs(2);

/// Attempt/window budget for one control conversation.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub attempts: u8,
    pub window: Duration,
}

/// The capability probe's budget. Against firmware that speaks the
/// envelope the report arrives in milliseconds; against older firmware
/// this is the whole price of finding that out, so it is kept short.
pub const PROBE_TIMING: Timing = Timing {
    attempts: 2,
    window: Duration::from_millis(700),
};

/// The budget for a command that changes board state, matching the
/// legacy radio-config sender's patience.
pub const CONTROL_TIMING: Timing = Timing {
    attempts: crate::radio::ATTEMPTS,
    window: crate::radio::ACK_WITHIN,
};

/// What the firmware said to a command frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOutcome {
    /// The enveloped ack for this frame type arrived.
    Acked,
    /// A named refusal for this frame type arrived.
    Refused { reason: u8 },
    /// The window closed without an answer.
    NoAnswer,
}

/// What a board answered to a control conversation that opens with a
/// capability probe.
///
/// [`ControlOutcome`] plus the two ways a board can be unable to hold the
/// conversation at all, so a session reports "this firmware cannot" as a
/// fact about the firmware rather than as a timeout the operator has to
/// interpret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionReply {
    /// The enveloped ack for this frame type arrived.
    Acked,
    /// A named refusal for this frame type arrived.
    Refused(u8),
    /// The window closed without an answer.
    NoAnswer,
    /// No capability report. Firmware from before the #238 envelope
    /// answers the probe with nothing — but so does a board whose
    /// transport port has stopped being serviced, and from silence the
    /// two cannot be told apart (local-4modem-wedge: a live, counting
    /// board answered nothing here and was reported as old firmware).
    /// Messages for this reply must name both readings, never assert
    /// the firmware's age.
    ProbeSilent,
    /// A capability report that does not list this frame type.
    NotAccepted,
}

impl SessionReply {
    /// Whether the board took what it was given. Only an ack counts — a
    /// method rather than a comparison so no call site can quietly decide
    /// that silence is good enough.
    pub fn took_it(self) -> bool {
        matches!(self, Self::Acked)
    }
}

/// The one honest reading of [`SessionReply::ProbeSilent`], shared by
/// every transcript line that reports it: silence does not identify old
/// firmware, so the operator is handed both causes and the cheap test
/// that separates them.
pub const PROBE_SILENCE_HINT: &str =
    "Either this firmware predates the control envelope, or the transport port has stopped \
     answering. A board that took envelope commands before is the second case: reset it and \
     retry. Only a board that never spoke the envelope needs the current bundle flashed.";

impl From<ControlOutcome> for SessionReply {
    fn from(outcome: ControlOutcome) -> Self {
        match outcome {
            ControlOutcome::Acked => Self::Acked,
            ControlOutcome::Refused { reason } => Self::Refused(reason),
            ControlOutcome::NoAnswer => Self::NoAnswer,
        }
    }
}

/// Probe what the firmware accepts, and send only if the report lists
/// `frame_type`.
///
/// Every frame whose payload is longer than the 19-byte Reticulum minimum
/// has to come through here: sent on a guess to firmware that does not know
/// the type it is packet-shaped noise on the transport CDC rather than a
/// named refusal. The probe costs one round trip and turns that guess into
/// a fact.
pub fn probed(
    fd: &Fd,
    frame_type: u8,
    send: impl FnOnce(&Fd) -> io::Result<ControlOutcome>,
) -> io::Result<SessionReply> {
    let Some(caps) = probe_capabilities(fd)? else {
        return Ok(SessionReply::ProbeSilent);
    };
    if !caps.accepts(frame_type) {
        return Ok(SessionReply::NotAccepted);
    }
    Ok(send(fd)?.into())
}

/// A named refusal reason, for the transcript.
pub fn reason_str(reason: u8) -> &'static str {
    match reason {
        REFUSE_UNKNOWN_TYPE => "the firmware does not accept this frame type",
        REFUSE_MALFORMED => "the firmware calls the frame malformed",
        REFUSE_VALUE => "the firmware refused the value",
        REFUSE_BUSY => "the firmware is busy",
        REFUSE_UNSUPPORTED => "this binary carries no consumer for the frame",
        REFUSE_PERSIST => {
            "the board applied the value but could not write it to flash, \
                           so a reset would lose it"
        }
        REFUSE_NO_CLOCK => {
            "the board has no calendar clock yet, so it withheld the announce \
                           (reason=no-clock on its debug port); seed one with --set-time \
                           or a GNSS fix and retry"
        }
        _ => "an unnamed reason",
    }
}

/// What the capability report said the firmware accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub version: u8,
    pub accepted: Vec<u8>,
}

impl Capabilities {
    pub fn accepts(&self, frame_type: u8) -> bool {
        self.accepted.contains(&frame_type)
    }
}

/// Write `payload` HDLC-framed and wait for a deframed answer `classify`
/// accepts, retrying up to the budget. `Ok(None)` is "the board never
/// answered" — a fact for the caller, not a failure of the port.
///
/// One deframer across all attempts: a late answer to a previous attempt
/// is still an answer, and resetting would drop a frame mid-arrival.
///
/// Anything the board said *before* this frame went out is dropped first
/// ([`Fd::drain_input`]). Within one transaction a late answer is still
/// an answer; across two it is a leftover, and a classifier that cannot
/// tell which frame a reply belongs to — the media report names none —
/// would take it for this one.
pub fn transact<T>(
    fd: &Fd,
    payload: &[u8],
    timing: Timing,
    classify: impl Fn(&[u8]) -> Option<T>,
) -> io::Result<Option<T>> {
    let mut framed = Vec::new();
    frame(payload, &mut framed);
    fd.drain_input()?;
    let mut deframer = Deframer::new();
    for _ in 0..timing.attempts {
        fd.write_all(&framed, Instant::now() + WRITE_WITHIN)?;
        if let Some(answer) =
            wait_for(fd, Instant::now() + timing.window, &mut deframer, &classify)?
        {
            return Ok(Some(answer));
        }
    }
    Ok(None)
}

fn wait_for<T>(
    fd: &Fd,
    deadline: Instant,
    deframer: &mut Deframer,
    classify: &impl Fn(&[u8]) -> Option<T>,
) -> io::Result<Option<T>> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        // End of file is the board going away — rebooting, unplugged — and
        // waiting the rest of the window out would only be a spin.
        let Some(chunk) = fd.read_available(remaining)? else {
            return Ok(None);
        };
        for result in deframer.process(&chunk) {
            if let DeframeResult::Frame(data) = result {
                if let Some(answer) = classify(&data) {
                    return Ok(Some(answer));
                }
            }
        }
    }
}

/// Ask the firmware what it accepts. `Ok(None)` is firmware from before
/// the envelope: the query is 5 bytes, shorter than any Reticulum packet,
/// so old firmware drops it silently and the probe times out.
pub fn probe_capabilities(fd: &Fd) -> io::Result<Option<Capabilities>> {
    transact(fd, &encode_capability_query(), PROBE_TIMING, |data| {
        let frame = decode_frame(data).ok()?;
        if frame.frame_type != TYPE_CAPABILITY_REPORT {
            return None;
        }
        let (version, accepted) = decode_capability_report_payload(frame.payload)?;
        Some(Capabilities {
            version,
            accepted: accepted.to_vec(),
        })
    })
}

/// The answer-classifier every command frame shares: its own typed ack,
/// or its own typed refusal. Answers about other frame types stay
/// unclaimed so a late ack from an earlier conversation cannot be
/// mistaken for this one's.
fn command_answer(expected_type: u8) -> impl Fn(&[u8]) -> Option<ControlOutcome> {
    move |data| {
        let frame = decode_frame(data).ok()?;
        match frame.frame_type {
            TYPE_ACK if decode_ack_payload(frame.payload) == Some(expected_type) => {
                Some(ControlOutcome::Acked)
            }
            TYPE_REFUSAL => match decode_refusal_payload(frame.payload) {
                Some((refused, reason)) if refused == expected_type => {
                    Some(ControlOutcome::Refused { reason })
                }
                _ => None,
            },
            _ => None,
        }
    }
}

/// Tell the board what time it is (#166 item 2, `TYPE_WALL_TIME`).
pub fn send_wall_time(fd: &Fd, unix_secs: u64) -> io::Result<ControlOutcome> {
    let payload = encode_wall_time(unix_secs);
    let outcome = transact(
        fd,
        &payload,
        CONTROL_TIMING,
        command_answer(leviculum_core::envelope::TYPE_WALL_TIME),
    )?;
    Ok(outcome.unwrap_or(ControlOutcome::NoAnswer))
}

/// Set or clear the telemetry target (#236, `TYPE_TELEMETRY_TARGET`).
///
/// The public key is optional and its absence is the common case: a user
/// knows the LXMF address, and the node resolves the key over the air.
/// Clearing is the same frame with [`crate::telemetry::clear_target`], so
/// "off" travels the one path "on" does. The frame is longer than the
/// 19-byte Reticulum minimum, so like the radio config it must only be
/// sent through [`probed`] — against older firmware it would be
/// packet-shaped noise rather than a named refusal.
pub fn send_telemetry_target(fd: &Fd, target: &TelemetryTargetWire) -> io::Result<ControlOutcome> {
    let payload = encode_telemetry_target(target);
    let outcome = transact(
        fd,
        &payload,
        CONTROL_TIMING,
        command_answer(leviculum_core::envelope::TYPE_TELEMETRY_TARGET),
    )?;
    Ok(outcome.unwrap_or(ControlOutcome::NoAnswer))
}

/// Set or clear the user-set fixed position (`TYPE_FIXED_POSITION`).
///
/// `None` is the explicit clear: the board returns to sensor reporting.
/// The set frame is 19 bytes — exactly Reticulum's minimum packet size —
/// so like the radio config it must only be sent through [`probed`];
/// against older firmware it would be packet-shaped noise rather than a
/// named refusal.
pub fn send_fixed_position(
    fd: &Fd,
    position: Option<&FixedPositionWire>,
) -> io::Result<ControlOutcome> {
    let payload = encode_fixed_position(position);
    let outcome = transact(
        fd,
        &payload,
        CONTROL_TIMING,
        command_answer(leviculum_core::envelope::TYPE_FIXED_POSITION),
    )?;
    Ok(outcome.unwrap_or(ControlOutcome::NoAnswer))
}

/// Set the on-air transmit spacing (#345, `TYPE_TX_SPACING`).
///
/// A bench instrument: the value is what the board's LoRa interface leaves
/// between the end of one packet's airtime and the key-up of the next, and
/// it is deliberately not persisted, so a reset puts the board back on the
/// compiled default. The frame is 7 bytes — shorter than the 19-byte
/// Reticulum minimum — so, like the wall time, it cannot be mistaken for a
/// packet by firmware that does not know the type; it still goes through
/// [`probed`] on the flow path so an old board is reported as old rather
/// than as silent.
pub fn send_tx_spacing(fd: &Fd, spacing_ms: u16) -> io::Result<ControlOutcome> {
    let payload = encode_tx_spacing(spacing_ms);
    let outcome = transact(
        fd,
        &payload,
        CONTROL_TIMING,
        command_answer(leviculum_core::envelope::TYPE_TX_SPACING),
    )?;
    Ok(outcome.unwrap_or(ControlOutcome::NoAnswer))
}

/// Ask the board to announce its LXMF delivery destination now (#376,
/// `TYPE_ANNOUNCE`).
///
/// One-shot, nothing persisted: the board runs exactly the announce its
/// telemetry path sends before a report — same destination, same app
/// data, same clock gate. A board without a calendar clock refuses with
/// [`leviculum_core::envelope::REFUSE_NO_CLOCK`] and says
/// `[ANNOUNCE] withheld reason=no-clock` on its debug port. The frame is
/// 5 bytes — under the 19-byte Reticulum minimum — so old firmware
/// cannot mistake it for a packet; it still goes through [`probed`] on
/// the flow path so an old board is reported as old rather than as
/// silent.
pub fn send_announce(fd: &Fd) -> io::Result<ControlOutcome> {
    let payload = leviculum_core::envelope::encode_announce();
    let outcome = transact(
        fd,
        &payload,
        CONTROL_TIMING,
        command_answer(leviculum_core::envelope::TYPE_ANNOUNCE),
    )?;
    Ok(outcome.unwrap_or(ControlOutcome::NoAnswer))
}

/// Set the BLE inter-packet transmit gap (#376, `TYPE_BLE_TX_GAP`).
///
/// The BLE sibling of [`send_tx_spacing`], and volatile like it: the
/// board leaves at least this gap between the last fragment of one
/// packet and the first fragment of the next on the same connection,
/// and a reset restores 0. Values above
/// [`leviculum_core::envelope::BLE_TX_GAP_MAX_MS`] are refused by the
/// board with a named value refusal — the CLI additionally refuses them
/// at the command line, so the wire only ever carries the board bound's
/// worth.
pub fn send_ble_tx_gap(fd: &Fd, gap_ms: u16) -> io::Result<ControlOutcome> {
    let payload = leviculum_core::envelope::encode_ble_tx_gap(gap_ms);
    let outcome = transact(
        fd,
        &payload,
        CONTROL_TIMING,
        command_answer(leviculum_core::envelope::TYPE_BLE_TX_GAP),
    )?;
    Ok(outcome.unwrap_or(ControlOutcome::NoAnswer))
}

/// What a board said about its media profile: the carriers it is running
/// right now and the ones a reboot would come up with.
///
/// Two values because they honestly differ — see
/// [`leviculum_core::envelope::TYPE_MEDIA_REPORT`]. `configured` without
/// `running` is the board saying "that carrier did not come up this boot
/// and cannot be started now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaState {
    pub running: MediaProfileWire,
    pub configured: MediaProfileWire,
}

impl MediaState {
    /// Whether a reboot is needed for the configured profile to be the
    /// running one. Only ever true for a carrier being switched **on**:
    /// switching one off takes effect at once.
    pub fn needs_reboot(self) -> bool {
        self.running != self.configured
    }
}

/// The answer classifier the two media conversations share: a media
/// report, or a named refusal of the frame that was sent. Answers about
/// other frame types stay unclaimed, exactly like [`command_answer`].
///
/// `expect_configured` is the correlation an ack gets for free and this
/// report cannot: `TYPE_MEDIA_REPORT` carries no "in reply to" field, so
/// a report is only recognisable as *this* conversation's by what it
/// says. After a set it must state the profile that was sent — the board
/// applies it before it answers, and only reports once the record is
/// durable — so a report saying anything else is a leftover from before
/// the write and is left unclaimed for the next attempt. A query has
/// nothing to correlate against and passes `None`; there, the stale
/// frame is handled by draining the port in [`transact`].
fn media_answer(
    sent_type: u8,
    expect_configured: Option<MediaProfileWire>,
) -> impl Fn(&[u8]) -> Option<Result<MediaState, u8>> {
    move |data| {
        let frame = decode_frame(data).ok()?;
        match frame.frame_type {
            TYPE_MEDIA_REPORT => {
                let (running, configured) = decode_media_report_payload(frame.payload)?;
                if expect_configured.is_some_and(|expected| expected != configured) {
                    return None;
                }
                Some(Ok(MediaState {
                    running,
                    configured,
                }))
            }
            TYPE_REFUSAL => match decode_refusal_payload(frame.payload) {
                Some((refused, reason)) if refused == sent_type => Some(Err(reason)),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Set the media profile — which carriers the node meshes over
/// (`TYPE_MEDIA_PROFILE`).
///
/// Answered with a report rather than an ack on purpose: a carrier
/// that did not come up at boot cannot start before the next reset, and the report's
/// `running`/`configured` difference is the board saying so. The frame is
/// 6 bytes — under the 19-byte Reticulum minimum — so firmware that does
/// not know the type cannot mistake it for a packet; it still travels
/// behind [`probed`] on the flow path so an old board is reported as old
/// rather than as silent.
///
/// The accepted report has to state the profile that was just written:
/// the board applies before it answers, so any other report is a leftover
/// from before the write, and taking it would tell the operator the board
/// is on the profile the set was meant to replace.
pub fn send_media_profile(
    fd: &Fd,
    profile: &MediaProfileWire,
) -> io::Result<Result<MediaState, ControlOutcome>> {
    let payload = encode_media_profile(profile);
    Ok(
        match transact(
            fd,
            &payload,
            CONTROL_TIMING,
            media_answer(TYPE_MEDIA_PROFILE, Some(*profile)),
        )? {
            Some(Ok(state)) => Ok(state),
            Some(Err(reason)) => Err(ControlOutcome::Refused { reason }),
            None => Err(ControlOutcome::NoAnswer),
        },
    )
}

/// Ask the board which carriers it meshes over (`TYPE_MEDIA_QUERY`).
///
/// `Ok(Err(..))` is "no report came back": firmware without the query, or
/// a binary that carries no media gate and refused by name. Either way
/// the caller has no profile, and the one thing it must not do is invent
/// one — a board whose declared media are guessed at is a board whose
/// measurements are worthless.
pub fn query_media_profile(fd: &Fd) -> io::Result<Result<MediaState, ControlOutcome>> {
    Ok(
        match transact(
            fd,
            &encode_media_query(),
            CONTROL_TIMING,
            media_answer(TYPE_MEDIA_QUERY, None),
        )? {
            Some(Ok(state)) => Ok(state),
            Some(Err(reason)) => Err(ControlOutcome::Refused { reason }),
            None => Err(ControlOutcome::NoAnswer),
        },
    )
}

/// The answer classifier the two node-name conversations share: a name
/// report, or a named refusal of the frame that was sent.
///
/// `expect` is the correlation an ack gets for free and this report
/// cannot: `TYPE_NODE_NAME_REPORT` carries no "in reply to" field, so a
/// report is only recognisable as *this* conversation's by what it says.
/// After a set it must state the mesh name that was sent — the board
/// applies before it answers, and only answers once the record is durable
/// — so a report saying anything else is a leftover from before the write
/// and is left unclaimed for the next attempt. This is the media report's
/// rule (`media_answer`) on a second unaddressed report, and it is the
/// same bug it was written for: a stale frame taken as this one's tells
/// the operator the board carries a name it does not.
///
/// A *clear* has nothing to correlate against — the host cannot compute
/// the board's derived default, which is the whole reason the report
/// carries resolved names — so it passes `None` and relies on
/// [`transact`] draining the port first.
fn node_name_answer(
    sent_type: u8,
    expect: Option<&NodeName>,
) -> impl Fn(&[u8]) -> Option<Result<NodeNameState, u8>> + '_ {
    move |data| {
        let frame = decode_frame(data).ok()?;
        match frame.frame_type {
            TYPE_NODE_NAME_REPORT => {
                let state = decode_node_name_report_payload(frame.payload)?;
                if expect.is_some_and(|expected| *expected != state.mesh) {
                    return None;
                }
                Some(Ok(state))
            }
            TYPE_REFUSAL => match decode_refusal_payload(frame.payload) {
                Some((refused, reason)) if refused == sent_type => Some(Err(reason)),
                _ => None,
            },
            _ => None,
        }
    }
}

/// Set the node's name — `None` clears it back to the derived default
/// (`TYPE_NODE_NAME`, Codeberg #235).
///
/// Answered with a report rather than an ack on purpose: the two name
/// surfaces do not adopt it at the same moment. The mesh name is in force
/// for the next announce; the BLE advertisement was built at boot and
/// says so through the report's `ble_pending` flag. The frame is at most
/// 38 bytes, so it travels behind [`probed`] on the flow path — over the
/// 19-byte Reticulum minimum, it must never be sent on a guess.
pub fn send_node_name(
    fd: &Fd,
    name: Option<&NodeName>,
) -> io::Result<Result<NodeNameState, ControlOutcome>> {
    let payload = encode_node_name(name);
    Ok(
        match transact(
            fd,
            &payload,
            CONTROL_TIMING,
            node_name_answer(TYPE_NODE_NAME, name),
        )? {
            Some(Ok(state)) => Ok(state),
            Some(Err(reason)) => Err(ControlOutcome::Refused { reason }),
            None => Err(ControlOutcome::NoAnswer),
        },
    )
}

/// Ask the board what it is called (`TYPE_NODE_NAME_QUERY`).
///
/// `Ok(Err(..))` is "no report came back": firmware without the query, a
/// binary that carries no name gate and refused by name, or a board still
/// in the boot window before its identity is known (`REFUSE_BUSY`).
/// Either way the caller has no names, and the one thing it must not do
/// is invent them — the derived defaults are two different strings built
/// from a hash this side never sees.
pub fn query_node_name(fd: &Fd) -> io::Result<Result<NodeNameState, ControlOutcome>> {
    Ok(
        match transact(
            fd,
            &encode_node_name_query(),
            CONTROL_TIMING,
            node_name_answer(TYPE_NODE_NAME_QUERY, None),
        )? {
            Some(Ok(state)) => Ok(state),
            Some(Err(reason)) => Err(ControlOutcome::Refused { reason }),
            None => Err(ControlOutcome::NoAnswer),
        },
    )
}

/// Ask the board for its identity hashes (`TYPE_IDENTITY_QUERY`).
///
/// `Ok(Err(..))` is "no report came back": firmware from before the
/// query refuses it by name (`REFUSE_UNKNOWN_TYPE`), a board still in
/// the boot window before its node exists answers `REFUSE_BUSY`, and
/// pre-envelope firmware never answers at all. The caller must not
/// derive the hashes itself — the board reports what it actually
/// registered, including "no probe responder".
pub fn query_identity(fd: &Fd) -> io::Result<Result<IdentityReportWire, ControlOutcome>> {
    Ok(
        match transact(fd, &encode_identity_query(), CONTROL_TIMING, |data| {
            let frame = decode_frame(data).ok()?;
            match frame.frame_type {
                TYPE_IDENTITY_REPORT => Some(Ok(decode_identity_report_payload(frame.payload)?)),
                TYPE_REFUSAL => match decode_refusal_payload(frame.payload) {
                    Some((refused, reason)) if refused == TYPE_IDENTITY_QUERY => Some(Err(reason)),
                    _ => None,
                },
                _ => None,
            }
        })? {
            Some(Ok(report)) => Ok(report),
            Some(Err(reason)) => Err(ControlOutcome::Refused { reason }),
            None => Err(ControlOutcome::NoAnswer),
        },
    )
}

/// What a board answered about its position sources — the second clause
/// of the telemetry send condition.
///
/// Not a bare bool: "no pin but a receiver" is a node that reports as soon
/// as it is switched on, and "neither" is a node an operator has to do
/// something about, so the two must not collapse into one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionSources {
    pub fixed: bool,
    pub gnss: bool,
}

impl PositionSources {
    fn from_flags(flags: u8) -> Self {
        Self {
            fixed: flags & leviculum_core::envelope::POSITION_SOURCE_FIXED != 0,
            gnss: flags & leviculum_core::envelope::POSITION_SOURCE_GNSS != 0,
        }
    }

    /// Whether the board will send telemetry at all once it has a target.
    pub fn any(self) -> bool {
        self.fixed || self.gnss
    }
}

/// Ask the board whether it has a position source
/// (`TYPE_POSITION_SOURCE_QUERY`).
///
/// `Ok(Err(..))` is "no report came back": firmware without the query, or a
/// binary with no reporter that refused by name. The caller must then say
/// nothing about position sources rather than guess — telling an operator
/// their board will stay silent when it will not is worse than the ack on
/// its own.
pub fn query_position_sources(fd: &Fd) -> io::Result<Result<PositionSources, ControlOutcome>> {
    Ok(
        match transact(
            fd,
            &encode_position_source_query(),
            CONTROL_TIMING,
            |data| {
                let frame = decode_frame(data).ok()?;
                match frame.frame_type {
                    TYPE_POSITION_SOURCE_REPORT => Some(Ok(PositionSources::from_flags(
                        decode_position_source_report_payload(frame.payload)?,
                    ))),
                    TYPE_REFUSAL => match decode_refusal_payload(frame.payload) {
                        Some((refused, reason)) if refused == TYPE_POSITION_SOURCE_QUERY => {
                            Some(Err(reason))
                        }
                        _ => None,
                    },
                    _ => None,
                }
            },
        )? {
            Some(Ok(sources)) => Ok(sources),
            Some(Err(reason)) => Err(ControlOutcome::Refused { reason }),
            None => Err(ControlOutcome::NoAnswer),
        },
    )
}

/// Send the radio configuration as an envelope frame. Only for firmware
/// whose capability report includes `TYPE_RADIO_CONFIG`: the frame is
/// longer than the 19-byte Reticulum minimum, so it must never be sent
/// on a guess.
pub fn send_radio_config(fd: &Fd, cfg: &RadioConfigWire) -> io::Result<ControlOutcome> {
    let payload = encode_radio_config(cfg);
    let outcome = transact(
        fd,
        &payload,
        CONTROL_TIMING,
        command_answer(leviculum_core::envelope::TYPE_RADIO_CONFIG),
    )?;
    Ok(outcome.unwrap_or(ControlOutcome::NoAnswer))
}

/// Ask the board what its radio is running (#349, `TYPE_RADIO_QUERY`).
///
/// `Ok(None)` is "no report came back": firmware without the query, or a
/// board whose radio has not come up yet and answered `REFUSE_BUSY`. Either
/// way the caller has no current settings, and the one thing it must not do
/// is invent them — a config frame carries the whole parameter set, so
/// substituting defaults for the fields it did not mean to touch is how a
/// power sweep quietly resets the bandwidth.
pub fn query_radio_config(fd: &Fd) -> io::Result<Option<RadioConfigWire>> {
    transact(
        fd,
        &leviculum_core::envelope::encode_radio_query(),
        CONTROL_TIMING,
        |data| {
            let frame = decode_frame(data).ok()?;
            if frame.frame_type != leviculum_core::envelope::TYPE_RADIO_REPORT {
                return None;
            }
            leviculum_core::envelope::decode_radio_report_payload(frame.payload)
        },
    )
}

/// The scripted boards every control-plane test drives.
///
/// Shared rather than per-test-module on purpose: a stub is a claim about
/// what a real board does, and two copies of that claim drift. The telemetry
/// prompts ([`crate::telemetry`]) and the wall-time session talk to the same
/// firmware, so they talk to the same stub.
#[cfg(test)]
pub(crate) mod testing {
    use crate::sys::testpty::{spawn_stub, spawn_stub_delayed, Pty};
    use leviculum_core::constants::EMISSION_PLAUSIBLE_MIN_SECS;
    use leviculum_core::envelope::{
        classify_control_frame, encode_ack, encode_capability_report, encode_media_report,
        encode_radio_report, encode_refusal, fixed_position_answer, identity_query_answer,
        media_profile_answer, media_query_answer, node_name_answer, node_name_query_answer,
        position_source_query_answer, telemetry_target_answer, ControlAction, IdentityReportWire,
        MediaProfileWire, Persist, NODE_NAME_FLAG_BLE_PENDING, NODE_NAME_FLAG_STORED,
        POSITION_SOURCE_FIXED, POSITION_SOURCE_GNSS, TYPE_ANNOUNCE, TYPE_BLE_TX_GAP,
        TYPE_CAPABILITIES, TYPE_FIXED_POSITION, TYPE_IDENTITY_QUERY, TYPE_MEDIA_PROFILE,
        TYPE_MEDIA_QUERY, TYPE_NODE_NAME, TYPE_NODE_NAME_QUERY, TYPE_POSITION_SOURCE_QUERY,
        TYPE_RADIO_CONFIG, TYPE_RADIO_QUERY, TYPE_RESET, TYPE_TELEMETRY_TARGET, TYPE_TX_SPACING,
        TYPE_WALL_TIME,
    };
    use leviculum_core::node_name::{truncate_on_char_boundary, NodeName, BLE_NAME_MAX_LEN};
    use leviculum_core::rnode::{RadioConfigWire, RADIO_CONFIG_ACK};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Every frame the stub was handed, for tests that assert on the bytes
    /// that actually reached the device.
    pub type Seen = Arc<Mutex<Vec<Vec<u8>>>>;

    pub fn seen() -> Seen {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// The accepted list the current firmware advertises. Must stay
    /// identical to `leviculum_nrf::usb::ACCEPTED_CONTROL_TYPES` — a stub
    /// that accepts more than the board does proves the host against a
    /// device that does not exist.
    pub const FIRMWARE_ACCEPTS: &[u8] = &[
        TYPE_RADIO_CONFIG,
        TYPE_RESET,
        TYPE_WALL_TIME,
        TYPE_CAPABILITIES,
        TYPE_TELEMETRY_TARGET,
        TYPE_TX_SPACING,
        TYPE_RADIO_QUERY,
        TYPE_FIXED_POSITION,
        TYPE_MEDIA_PROFILE,
        TYPE_MEDIA_QUERY,
        TYPE_POSITION_SOURCE_QUERY,
        TYPE_NODE_NAME,
        TYPE_NODE_NAME_QUERY,
        TYPE_IDENTITY_QUERY,
        TYPE_ANNOUNCE,
        TYPE_BLE_TX_GAP,
    ];

    /// The scripted board's probe and LXMF destination hashes. Distinct
    /// from [`STUB_IDENTITY_HASH`] and from each other, so a host that
    /// printed one hash under another's label shows up as a changed
    /// value rather than as a coincidence.
    pub const STUB_PROBE_HASH: [u8; 16] = [
        0x0c, 0x21, 0x92, 0xe5, 0x00, 0x75, 0x3e, 0xb4, 0x2e, 0x1e, 0x42, 0x2f, 0xe3, 0x32, 0x2a,
        0x1d,
    ];
    pub const STUB_LXMF_HASH: [u8; 16] = [
        0x9b, 0x42, 0x1c, 0x5a, 0x9c, 0x86, 0x20, 0x2b, 0x3d, 0x7a, 0x59, 0xd4, 0xd3, 0x4a, 0x3c,
        0x8b,
    ];

    /// The position sources the scripted board has. A cell rather than a
    /// constant for the same reason [`StubMedia`] is one: setting a fixed
    /// position has to be *visible* to the next query on the same board,
    /// which is what makes "set a pin and the board stops being silent"
    /// testable without a board.
    pub fn position_sources(flags: u8) -> Arc<Mutex<u8>> {
        Arc::new(Mutex::new(flags))
    }

    /// The scripted board's default: a receiver built in, no pin set —
    /// the RAK with its GNSS task, which is the board that reports out of
    /// the box.
    pub fn gnss_board() -> Arc<Mutex<u8>> {
        position_sources(POSITION_SOURCE_GNSS)
    }

    /// The media profile the scripted board boots with, and the one it
    /// currently runs. A stub-wide cell rather than a constant: a media
    /// set has to be *visible* to the next query on the same board, which
    /// is what makes the read-modify-write of a one-sided `--set-media`
    /// testable at all.
    ///
    /// The stub reproduces the firmware's own running-versus-configured
    /// rule ([`leviculum_nrf::media`]): switching a carrier off takes
    /// effect at once; a carrier that never came up cannot start, so
    /// `running` can only ever lose bits relative to what booted.
    #[derive(Debug, Clone, Copy)]
    pub struct StubMedia {
        booted: MediaProfileWire,
        configured: MediaProfileWire,
    }

    impl StubMedia {
        fn running(&self) -> MediaProfileWire {
            MediaProfileWire {
                lora_enabled: self.booted.lora_enabled && self.configured.lora_enabled,
                ble_enabled: self.booted.ble_enabled && self.configured.ble_enabled,
            }
        }
    }

    /// The scripted board's name state: the identity hash its derived
    /// defaults are built from, the name stored on its flash page, and
    /// the BLE name the current boot came up on.
    ///
    /// A cell rather than a constant for the same reason [`StubMedia`] is
    /// one: setting a name has to be *visible* to the next query on the
    /// same board, and the BLE name has to stay stuck at what the boot
    /// built — which is the whole behaviour the report exists to state.
    #[derive(Debug, Clone)]
    pub struct StubName {
        identity_hash: [u8; 16],
        stored: Option<NodeName>,
        booted_ble: String,
    }

    impl StubName {
        /// The firmware's own rule, reproduced: the mesh name follows the
        /// stored name at once, the BLE name is whatever the boot built.
        fn mesh(&self) -> NodeName {
            match &self.stored {
                Some(name) => *name,
                None => NodeName::decode(derived_mesh(&self.identity_hash).as_bytes())
                    .expect("derived names are graphic ASCII"),
            }
        }

        fn ble(&self) -> NodeName {
            NodeName::decode(self.booted_ble.as_bytes()).expect("a boot name is displayable")
        }

        /// What a reset would put on the BLE surfaces, given what is
        /// stored right now.
        fn pending_ble(&self) -> String {
            match &self.stored {
                Some(name) => {
                    truncate_on_char_boundary(name.as_str(), BLE_NAME_MAX_LEN).to_string()
                }
                None => derived_ble(&self.identity_hash),
            }
        }

        fn flags(&self) -> u8 {
            let stored = if self.stored.is_some() {
                NODE_NAME_FLAG_STORED
            } else {
                0
            };
            let pending = if self.booted_ble == self.pending_ble() {
                0
            } else {
                NODE_NAME_FLAG_BLE_PENDING
            };
            stored | pending
        }
    }

    /// `LNode-<hex8>` — the mesh default (`leviculum_nrf::name::mesh_name`).
    fn derived_mesh(hash: &[u8; 16]) -> String {
        format!("LNode-{}", hex8(hash))
    }

    /// `LN-<hex8>` — the BLE default (`leviculum_ble_tx::device_name`).
    /// Deliberately a *different* string from the mesh default, which is
    /// why `ble_pending` is a flag the board sets rather than something
    /// the host derives.
    fn derived_ble(hash: &[u8; 16]) -> String {
        format!("LN-{}", hex8(hash))
    }

    fn hex8(hash: &[u8; 16]) -> String {
        hash[..4].iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The scripted board's hash. Not all-zero: a derived name built from
    /// the wrong hash has to look wrong.
    pub const STUB_IDENTITY_HASH: [u8; 16] = [
        0xa1, 0xb2, 0xc3, 0xd4, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99, 0x88, 0x77, 0x66, 0x55,
        0x44,
    ];

    /// A board that booted unnamed — the state every fielded board is in.
    pub fn name_state() -> Arc<Mutex<StubName>> {
        name_state_booted(None)
    }

    /// A board that came up carrying `booted` — the state a board is in
    /// after being named and reset.
    pub fn name_state_booted(booted: Option<&str>) -> Arc<Mutex<StubName>> {
        let stored = booted.map(|n| NodeName::parse(n.as_bytes()).expect("a valid fixture name"));
        let booted_ble = match booted {
            Some(n) => truncate_on_char_boundary(n, BLE_NAME_MAX_LEN).to_string(),
            None => derived_ble(&STUB_IDENTITY_HASH),
        };
        Arc::new(Mutex::new(StubName {
            identity_hash: STUB_IDENTITY_HASH,
            stored,
            booted_ble,
        }))
    }

    /// A board that booted on both carriers, which is the default and the
    /// state every fielded board is in.
    pub fn media_state() -> Arc<Mutex<StubMedia>> {
        media_state_booted(MediaProfileWire::BOTH)
    }

    /// A board that came up on `booted` — the state a board is in after
    /// being flashed with a profile and reset. A carrier absent from
    /// `booted` never started this boot and cannot be started before the
    /// next reset, whatever is configured.
    pub fn media_state_booted(booted: MediaProfileWire) -> Arc<Mutex<StubMedia>> {
        Arc::new(Mutex::new(StubMedia {
            booted,
            configured: booted,
        }))
    }

    /// What the scripted board's radio is running.
    ///
    /// Deliberately not the eu868 default any part of `lnflash` would
    /// substitute: SF9 and 250 kHz are this fixture's and nothing else's, so
    /// a flow that quietly rebuilt the config from its own defaults instead
    /// of from the report shows up as a changed number rather than as a
    /// coincidence.
    pub fn stub_running_config() -> RadioConfigWire {
        RadioConfigWire {
            frequency_hz: 867_500_000,
            bandwidth_hz: 250_000,
            sf: 9,
            cr: 6,
            tx_power_dbm: 14,
            preamble_len: 20,
            csma_enabled: true,
            radio_silent: false,
            st_alock: 1_500,
            lt_alock: 250,
            lt_alock_present: true,
        }
    }

    /// The accepted list of firmware from before #236 landed its
    /// telemetry consumer: everything else, and a named refusal for the
    /// target frame. This is how a #236-aware host detects an old board.
    pub const PRE_236_ACCEPTS: &[u8] = &[
        TYPE_RADIO_CONFIG,
        TYPE_RESET,
        TYPE_WALL_TIME,
        TYPE_CAPABILITIES,
    ];

    /// A scripted device running the firmware's actual decision function
    /// (`classify_control_frame`) — the stub answers exactly what the
    /// usb.rs accept path would answer, so these tests exercise the same
    /// contract the board implements.
    pub fn envelope_firmware_stub(pty: &Pty, seen: Seen) {
        envelope_firmware_stub_with_media(pty, seen, media_state());
    }

    /// [`envelope_firmware_stub`] with the name state handed in, so a
    /// test can watch a name change across two conversations on the same
    /// board — and watch the BLE half stay stuck at what the boot built.
    pub fn envelope_firmware_stub_with_name(pty: &Pty, seen: Seen, name: Arc<Mutex<StubName>>) {
        envelope_firmware_stub_full_named(pty, seen, media_state(), gnss_board(), name);
    }

    /// [`envelope_firmware_stub`] with the media state handed in, so a
    /// test can watch a profile change across two conversations on the
    /// same board — the read-modify-write a one-sided `--set-media` does.
    pub fn envelope_firmware_stub_with_media(pty: &Pty, seen: Seen, media: Arc<Mutex<StubMedia>>) {
        envelope_firmware_stub_full(pty, seen, media, gnss_board());
    }

    /// [`envelope_firmware_stub`] on a board with no position source at
    /// all: no receiver, no pin. It takes a telemetry target — the target
    /// is valid configuration — and reports zero sources, which is the
    /// board `--set-telemetry` owes a consequence sentence.
    pub fn positionless_firmware_stub(pty: &Pty, seen: Seen) {
        envelope_firmware_stub_full(pty, seen, media_state(), position_sources(0));
    }

    /// The scripted board with both cells handed in.
    pub fn envelope_firmware_stub_full(
        pty: &Pty,
        seen: Seen,
        media: Arc<Mutex<StubMedia>>,
        sources: Arc<Mutex<u8>>,
    ) {
        envelope_firmware_stub_full_named(pty, seen, media, sources, name_state());
    }

    /// The scripted board with all three cells handed in.
    pub fn envelope_firmware_stub_full_named(
        pty: &Pty,
        seen: Seen,
        media: Arc<Mutex<StubMedia>>,
        sources: Arc<Mutex<u8>>,
        name: Arc<Mutex<StubName>>,
    ) {
        spawn_stub(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, FIRMWARE_ACCEPTS) {
                ControlAction::CapabilityQuery => Some(encode_capability_report(FIRMWARE_ACCEPTS)),
                ControlAction::WallTime(unix) => Some(if unix >= EMISSION_PLAUSIBLE_MIN_SECS {
                    encode_ack(TYPE_WALL_TIME)
                } else {
                    encode_refusal(TYPE_WALL_TIME, super::REFUSE_VALUE)
                }),
                ControlAction::RadioConfig(_) => Some(encode_ack(TYPE_RADIO_CONFIG)),
                ControlAction::RadioQuery => Some(encode_radio_report(&stub_running_config())),
                // The firmware's own answer functions, reporter wired and
                // the channel taking the frame — the ack direction of the
                // capability gate.
                ControlAction::TelemetryTarget(_) => {
                    Some(telemetry_target_answer(true, true, Persist::Durable))
                }
                // The pin IS a position source, so setting one changes what
                // the next query answers — the runtime path out of
                // `state=no-position-source`, reproduced here so the host
                // side of it can be driven without a board.
                ControlAction::FixedPosition(position) => {
                    let mut sources = sources.lock().unwrap();
                    if position.is_some() {
                        *sources |= POSITION_SOURCE_FIXED;
                    } else {
                        *sources &= !POSITION_SOURCE_FIXED;
                    }
                    Some(fixed_position_answer(true, true, Persist::Durable))
                }
                ControlAction::PositionSourceQuery => {
                    Some(position_source_query_answer(true, *sources.lock().unwrap()))
                }
                ControlAction::TxSpacing(_) => Some(encode_ack(TYPE_TX_SPACING)),
                // The scripted board has a calendar clock, so a host
                // announce request is acked; the clockless case is its
                // own stub ([`clockless_firmware_stub`]), because the
                // no-clock refusal is the answer the desk most needs to
                // read correctly.
                ControlAction::AnnounceNow => Some(encode_ack(TYPE_ANNOUNCE)),
                ControlAction::BleTxGap(_) => Some(encode_ack(TYPE_BLE_TX_GAP)),
                // The media gate, run exactly as the board runs it: apply,
                // then answer from the state that is already in force.
                ControlAction::MediaProfile(profile) => {
                    let mut media = media.lock().unwrap();
                    media.configured = profile;
                    Some(media_profile_answer(
                        true,
                        true,
                        Persist::Durable,
                        media.running(),
                        media.configured,
                    ))
                }
                ControlAction::MediaQuery => {
                    let media = media.lock().unwrap();
                    Some(media_query_answer(true, media.running(), media.configured))
                }
                // The name gate, run exactly as the board runs it: apply,
                // then answer from the state that is already in force —
                // including the BLE half, which stays at what the boot
                // built until the board is reset.
                ControlAction::NodeName(chosen) => {
                    let mut name = name.lock().unwrap();
                    name.stored = chosen;
                    Some(node_name_answer(
                        true,
                        Persist::Durable,
                        Some((name.flags(), &name.mesh(), &name.ble())),
                    ))
                }
                ControlAction::NodeNameQuery => {
                    let name = name.lock().unwrap();
                    Some(node_name_query_answer(
                        true,
                        Some((name.flags(), &name.mesh(), &name.ble())),
                    ))
                }
                // The identity gate, run exactly as the board runs it:
                // the hashes the boot published, all three registered.
                ControlAction::IdentityQuery => {
                    Some(identity_query_answer(Some(&IdentityReportWire {
                        identity: STUB_IDENTITY_HASH,
                        probe: Some(STUB_PROBE_HASH),
                        lxmf: Some(STUB_LXMF_HASH),
                    })))
                }
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => Some(encode_refusal(refused_type, reason)),
                _ => None,
            }
        });
    }

    /// A scripted device whose binary never wired the name gate: it
    /// advertises the frame types (the envelope layer knows them) but its
    /// answer runs the firmware's own decision function with the
    /// capability absent, so the name comes back refused by name.
    pub fn nameless_firmware_stub(pty: &Pty, seen: Seen) {
        spawn_stub(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, FIRMWARE_ACCEPTS) {
                ControlAction::CapabilityQuery => Some(encode_capability_report(FIRMWARE_ACCEPTS)),
                ControlAction::NodeName(_) => Some(node_name_answer(false, Persist::Durable, None)),
                ControlAction::NodeNameQuery => Some(node_name_query_answer(false, None)),
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => Some(encode_refusal(refused_type, reason)),
                _ => None,
            }
        });
    }

    /// A scripted device still in its boot window: it knows the frames but
    /// its identity hash is not published yet, so it cannot state the
    /// derived names and says `busy` rather than reporting a name built
    /// from sixteen zero bytes.
    pub fn booting_firmware_stub(pty: &Pty, seen: Seen) {
        spawn_stub(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, FIRMWARE_ACCEPTS) {
                ControlAction::CapabilityQuery => Some(encode_capability_report(FIRMWARE_ACCEPTS)),
                ControlAction::NodeName(_) => Some(node_name_answer(true, Persist::Durable, None)),
                ControlAction::NodeNameQuery => Some(node_name_query_answer(true, None)),
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => Some(encode_refusal(refused_type, reason)),
                _ => None,
            }
        });
    }

    /// The node name the stub decoded, if a set frame reached it. The
    /// **last** one, like [`media_frame`]: a read-modify-write session
    /// must not be satisfied by an earlier attempt. The outer `Option` is
    /// "did a frame arrive at all", the inner is the frame's own
    /// set-versus-clear.
    pub fn node_name_frame(seen: &Seen) -> Option<Option<NodeName>> {
        seen.lock().unwrap().iter().rev().find_map(|f| {
            match classify_control_frame(f, FIRMWARE_ACCEPTS) {
                ControlAction::NodeName(name) => Some(name),
                _ => None,
            }
        })
    }

    /// A scripted board that is still repeating an earlier media report
    /// while the host has moved on to its next frame.
    ///
    /// The board answers everything correctly and, twenty milliseconds
    /// after each capability probe, puts one more copy of a *stale*
    /// report on the wire — the answer to a frame whose window had
    /// already run down, which `transact` retried. A real board does this
    /// whenever it is slower than `CONTROL_TIMING.window`; the delay is
    /// what makes the leftover land in the host's *next* transaction
    /// instead of being dropped with the current one's deframer.
    ///
    /// `stale` is deliberately a profile the board is not on, so a host
    /// that mistakes the leftover for its own answer reports a profile
    /// the board is not running.
    pub fn late_report_firmware_stub(
        pty: &Pty,
        seen: Seen,
        media: Arc<Mutex<StubMedia>>,
        stale: MediaProfileWire,
    ) {
        spawn_stub_delayed(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, FIRMWARE_ACCEPTS) {
                ControlAction::CapabilityQuery => vec![
                    (Duration::ZERO, encode_capability_report(FIRMWARE_ACCEPTS)),
                    (
                        Duration::from_millis(20),
                        encode_media_report(&stale, &stale),
                    ),
                ],
                ControlAction::MediaProfile(profile) => {
                    let mut media = media.lock().unwrap();
                    media.configured = profile;
                    vec![(
                        Duration::ZERO,
                        media_profile_answer(
                            true,
                            true,
                            Persist::Durable,
                            media.running(),
                            media.configured,
                        ),
                    )]
                }
                ControlAction::MediaQuery => {
                    let media = media.lock().unwrap();
                    vec![(
                        Duration::ZERO,
                        media_query_answer(true, media.running(), media.configured),
                    )]
                }
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => vec![(Duration::ZERO, encode_refusal(refused_type, reason))],
                _ => Vec::new(),
            }
        });
    }

    /// A scripted device whose binary never wired the media gate: it
    /// advertises the frame types (the envelope layer knows them) but its
    /// answer runs the firmware's own decision function with the
    /// capability absent, so the profile comes back refused by name.
    ///
    /// The fc60b95 rule again, and it matters most here: an ack for a
    /// profile that is not honoured is what a measurement run would read
    /// as "this node is single-medium now", and every number it then
    /// produced would be a lie.
    pub fn medialess_firmware_stub(pty: &Pty, seen: Seen) {
        spawn_stub(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, FIRMWARE_ACCEPTS) {
                ControlAction::CapabilityQuery => Some(encode_capability_report(FIRMWARE_ACCEPTS)),
                ControlAction::MediaProfile(_) => Some(media_profile_answer(
                    false,
                    false,
                    Persist::Durable,
                    MediaProfileWire::BOTH,
                    MediaProfileWire::BOTH,
                )),
                ControlAction::MediaQuery => Some(media_query_answer(
                    false,
                    MediaProfileWire::BOTH,
                    MediaProfileWire::BOTH,
                )),
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => Some(encode_refusal(refused_type, reason)),
                _ => None,
            }
        });
    }

    /// A scripted device whose binary never declared a telemetry reporter:
    /// it advertises the frame type (the envelope layer knows it) but its
    /// answer runs the firmware's own decision function with the
    /// capability absent, so the target comes back refused by name — the
    /// T114's pre-wiring dishonesty, made impossible. Deliberately not a
    /// board: the mechanism must hold for any reporter-less configuration,
    /// whichever BSPs happen to wire a reporter today.
    pub fn reporterless_firmware_stub(pty: &Pty, seen: Seen) {
        spawn_stub(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, FIRMWARE_ACCEPTS) {
                ControlAction::CapabilityQuery => Some(encode_capability_report(FIRMWARE_ACCEPTS)),
                ControlAction::TelemetryTarget(_) => {
                    Some(telemetry_target_answer(false, false, Persist::Durable))
                }
                ControlAction::FixedPosition(_) => {
                    Some(fixed_position_answer(false, false, Persist::Durable))
                }
                ControlAction::PositionSourceQuery => Some(position_source_query_answer(false, 0)),
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => Some(encode_refusal(refused_type, reason)),
                _ => None,
            }
        });
    }

    /// A scripted device running firmware from before #236: it speaks
    /// the envelope but has no telemetry consumer, so the target frame
    /// comes back refused by name rather than acked.
    pub fn pre_236_firmware_stub(pty: &Pty, seen: Seen) {
        spawn_stub(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, PRE_236_ACCEPTS) {
                ControlAction::CapabilityQuery => Some(encode_capability_report(PRE_236_ACCEPTS)),
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => Some(encode_refusal(refused_type, reason)),
                _ => None,
            }
        });
    }

    /// A scripted device running firmware from before the envelope: it
    /// answers the legacy config magic and nothing else — an envelope
    /// frame is packet-shaped noise to it.
    pub fn old_firmware_stub(pty: &Pty, seen: Seen) {
        spawn_stub(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, FIRMWARE_ACCEPTS) {
                ControlAction::LegacyRadioConfig(_) => Some(RADIO_CONFIG_ACK.to_vec()),
                _ => None,
            }
        });
    }

    /// The transmit spacing the stub decoded, if a #345 frame reached it.
    pub fn tx_spacing_frame(seen: &Seen) -> Option<u16> {
        seen.lock().unwrap().iter().find_map(|f| {
            match classify_control_frame(f, FIRMWARE_ACCEPTS) {
                ControlAction::TxSpacing(ms) => Some(ms),
                _ => None,
            }
        })
    }

    /// Whether a #376 announce request reached the stub.
    pub fn announce_frame_seen(seen: &Seen) -> bool {
        seen.lock().unwrap().iter().any(|f| {
            matches!(
                classify_control_frame(f, FIRMWARE_ACCEPTS),
                ControlAction::AnnounceNow
            )
        })
    }

    /// The BLE transmit gap the stub decoded, if a #376 frame reached it.
    pub fn ble_tx_gap_frame(seen: &Seen) -> Option<u16> {
        seen.lock().unwrap().iter().find_map(|f| {
            match classify_control_frame(f, FIRMWARE_ACCEPTS) {
                ControlAction::BleTxGap(ms) => Some(ms),
                _ => None,
            }
        })
    }

    /// A scripted device whose calendar clock is still unseeded: it runs
    /// the firmware's decision function, but the main-loop clock gate
    /// withholds the announce and answers the named `no-clock` refusal —
    /// the case the #376 desk most needs the tool to state correctly,
    /// because a fresh board on a bench with no GNSS view is exactly
    /// this board.
    pub fn clockless_firmware_stub(pty: &Pty, seen: Seen) {
        spawn_stub(pty, move |frame_bytes| {
            seen.lock().unwrap().push(frame_bytes.to_vec());
            match classify_control_frame(frame_bytes, FIRMWARE_ACCEPTS) {
                ControlAction::CapabilityQuery => Some(encode_capability_report(FIRMWARE_ACCEPTS)),
                ControlAction::AnnounceNow => Some(encode_refusal(
                    TYPE_ANNOUNCE,
                    leviculum_core::envelope::REFUSE_NO_CLOCK,
                )),
                ControlAction::Refuse {
                    refused_type,
                    reason,
                } => Some(encode_refusal(refused_type, reason)),
                _ => None,
            }
        });
    }

    /// The radio config the stub was sent, if a config frame reached it.
    pub fn radio_config_frame(seen: &Seen) -> Option<RadioConfigWire> {
        seen.lock().unwrap().iter().find_map(|f| {
            match classify_control_frame(f, FIRMWARE_ACCEPTS) {
                ControlAction::RadioConfig(cfg) => Some(cfg),
                _ => None,
            }
        })
    }

    /// The fixed-position payload the stub decoded, if one reached it.
    /// The outer `Option` is "did a frame arrive at all", the inner is
    /// the frame's own set-versus-clear.
    pub fn fixed_position_frame(
        seen: &Seen,
    ) -> Option<Option<leviculum_core::envelope::FixedPositionWire>> {
        seen.lock().unwrap().iter().find_map(|f| {
            match classify_control_frame(f, FIRMWARE_ACCEPTS) {
                ControlAction::FixedPosition(position) => Some(position),
                _ => None,
            }
        })
    }

    /// The media profile the stub decoded, if a set frame reached it.
    /// The **last** one, not the first: a read-modify-write session sends
    /// one frame, but a test that asserts on "what the board ended up
    /// being told" must not be satisfied by an earlier attempt.
    pub fn media_frame(seen: &Seen) -> Option<MediaProfileWire> {
        seen.lock().unwrap().iter().rev().find_map(|f| {
            match classify_control_frame(f, FIRMWARE_ACCEPTS) {
                ControlAction::MediaProfile(profile) => Some(profile),
                _ => None,
            }
        })
    }

    /// The telemetry-target payload the stub decoded, if one reached it.
    pub fn telemetry_frame(seen: &Seen) -> Option<leviculum_core::envelope::TelemetryTargetWire> {
        seen.lock().unwrap().iter().find_map(|f| {
            match classify_control_frame(f, FIRMWARE_ACCEPTS) {
                ControlAction::TelemetryTarget(target) => Some(target),
                _ => None,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::sys::testpty::Pty;
    use leviculum_core::envelope::{
        encode_media_report, encode_refusal, TELEMETRY_PROFILE_STATION, TYPE_TELEMETRY_TARGET,
        TYPE_WALL_TIME,
    };

    /// Short budgets so the negative tests do not sit out field windows.
    fn quick(fd_window_ms: u64) -> Timing {
        Timing {
            attempts: 1,
            window: Duration::from_millis(fd_window_ms),
        }
    }

    #[test]
    fn the_capability_probe_learns_what_the_firmware_accepts() {
        let pty = Pty::open();
        envelope_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        let caps = probe_capabilities(&fd).unwrap().unwrap();
        assert_eq!(caps.version, leviculum_core::envelope::ENVELOPE_VERSION);
        assert_eq!(caps.accepted, FIRMWARE_ACCEPTS);
        assert!(caps.accepts(TYPE_WALL_TIME));
        assert!(caps.accepts(TYPE_TELEMETRY_TARGET));
    }

    #[test]
    fn the_capability_probe_reports_silence_as_none_not_as_an_error() {
        let pty = Pty::open();
        old_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        let answer = transact(&fd, &encode_capability_query(), quick(200), |data| {
            decode_frame(data).ok().map(|f| f.frame_type)
        })
        .unwrap();
        assert_eq!(answer, None);
    }

    #[test]
    fn a_plausible_wall_time_is_acked() {
        let pty = Pty::open();
        envelope_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert_eq!(
            send_wall_time(&fd, 1_790_000_000).unwrap(),
            ControlOutcome::Acked
        );
    }

    #[test]
    fn an_implausible_wall_time_is_refused_by_name_not_by_timeout() {
        let pty = Pty::open();
        envelope_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        let started = Instant::now();
        assert_eq!(
            send_wall_time(&fd, 1_000).unwrap(),
            ControlOutcome::Refused {
                reason: REFUSE_VALUE
            }
        );
        // The named refusal must arrive as an answer, not as a run-down
        // window: well under one attempt window proves it was spoken.
        assert!(started.elapsed() < CONTROL_TIMING.window);
    }

    #[test]
    fn wall_time_against_old_firmware_times_out_to_no_answer() {
        let pty = Pty::open();
        old_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        let outcome = transact(
            &fd,
            &encode_wall_time(1_790_000_000),
            quick(200),
            command_answer(TYPE_WALL_TIME),
        )
        .unwrap();
        assert_eq!(outcome, None);
    }

    #[test]
    fn an_unknown_frame_type_comes_back_as_a_named_refusal() {
        let pty = Pty::open();
        envelope_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        let outcome = transact(
            &fd,
            &leviculum_core::envelope::encode_frame(0x6E, &[]),
            CONTROL_TIMING,
            command_answer(0x6E),
        )
        .unwrap();
        assert_eq!(
            outcome,
            Some(ControlOutcome::Refused {
                reason: REFUSE_UNKNOWN_TYPE
            })
        );
    }

    // -----------------------------------------------------------------
    // Telemetry target (Codeberg #236)
    // -----------------------------------------------------------------

    fn hash_only_target() -> TelemetryTargetWire {
        TelemetryTargetWire {
            profile: TELEMETRY_PROFILE_STATION,
            dest_hash: [0xA7; 16],
            public_key: None,
        }
    }

    #[test]
    fn a_hash_only_target_is_acked_by_a_236_firmware() {
        // The common case per the 2026-08-22 UX decision: the user knows
        // the address and nothing else.
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let target = hash_only_target();
        assert_eq!(
            send_telemetry_target(&fd, &target).unwrap(),
            ControlOutcome::Acked
        );

        // What went on the wire is what the board decoded: a hash-only
        // payload, key-present flag explicitly absent.
        let payload = telemetry_frame(&seen).expect("no telemetry target frame reached the stub");
        assert_eq!(payload, target);
        assert_eq!(payload.public_key, None);
    }

    #[test]
    fn a_target_with_a_key_is_acked_too() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let target = TelemetryTargetWire {
            public_key: Some([0x5E; 64]),
            ..hash_only_target()
        };
        assert_eq!(
            send_telemetry_target(&fd, &target).unwrap(),
            ControlOutcome::Acked
        );
    }

    #[test]
    fn a_reporterless_firmware_refuses_the_target_it_cannot_honor() {
        // Ack honesty (#236): a binary that never wired a reporter answers
        // a named refusal, not the ack the T114 used to give. The refusal
        // is distinct from busy (retry helps) and from unknown-type (the
        // envelope layer knows the type) — only different firmware helps.
        let pty = Pty::open();
        let seen = seen();
        reporterless_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let reply = probed(&fd, TYPE_TELEMETRY_TARGET, |fd| {
            send_telemetry_target(fd, &hash_only_target())
        })
        .unwrap();
        assert_eq!(reply, SessionReply::Refused(REFUSE_UNSUPPORTED));
        // took_it() false is what drives the session's non-zero exit.
        assert!(!reply.took_it());
    }

    #[test]
    fn a_pre_236_board_refuses_the_target_by_name_instead_of_timing_out() {
        // The detection path: an old board answers, and what it answers
        // says exactly what is missing.
        let pty = Pty::open();
        pre_236_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let caps = probe_capabilities(&fd).unwrap().unwrap();
        assert!(!caps.accepts(TYPE_TELEMETRY_TARGET));
        assert_eq!(
            send_telemetry_target(&fd, &hash_only_target()).unwrap(),
            ControlOutcome::Refused {
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    // -----------------------------------------------------------------
    // Transmit spacing (Codeberg #345)
    // -----------------------------------------------------------------

    #[test]
    fn a_transmit_spacing_is_acked_and_the_value_reaches_the_board() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(send_tx_spacing(&fd, 60).unwrap(), ControlOutcome::Acked);
        // The number on the wire is the number asked for, not a rounding
        // or a default: a sweep point that arrives changed is worse than
        // one that does not arrive.
        assert_eq!(tx_spacing_frame(&seen), Some(60));
    }

    #[test]
    fn zero_travels_as_a_value_rather_than_as_nothing_sent() {
        // Zero is how a sweep puts a board back on the default, so it has
        // to be a frame the board acks, not a skipped command.
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(send_tx_spacing(&fd, 0).unwrap(), ControlOutcome::Acked);
        assert_eq!(tx_spacing_frame(&seen), Some(0));
    }

    #[test]
    fn the_largest_spacing_the_wire_can_carry_is_acked_too() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            send_tx_spacing(&fd, u16::MAX).unwrap(),
            ControlOutcome::Acked
        );
        assert_eq!(tx_spacing_frame(&seen), Some(65_535));
    }

    #[test]
    fn a_board_without_the_knob_refuses_it_by_name_instead_of_timing_out() {
        let pty = Pty::open();
        pre_236_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let caps = probe_capabilities(&fd).unwrap().unwrap();
        assert!(!caps.accepts(leviculum_core::envelope::TYPE_TX_SPACING));
        assert_eq!(
            send_tx_spacing(&fd, 60).unwrap(),
            ControlOutcome::Refused {
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    // -----------------------------------------------------------------
    // Announce-now and the BLE transmit gap (Codeberg #376)
    // -----------------------------------------------------------------

    #[test]
    fn an_announce_request_is_acked_and_reaches_the_board() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(send_announce(&fd).unwrap(), ControlOutcome::Acked);
        assert!(announce_frame_seen(&seen));
    }

    #[test]
    fn a_clockless_board_refuses_the_announce_by_name_not_by_timeout() {
        // The desk's most important reading: "the board withheld for lack
        // of a clock" must arrive as a spoken refusal, not as a run-down
        // window a bench would misread as dead firmware.
        let pty = Pty::open();
        clockless_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let started = Instant::now();
        assert_eq!(
            send_announce(&fd).unwrap(),
            ControlOutcome::Refused {
                reason: REFUSE_NO_CLOCK
            }
        );
        assert!(started.elapsed() < CONTROL_TIMING.window);
    }

    #[test]
    fn a_board_without_the_announce_command_refuses_it_by_name() {
        let pty = Pty::open();
        pre_236_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let caps = probe_capabilities(&fd).unwrap().unwrap();
        assert!(!caps.accepts(leviculum_core::envelope::TYPE_ANNOUNCE));
        assert_eq!(
            send_announce(&fd).unwrap(),
            ControlOutcome::Refused {
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    #[test]
    fn a_ble_tx_gap_is_acked_and_the_value_reaches_the_board() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(send_ble_tx_gap(&fd, 20).unwrap(), ControlOutcome::Acked);
        // The number on the wire is the number asked for — a sweep point
        // that arrives changed is worse than one that does not arrive.
        assert_eq!(ble_tx_gap_frame(&seen), Some(20));
    }

    #[test]
    fn a_zero_gap_travels_as_a_value_rather_than_as_nothing_sent() {
        // Zero puts a board back on the default, so it has to be a frame
        // the board acks, not a skipped command.
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(send_ble_tx_gap(&fd, 0).unwrap(), ControlOutcome::Acked);
        assert_eq!(ble_tx_gap_frame(&seen), Some(0));
    }

    #[test]
    fn a_gap_beyond_the_board_bound_is_refused_by_value() {
        // The stub runs the firmware's own decision function, so this is
        // the board-side bound (#376 "reject the rest on the board"), not
        // a CLI parse: 5001 ms arrives, is decoded, and is refused with
        // the named value reason.
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            send_ble_tx_gap(&fd, leviculum_core::envelope::BLE_TX_GAP_MAX_MS + 1).unwrap(),
            ControlOutcome::Refused {
                reason: REFUSE_VALUE
            }
        );
        // …and the refused value never classified into an action, so a
        // board that shares this decision function never applied it.
        assert_eq!(ble_tx_gap_frame(&seen), None);
    }

    #[test]
    fn a_board_without_the_gap_knob_refuses_it_by_name() {
        let pty = Pty::open();
        pre_236_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let caps = probe_capabilities(&fd).unwrap().unwrap();
        assert!(!caps.accepts(leviculum_core::envelope::TYPE_BLE_TX_GAP));
        assert_eq!(
            send_ble_tx_gap(&fd, 20).unwrap(),
            ControlOutcome::Refused {
                reason: REFUSE_UNKNOWN_TYPE
            }
        );
    }

    // -----------------------------------------------------------------
    // Reading the radio back (#349)
    // -----------------------------------------------------------------

    /// The board's own settings come back, not the host's defaults.
    #[test]
    fn the_radio_query_returns_what_the_board_is_running() {
        let pty = Pty::open();
        envelope_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        assert_eq!(
            query_radio_config(&fd).unwrap(),
            Some(stub_running_config())
        );
    }

    /// **The control for `--set-tx-power`.** Read the board's settings,
    /// change one field, send them back: the frame that reaches the board
    /// differs from what it reported in the power and in nothing else.
    ///
    /// Asserted field by field against the board's own report rather than
    /// against a constructed expectation, so a flow that rebuilt the config
    /// from `lnflash`'s eu868 defaults — the failure this whole read-back
    /// exists to prevent, since it would reset the bandwidth while claiming
    /// to set the power — fails here.
    #[test]
    fn changing_the_power_leaves_every_other_setting_where_the_board_had_it() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let mut cfg = query_radio_config(&fd).unwrap().expect("board reports");
        cfg.tx_power_dbm = -9;
        assert_eq!(send_radio_config(&fd, &cfg).unwrap(), ControlOutcome::Acked);

        let arrived = radio_config_frame(&seen).expect("a config frame reached the board");
        let running = stub_running_config();
        assert_eq!(arrived.tx_power_dbm, -9, "the power did not change");
        assert_eq!(arrived.frequency_hz, running.frequency_hz);
        assert_eq!(arrived.bandwidth_hz, running.bandwidth_hz);
        assert_eq!(arrived.sf, running.sf);
        assert_eq!(arrived.cr, running.cr);
        assert_eq!(arrived.preamble_len, running.preamble_len);
        assert_eq!(arrived.csma_enabled, running.csma_enabled);
        assert_eq!(arrived.radio_silent, running.radio_silent);
        assert_eq!(arrived.st_alock, running.st_alock);
        assert_eq!(arrived.lt_alock, running.lt_alock);
        assert_eq!(arrived.lt_alock_present, running.lt_alock_present);
    }

    /// Firmware without the query answers nothing usable, and the caller is
    /// told so rather than handed a plausible config it can act on.
    #[test]
    fn a_board_without_the_query_yields_no_config_rather_than_a_guess() {
        let pty = Pty::open();
        pre_236_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let caps = probe_capabilities(&fd).unwrap().unwrap();
        assert!(!caps.accepts(leviculum_core::envelope::TYPE_RADIO_QUERY));
        assert_eq!(query_radio_config(&fd).unwrap(), None);
    }

    // -----------------------------------------------------------------
    // The probe-then-send shape every configure session uses
    // -----------------------------------------------------------------

    #[test]
    fn probed_sends_once_the_report_lists_the_type() {
        let pty = Pty::open();
        let seen = seen();
        envelope_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let reply = probed(&fd, TYPE_TELEMETRY_TARGET, |fd| {
            send_telemetry_target(fd, &hash_only_target())
        })
        .unwrap();
        assert_eq!(reply, SessionReply::Acked);
        assert!(reply.took_it());
        assert_eq!(telemetry_frame(&seen), Some(hash_only_target()));
    }

    #[test]
    fn probed_will_not_send_a_long_frame_to_firmware_that_never_answered_the_probe() {
        // Firmware from before the envelope drops the 5-byte query and would
        // read the 24-byte target frame as a Reticulum packet. Silence has to
        // stop the conversation, not start it.
        let pty = Pty::open();
        let seen = seen();
        old_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let reply = probed(&fd, TYPE_TELEMETRY_TARGET, |fd| {
            send_telemetry_target(fd, &hash_only_target())
        })
        .unwrap();
        assert_eq!(reply, SessionReply::ProbeSilent);
        assert!(!reply.took_it());
        assert_eq!(telemetry_frame(&seen), None, "nothing may go on the wire");
    }

    #[test]
    fn probed_will_not_send_a_type_the_report_leaves_out() {
        let pty = Pty::open();
        let seen = seen();
        pre_236_firmware_stub(&pty, seen.clone());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let reply = probed(&fd, TYPE_TELEMETRY_TARGET, |fd| {
            send_telemetry_target(fd, &hash_only_target())
        })
        .unwrap();
        assert_eq!(reply, SessionReply::NotAccepted);
        assert_eq!(telemetry_frame(&seen), None, "nothing may go on the wire");
    }

    #[test]
    fn probed_carries_a_named_refusal_through_rather_than_flattening_it() {
        // An implausible wall time is the one refusal the stub speaks, and
        // the session vocabulary has to keep the reason.
        let pty = Pty::open();
        envelope_firmware_stub(&pty, seen());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        assert_eq!(
            probed(&fd, TYPE_WALL_TIME, |fd| send_wall_time(fd, 1_000)).unwrap(),
            SessionReply::Refused(REFUSE_VALUE)
        );
        assert_eq!(
            probed(&fd, TYPE_WALL_TIME, |fd| send_wall_time(fd, 1_790_000_000)).unwrap(),
            SessionReply::Acked
        );
    }

    /// A media report names no frame, so a leftover one is indistinguish-
    /// able from an answer by its type alone. A query has nothing to
    /// correlate against either — its only defence is that bytes which
    /// were already in the port before the query went out cannot be its
    /// answer.
    #[test]
    fn a_report_left_in_the_port_is_not_read_as_the_answer_to_a_query() {
        let pty = Pty::open();
        // The board is on both carriers, and says so when asked.
        envelope_firmware_stub_with_media(&pty, seen(), media_state());
        let fd = Fd::open_serial(&pty.slave_path).unwrap();
        // A leftover from an earlier conversation: a report about a board
        // with both carriers down, sitting in the port before we ask.
        pty.inject(&encode_media_report(
            &MediaProfileWire {
                lora_enabled: false,
                ble_enabled: false,
            },
            &MediaProfileWire {
                lora_enabled: false,
                ble_enabled: false,
            },
        ));

        let read = query_media_profile(&fd)
            .unwrap()
            .expect("the board answers");
        assert_eq!(
            read.configured,
            MediaProfileWire::BOTH,
            "the leftover report was read as this query's answer"
        );
        assert_eq!(read.running, MediaProfileWire::BOTH);
    }

    /// The set direction, where being wrong is worse: `set_media_on`
    /// queries and then writes on the same fd, so a board that is still
    /// repeating an earlier report has one in the line exactly when the
    /// set is waiting for its own. Reading it reports the profile the
    /// board was on BEFORE the write.
    #[test]
    fn a_report_about_another_profile_is_not_read_as_this_sets_answer() {
        let pty = Pty::open();
        let stale = MediaProfileWire::BOTH;
        let media = media_state();
        late_report_firmware_stub(&pty, seen(), media, stale);
        let fd = Fd::open_serial(&pty.slave_path).unwrap();

        let lora_only = MediaProfileWire {
            lora_enabled: true,
            ble_enabled: false,
        };
        // The probe inside `send_configured` is what the stub trails its
        // stale report behind, so the leftover is in the line while the
        // set frame waits for its answer.
        let state = crate::media::send_configured(&fd, lora_only)
            .unwrap()
            .expect("the board answers the set");
        assert_eq!(
            state.configured, lora_only,
            "the stale report was read as the answer to the set"
        );
        assert_eq!(state.running, lora_only);
    }

    /// The classifier itself, without timing: a report that does not
    /// state the profile just written cannot be this conversation's, and
    /// a refusal of the frame that was sent still is.
    #[test]
    fn the_set_classifier_only_claims_a_report_of_the_profile_it_sent() {
        let lora_only = MediaProfileWire {
            lora_enabled: true,
            ble_enabled: false,
        };
        let classify = media_answer(TYPE_MEDIA_PROFILE, Some(lora_only));
        assert!(
            classify(&encode_media_report(
                &MediaProfileWire::BOTH,
                &MediaProfileWire::BOTH
            ))
            .is_none(),
            "a report about a profile we did not send is not our answer"
        );
        assert_eq!(
            classify(&encode_media_report(&lora_only, &lora_only)),
            Some(Ok(MediaState {
                running: lora_only,
                configured: lora_only,
            }))
        );
        assert_eq!(
            classify(&encode_refusal(TYPE_MEDIA_PROFILE, REFUSE_BUSY)),
            Some(Err(REFUSE_BUSY))
        );
    }
}
