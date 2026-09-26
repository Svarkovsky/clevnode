//! The media profile: which carriers this node meshes over.
//!
//! # Why the node declares its carriers
//!
//! An LNode meshes over LoRa **and** BLE at once by default, so a packet
//! that arrived over the other medium masks a loss on the medium under
//! test: every single-medium number such a node produces is falsifiable.
//! The cure is a profile that is *declared*, *applied* and *proven* — this
//! module is the applied and the proven half on the board:
//!
//! * **applied** — [`lora_active`] / [`ble_active`] are read by the two
//!   interfaces and by the binaries' RX arms, and by the boot path that
//!   decides whether to spawn a carrier's tasks at all.
//! * **proven** — [`log_banner`] emits one `[MEDIA]` line per boot. That
//!   line is the interface a measurement asserts on; its shape is frozen
//!   in `docs/src/structured-event-logs.md` and must not drift.
//!
//! The record itself lives on the telemetry flash page (layout in
//! [`crate::telemetry`]); the bytes are
//! [`leviculum_core::media_profile_store`].
//!
//! # Running versus configured
//!
//! Two states, because they honestly differ:
//!
//! * **configured** — what a reboot would come up with. Set from flash at
//!   boot (or from the both-on default), and by a host over
//!   [`TYPE_MEDIA_PROFILE`](leviculum_core::envelope::TYPE_MEDIA_PROFILE).
//! * **running** — what this boot is actually carrying traffic on:
//!   `configured AND booted_with`.
//!
//! Switching a carrier off always takes effect at once — and for BLE,
//! off means off: every live link is disconnected (a phone sees the
//! board go, the way it would if the board left range), advertising and
//! scanning stop, and each dropped link's `PeerEvent::Lost` runs the
//! same path cull that range loss runs (see §Teardown semantics below).
//! Switching one back on takes effect at once too **as long as it came
//! up at boot** — its tasks are still there, gated, and resume
//! advertising and scanning. A carrier that did *not* come up at boot
//! has no task to un-gate: an embassy task cannot be spawned from
//! nothing after the fact, so it cannot start before the next reset.
//!
//! That case is answered with a report saying "configured, still not
//! running", never with an ack that would claim otherwise. See
//! [`leviculum_core::envelope::TYPE_MEDIA_REPORT`].
//!
//! Because that answer is terminal — `lnflash` prints "Reset the board"
//! for it — it must not be reachable while the boot is merely still on
//! its way to the carriers. USB comes up before them by design, so the
//! serial task answers frames during a window in which nothing has been
//! spawned yet; [`load_at_boot`] seeds the boot state with the profile
//! so that window answers "running what you configured", and
//! [`note_boot_state`] narrows it to what really started. Rationale and
//! host tests in [`leviculum_media_state`].
//!
//! # Teardown semantics of a runtime OFF
//!
//! Switching a medium off at runtime stops it carrying Reticulum traffic
//! in **both** directions and does so immediately: the interface drops
//! what the core hands it, and the binary's RX arm drops what the medium
//! hands up, so nothing crosses in either direction from the moment the
//! frame is answered.
//!
//! For **BLE** the runtime off also takes the carrier off the air:
//! [`apply`] wakes the BLE protocol tasks (`crate::ble`), which
//! disconnect every live link — central and peripheral role — and drop
//! their advertise and scan futures. Each disconnect unwinds through
//! the same per-link teardown a peer walking out of range takes, so the
//! main loop receives one `PeerEvent::Lost` per peer and culls its
//! paths identically; on off→on the tasks resume advertising and
//! scanning, and the reconnect produces the ordinary `PeerEvent::Up`
//! and path pull. This is what makes the #365 BLE-loss fallback desk-
//! testable: before it, a runtime off was a mute — packets to a still-
//! connected phone died silently in the interface and nothing
//! re-resolved over LoRa until the phone really left range.
//!
//! **LoRa** keeps the weaker semantics: its task keeps listening (RX is
//! dropped upward, nothing is transmitted), so LoRa radio silence still
//! needs the boot path — the rig acceptance for it is flash-set-reboot,
//! and the `[MEDIA]` banner then proves it.

use leviculum_core::envelope::MediaProfileWire;
use leviculum_media_state::{Carriers, MediaState};

/// The whole state this module answers from: the declaration gate, the
/// configured profile and what the boot brought up. In
/// [`leviculum_media_state`] rather than here because its bugs are
/// ordering bugs — the boot window between [`load_at_boot`] and
/// [`note_boot_state`] most of all — and the firmware crate cross-
/// compiles, so nothing in it can be driven by a host test.
///
/// The gate ([`media_wired`]) carries the same ack-honesty rule as the
/// telemetry reporter's, and it bites harder here: an ack is what a
/// measurement run reads as "this node is now single-medium", so a
/// binary that acked without gating would make every number that run
/// produced a lie.
static STATE: MediaState = MediaState::new();

const fn carriers(profile: MediaProfileWire) -> Carriers {
    Carriers {
        lora: profile.lora_enabled,
        ble: profile.ble_enabled,
    }
}

const fn wire(carriers: Carriers) -> MediaProfileWire {
    MediaProfileWire {
        lora_enabled: carriers.lora,
        ble_enabled: carriers.ble,
    }
}

/// Where the configured profile came from, for the boot banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// No usable record on the page: blank, corrupt, or naming a carrier
    /// this firmware does not know. Both carriers on — today's behaviour,
    /// which is what the absence of a record has to mean.
    Default,
    /// A valid record, written by a host.
    Flash,
}

impl Source {
    /// The `src=` value of the `[MEDIA]` banner.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Flash => "flash",
        }
    }
}

/// Read the persisted profile, declare this binary's media gate, and
/// return what the boot should honour.
///
/// Call **before** `usb::init` and before either carrier is brought up:
/// before USB so a host frame can never be answered against the default
/// while the record is still unread, and before the carriers because the
/// answer decides which of them start at all. The read is an ordinary
/// memory-mapped flash read and is safe at any point in boot, including
/// before `Softdevice::enable`.
pub fn load_at_boot(page: u32) -> (MediaProfileWire, Source) {
    let (profile, source) = match crate::telemetry::load_media_profile(page) {
        Some(profile) => (profile, Source::Flash),
        None => (MediaProfileWire::BOTH, Source::Default),
    };
    // Declaring seeds the boot state with the profile as well, because
    // USB comes up in the next statement and the carriers only after
    // several awaited SPI transactions: a frame answered in that window
    // would otherwise read `configured=on running=off`, which the frame's
    // own contract defines as "did not come up, cannot start before the
    // next reset". Observed on the rig, run 4 of the BLE acceptance
    // (#255). `note_boot_state` narrows the seed to what really started.
    STATE.declare(carriers(profile));
    (profile, source)
}

/// Record what this boot actually brought up. Call once, after both
/// carriers' spawn decisions have been made, and pass what was really
/// spawned rather than what was asked for — this is the value that makes
/// "a carrier that did not come up cannot be started" a fact the board
/// states instead of a hope.
pub fn note_boot_state(lora_started: bool, ble_started: bool) {
    STATE.note_boot_state(Carriers {
        lora: lora_started,
        ble: ble_started,
    });
}

/// Whether [`load_at_boot`] was called — the capability the serial task's
/// media answers are gated on.
pub fn media_wired() -> bool {
    STATE.wired()
}

/// Whether LoRa is carrying Reticulum traffic right now.
pub fn lora_active() -> bool {
    STATE.lora_active()
}

/// Whether BLE is carrying Reticulum traffic right now.
pub fn ble_active() -> bool {
    STATE.ble_active()
}

/// What this boot is carrying traffic on (see the module docs).
pub fn running() -> MediaProfileWire {
    wire(STATE.running())
}

/// What a reboot would come up with (see the module docs).
pub fn configured() -> MediaProfileWire {
    wire(STATE.configured())
}

/// Apply a profile a host sent: take effect where that is possible, and
/// persist it either way.
///
/// Runs on the serial task rather than being handed to the main loop: the
/// whole apply is two atomic stores and a non-blocking save request, and
/// nothing here needs the node. That also means the answer the serial
/// task writes is the state that is already in force, not a prediction of
/// one — [`running`] and [`configured`] read back correct the instant
/// this returns.
///
/// The returned [`crate::telemetry::PendingSave`] is the other half of
/// the answer, and the caller must not write the report before waiting on
/// it (`crate::telemetry::confirm`, Codeberg #358). The report says what a
/// reboot would come up with; sent while the page write was still owed,
/// it was a claim about a reboot that the reboot disproved.
pub fn apply(profile: MediaProfileWire) -> crate::telemetry::PendingSave {
    STATE.set_configured(carriers(profile));
    // After the stores, never before: the woken BLE tasks re-read
    // `ble_active`, and waking them against the old state would let a
    // just-switched-off carrier advertise on. On an on→off edge the
    // tasks disconnect every live link and stop advertising and
    // scanning; each dropped link reports `PeerEvent::Lost` through the
    // same teardown as range loss, so the core's cull is identical (see
    // the module docs and `crate::ble`).
    crate::ble::note_media_changed();
    crate::telemetry::request_save_media_profile(profile)
}

/// **The proof line.** One per boot, and re-emitted with the firmware
/// build banner so a capture attached after the boot window still reads
/// the running profile off the board rather than off an operator's
/// memory.
///
/// ```text
/// [MEDIA] lora=on ble=off src=flash t=1183
/// ```
///
/// `lora=`/`ble=` are what this boot is **running** (a medium configured
/// on but not started reads `off` here, which is the honest answer), and
/// `src=` says whether the configuration came off the page or from the
/// both-on default. The shape is an interface: periculum asserts on it,
/// and it is frozen in `docs/src/structured-event-logs.md`.
pub fn log_banner(source: Source) {
    let running = running();
    crate::log::log_fmt_critical(
        "[MEDIA] ",
        format_args!(
            "lora={} ble={} src={}",
            on_off(running.lora_enabled),
            on_off(running.ble_enabled),
            source.as_str()
        ),
    );
}

/// The banner's boolean spelling. `on`/`off` rather than `1`/`0`: the
/// line is read by operators at least as often as by periculum.
const fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

/// Report a run of packets a down carrier threw away.
///
/// Called by the interfaces on the first drop of a run and at every
/// decade after it, never per packet: the reasoning, and the host tests,
/// are in [`leviculum_media_state::DropRun`]. `packets=` is the run's
/// count at the moment of the line, so the last such line is a lower
/// bound on the run within a factor of ten, and
/// [`log_tx_resumed`] states the exact total when the carrier comes back.
///
/// ```text
/// [MEDIA] MEDIA_TX_DROP iface=ble packets=10 bytes=450 reason=carrier-off
/// ```
pub fn log_tx_drop(iface: &str, run: leviculum_media_state::Swallowed) {
    crate::log::log_fmt(
        "[MEDIA] ",
        format_args!(
            "MEDIA_TX_DROP iface={} packets={} bytes={} reason=carrier-off",
            iface, run.packets, run.bytes
        ),
    );
}

/// Close a drop run: the carrier took a packet again, and this is the
/// only line carrying the run's untruncated totals.
///
/// ```text
/// [MEDIA] MEDIA_TX_RESUMED iface=ble packets=37 bytes=1665
/// ```
pub fn log_tx_resumed(iface: &str, run: leviculum_media_state::Swallowed) {
    crate::log::log_fmt(
        "[MEDIA] ",
        format_args!(
            "MEDIA_TX_RESUMED iface={} packets={} bytes={}",
            iface, run.packets, run.bytes
        ),
    );
}

/// Say that a carrier was held down at boot because the profile said so.
///
/// Emitted next to the carrier's own init log so a reader who greps for
/// `[LORA]` or `[BLE ]` and finds nothing has the reason on the line
/// where the bring-up would have been, not only in the `[MEDIA]` banner.
pub fn log_carrier_held_down(carrier: &str) {
    crate::log::log_fmt_critical(
        "[MEDIA] ",
        format_args!("carrier={} state=down reason=profile", carrier),
    );
}
