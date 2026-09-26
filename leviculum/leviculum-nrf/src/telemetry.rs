//! The LNode telemetry reporter (Codeberg #236).
//!
//! Three things meet here, and none of them is a decision:
//!
//! * **The policy** — `leviculum_telemetry_policy` owns every "report
//!   now?" question and every target-lifecycle transition. It is a
//!   separate, host-tested crate for the same reason the GNSS presence
//!   machine is: a state machine that needs a radio to be exercised is a
//!   state machine that is never exercised.
//! * **The codec** — `leviculum_lxmf::telemetry` owns the Telemeter bytes
//!   and `build_report` owns the message shape (empty content and title,
//!   opportunistic delivery). Both are proven against the #237 fixtures on
//!   the host.
//! * **The storage** — [`leviculum_core::telemetry_target_store`] owns the
//!   flash record.
//!
//! What is left here is wiring: read the sensors this board has, ask the
//! policy, build the message, hand the packet to the node, persist what
//! the host set, and say out loud what state the target is in. Nothing
//! in this module is RAK-specific; the per-board part is which peripherals
//! exist to read, and that lives in the binary.
//!
//! # Airtime
//!
//! There is no airtime figure in this module and there must never be one.
//! Cadence is policy; when the radio may transmit is the interface's
//! business (`docs/src/concepts/interface-isolation.md`,
//! `docs/src/concepts/regulatory-airtime.md`).

extern crate alloc;

use alloc::vec::Vec;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;

use leviculum_core::envelope::{
    FixedPositionWire, MediaProfileWire, Persist, TelemetryTargetWire, TELEMETRY_PROFILE_OFF,
};
use leviculum_core::fixed_position_store::{decode_fixed_position, encode_fixed_position};
use leviculum_core::identity::Identity;
use leviculum_core::media_profile_store::{decode_media_profile, encode_media_profile};
use leviculum_core::node::{NodeCore, NodeEvent};
use leviculum_core::node_name::NodeName;
use leviculum_core::node_name_store::{decode_node_name, encode_node_name};
use leviculum_core::telemetry_target_store::{decode_telemetry_target, encode_telemetry_target};
use leviculum_core::traits::{Clock, Storage};
use leviculum_core::transport::{Action, DispatchResult};
use leviculum_core::DestinationHash;
use leviculum_lxmf::msgpack::Number;
use leviculum_lxmf::telemetry::{
    build_report, celsius_from_quarter_degrees, screen_telemetry_request, Battery, Location,
    Telemetry, TelemetryRequestVerdict,
};
use leviculum_persist_ack::{PersistGate, Persisted, SaveTicket};
use leviculum_telemetry_policy::{
    choose_position, command_from_wire, EmissionRoute, FailureVerdict, Fix, PacketHash,
    PositionSource, Profile, ProofTracker, ReportReason, RequestOutcome, SendPolicy, TargetCommand,
    TargetState, FIXED_POSITION_HDOP_E2,
};

/// Re-exported so the binaries name the outcome of
/// [`Reporter::apply_target`] without also depending on the policy crate
/// directly: the wiring layer is the seam, and the seam owns its
/// vocabulary.
pub use leviculum_telemetry_policy::TargetOutcome;
use rand_core::CryptoRngCore;

/// The profile ids are allocated twice — once on the wire
/// (`leviculum_core::envelope`) and once in the policy crate, which
/// cannot depend on core — so this is the one place that sees both.
/// A drift between them would turn a "clear" frame into a "set" frame
/// with a zero destination, which is exactly the silent failure the
/// explicit clear encoding exists to avoid.
const _: () = {
    assert!(TELEMETRY_PROFILE_OFF == leviculum_telemetry_policy::PROFILE_ID_OFF);
    assert!(
        leviculum_core::envelope::TELEMETRY_PROFILE_TRACKER
            == leviculum_telemetry_policy::PROFILE_ID_TRACKER
    );
    assert!(
        leviculum_core::envelope::TELEMETRY_PROFILE_STATION
            == leviculum_telemetry_policy::PROFILE_ID_STATION
    );
};

/// Whether this binary wires a [`Reporter`] into its main loop.
///
/// The serial control task gates the telemetry-target ack on this
/// declaration ([`leviculum_core::envelope::telemetry_target_answer`]):
/// a binary that never constructs a reporter must answer a named refusal,
/// not an ack for a target nothing will honor. Set once via
/// [`declare_reporter`] before `usb::init`, so no frame can be answered
/// before the declaration exists.
static REPORTER_WIRED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Declare that this binary constructs a [`Reporter`] and drains
/// [`inbound_target_receiver`]. Call before `usb::init`.
pub fn declare_reporter() {
    REPORTER_WIRED.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// Whether [`declare_reporter`] was called — the capability the serial
/// task's telemetry-target answer is gated on.
pub fn reporter_wired() -> bool {
    REPORTER_WIRED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Which position sources this node has configured, published for the
/// serial task's [`leviculum_core::envelope::TYPE_POSITION_SOURCE_QUERY`]
/// answer.
///
/// A static rather than a question put to the main loop, for the same
/// reason [`crate::media`] keeps its two profiles in atomics: the serial
/// task must answer a read-only query without waiting on a loop that may
/// be mid-dispatch, and both writers of this value are one atomic store.
static POSITION_SOURCES: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// A user-set fixed position is stored ([`Reporter::apply_fixed_position`]).
pub const POSITION_SOURCE_FIXED: u8 = 0x01;
/// A GNSS receiver is built into this firmware and not switched off
/// ([`declare_gnss_source`]).
pub const POSITION_SOURCE_GNSS: u8 = 0x02;

/// Declare which position sources this node boots with. Call beside
/// [`declare_reporter`], before `usb::init`.
///
/// Both halves are known this early and both must be: the receiver is a
/// property of the binary, and the stored pin is a memory-mapped flash read
/// that is legal before `Softdevice::enable` (the argument [`load`] makes).
/// Declaring them here rather than when the [`Reporter`] is built closes the
/// window in which a host query would be answered "no position source" by a
/// board that has one — the same reason the media profile is read before
/// USB comes up.
///
/// `gnss_available` is passed per binary rather than read from a `cfg!`
/// here: the workspace clippy run builds both board binaries under one
/// feature set, so a `cfg!(feature = "gnss")` inside this module would have
/// the T114 claim a receiver it has no task for. The binary knows which
/// peripherals it actually spawned.
pub fn declare_position_sources(gnss_available: bool, page: u32) {
    let mut flags = 0;
    if gnss_available {
        flags |= POSITION_SOURCE_GNSS;
    }
    if load_fixed_position(page).is_some() {
        flags |= POSITION_SOURCE_FIXED;
    }
    POSITION_SOURCES.store(flags, core::sync::atomic::Ordering::Relaxed);
}

/// The position sources this node has, as the wire flags.
pub fn position_source_flags() -> u8 {
    POSITION_SOURCES.load(core::sync::atomic::Ordering::Relaxed)
}

/// Whether any position source is configured — the second clause of the
/// send condition (`docs/src/concepts/telemetry.md`).
pub fn has_position_source() -> bool {
    position_source_flags() != 0
}

/// Record whether a fixed position is stored. The GNSS bit is untouched:
/// clearing the pin on a GNSS board still leaves it a reporting node.
fn note_fixed_position(present: bool) {
    use core::sync::atomic::Ordering::Relaxed;
    if present {
        POSITION_SOURCES.fetch_or(POSITION_SOURCE_FIXED, Relaxed);
    } else {
        POSITION_SOURCES.fetch_and(!POSITION_SOURCE_FIXED, Relaxed);
    }
}

/// Targets arriving from the host over the #238 control envelope. Depth 1
/// and `try_send`: the serial task must never block, and a superseded
/// target is worthless — the newest one is the one that must take effect.
static INBOUND_TARGET: Channel<CriticalSectionRawMutex, TelemetryTargetWire, 1> = Channel::new();

/// Hand a target from the serial task to the main loop. Returns whether
/// it was taken; `false` is a main loop that has not drained the previous
/// one yet, which the host's retry covers.
pub fn deliver_target(target: TelemetryTargetWire) -> bool {
    if INBOUND_TARGET.try_send(target).is_err() {
        let _ = INBOUND_TARGET.try_receive();
        INBOUND_TARGET.try_send(target).is_ok()
    } else {
        true
    }
}

/// The main loop's end of [`deliver_target`].
pub fn inbound_target_receiver(
) -> embassy_sync::channel::Receiver<'static, CriticalSectionRawMutex, TelemetryTargetWire, 1> {
    INBOUND_TARGET.receiver()
}

/// Fixed positions arriving from the host over the control envelope
/// (`TYPE_FIXED_POSITION`; `None` is the explicit clear). Same depth-1
/// replace discipline as [`INBOUND_TARGET`], for the same reason.
static INBOUND_FIXED_POSITION: Channel<CriticalSectionRawMutex, Option<FixedPositionWire>, 1> =
    Channel::new();

/// Hand a fixed position (or its clear) from the serial task to the main
/// loop. Returns whether it was taken; `false` is covered by the host's
/// retry, exactly like [`deliver_target`].
pub fn deliver_fixed_position(position: Option<FixedPositionWire>) -> bool {
    if INBOUND_FIXED_POSITION.try_send(position).is_err() {
        let _ = INBOUND_FIXED_POSITION.try_receive();
        INBOUND_FIXED_POSITION.try_send(position).is_ok()
    } else {
        true
    }
}

/// The main loop's end of [`deliver_fixed_position`].
pub fn inbound_fixed_position_receiver(
) -> embassy_sync::channel::Receiver<'static, CriticalSectionRawMutex, Option<FixedPositionWire>, 1>
{
    INBOUND_FIXED_POSITION.receiver()
}

// ---------------------------------------------------------------------------
// Persistence — same two-halves shape as `radio_store`
// ---------------------------------------------------------------------------

/// The telemetry flash page layout (`BoardConfig::telemetry_flash_page`).
///
/// One 4 KiB page carries four independent records, because the page is
/// the last one the bootloader's `USER_FLASH_END` protects on both boards
/// — 0xEB000/0xEC000 hold radio config and identity, and 0xED000 upward is
/// Heltec's license/version band on the T114 (memory.x) — so a second
/// page was never on offer:
///
/// ```text
/// +0x000  telemetry target record  ("LTTG", telemetry_target_store)   24 B
/// +0x100  fixed position record    ("LFPO", fixed_position_store)      24 B
/// +0x200  media profile record     ("LMED", media_profile_store)        8 B
/// +0x300  node name record         ("LNAM", node_name_store)          44 B
/// ```
///
/// The target keeps offset 0, where every fielded board already has it, so
/// this layout is what those boards are running the moment they first
/// persist one of the later records. Erase granularity is the whole page,
/// so the store task rewrites all four records on every save; each
/// record's own magic + checksum keeps a torn write from becoming a
/// garbage target, a garbage pin, a board on the wrong carriers or a
/// board announcing a name nobody set.
///
/// The offsets are 0x100 apart and the longest record is 44 bytes, so no
/// two records overlap and the page (4096 B) has room for twelve more.
/// The compile-time assertion below is what keeps that true when a record
/// grows.
const TARGET_OFFSET: u32 = 0x000;
/// See [`TARGET_OFFSET`].
const FIXED_POSITION_OFFSET: u32 = 0x100;
/// See [`TARGET_OFFSET`].
const MEDIA_OFFSET: u32 = 0x200;
/// See [`TARGET_OFFSET`].
const NAME_OFFSET: u32 = 0x300;

/// The page layout's collision check, run by the compiler rather than by
/// a reviewer reading three offsets: each record must end before the next
/// one starts, and the last must end inside the 4 KiB page.
const _: () = {
    const PAGE_SIZE: u32 = 4096;
    assert!(
        TARGET_OFFSET + leviculum_core::telemetry_target_store::ENCODED_SIZE_ALIGNED as u32
            <= FIXED_POSITION_OFFSET
    );
    assert!(
        FIXED_POSITION_OFFSET + leviculum_core::fixed_position_store::ENCODED_SIZE_ALIGNED as u32
            <= MEDIA_OFFSET
    );
    assert!(
        MEDIA_OFFSET + leviculum_core::media_profile_store::ENCODED_SIZE_ALIGNED as u32
            <= NAME_OFFSET
    );
    assert!(
        NAME_OFFSET + leviculum_core::node_name_store::ENCODED_SIZE_ALIGNED as u32 <= PAGE_SIZE
    );
};

/// Pending save requests. Depth 1 for the same reason as the radio store:
/// the newest value of each record is the one that must end up on the
/// page. Four channels rather than one queue so a target save, a fixed
/// position save, a media save and a name save can never displace each
/// other.
///
/// Each item carries the [`SaveTicket`] the requester is waiting on, so
/// the store task can hand back the outcome of *that* write rather than
/// of whichever write it happened to finish next (Codeberg #358).
static PENDING_SAVE: Channel<CriticalSectionRawMutex, (SaveTicket, TelemetryTargetWire), 1> =
    Channel::new();
static PENDING_SAVE_FIXED: Channel<
    CriticalSectionRawMutex,
    (SaveTicket, Option<FixedPositionWire>),
    1,
> = Channel::new();
static PENDING_SAVE_MEDIA: Channel<CriticalSectionRawMutex, (SaveTicket, MediaProfileWire), 1> =
    Channel::new();
static PENDING_SAVE_NAME: Channel<CriticalSectionRawMutex, (SaveTicket, Option<NodeName>), 1> =
    Channel::new();

/// One record's persist bookkeeping: the [`PersistGate`] that says whether
/// a given save has reached the page, plus the wake-up that saves the
/// waiter a polling loop.
///
/// The gate is a separate, host-tested crate because the ordering it
/// encodes is the whole of #358 and the firmware crate runs no host
/// tests; what stays here is the part that needs Embassy — the wake-up
/// and the bound on how long a client is made to wait.
struct PersistSlot {
    gate: PersistGate,
    /// Pulsed after every [`PersistGate::finish`]. One waiter at a time
    /// by construction: the serial task is the only caller of
    /// [`confirm`], and it answers one control frame at a time.
    wake: Signal<CriticalSectionRawMutex, ()>,
}

impl PersistSlot {
    const fn new() -> Self {
        Self {
            gate: PersistGate::new(),
            wake: Signal::new(),
        }
    }

    fn issue(&'static self) -> PendingSave {
        PendingSave {
            slot: self,
            ticket: self.gate.issue(),
        }
    }

    fn finish(&self, ticket: SaveTicket, outcome: Persisted) {
        self.gate.finish(ticket, outcome);
        self.wake.signal(());
    }
}

static TARGET_PERSIST: PersistSlot = PersistSlot::new();
static FIXED_PERSIST: PersistSlot = PersistSlot::new();
static MEDIA_PERSIST: PersistSlot = PersistSlot::new();
static NAME_PERSIST: PersistSlot = PersistSlot::new();

/// A save the caller may wait for with [`confirm`], returned by every
/// `request_save*`.
///
/// Carries its own record's slot so a caller cannot wait on the wrong
/// gate — the store task rewrites all four records on every save, but
/// only the one a request names carries the requester's value.
#[must_use = "a control-envelope ack on the persist path must wait for this (Codeberg #358)"]
#[derive(Clone, Copy)]
pub struct PendingSave {
    slot: &'static PersistSlot,
    ticket: SaveTicket,
}

/// How long a caller waits for the store task before answering the frame
/// with the truth instead of a promise.
///
/// The observed cost of one page write is ~200 ms (#358), and the store
/// task's own retry budget is [`SAVE_RETRIES`] × [`SAVE_RETRY_MS`] plus
/// three erase-and-write rounds — call it 1.2 s if the SoftDevice keeps
/// refusing flash access while the radio is busy. 2.5 s covers that twice
/// over and still lands inside the 3.5 s window lnflash gives one control
/// conversation (`lnflash::radio::ACK_WITHIN`), so a board that has to
/// report a failure gets the report out before the host stops listening.
const PERSIST_CONFIRM_WITHIN: embassy_time::Duration = embassy_time::Duration::from_millis(2_500);

/// Wait until the store task has settled `save`, and say what a reset
/// would find (Codeberg #358).
///
/// This is what turns the control-envelope ack from "applied" into "a
/// reset cannot lose this". The wait is bounded: a wedged store task
/// makes the frame answer [`Persist::Lost`] — the truth — rather than
/// holding the transport port open forever.
///
/// It does block the calling task for the duration, which for the serial
/// task means it stops servicing the transport CDC — typically ~200 ms,
/// [`PERSIST_CONFIRM_WITHIN`] at worst. Stated rather than hidden, with
/// the reasons it is the right trade: the frame being answered is an
/// explicit reconfiguration and the host that sent it is doing nothing
/// but waiting for the answer; the same task already awaits the main loop
/// on the incoming path; and the main loop pushes outbound serial frames
/// with `try_send` (`crate::interface`), so the stall can cost a dropped
/// frame on a full depth-8 channel but can never stall the node.
pub async fn confirm(save: PendingSave) -> Persist {
    let deadline = embassy_time::Instant::now() + PERSIST_CONFIRM_WITHIN;
    loop {
        if let Some(outcome) = save.slot.gate.poll(save.ticket) {
            return match outcome {
                Persisted::Durable => Persist::Durable,
                Persisted::Lost => Persist::Lost,
            };
        }
        if embassy_time::with_deadline(deadline, save.slot.wake.wait())
            .await
            .is_err()
        {
            // One last look: the store task may have finished between the
            // poll above and the deadline.
            return match save.slot.gate.poll(save.ticket) {
                Some(Persisted::Durable) => Persist::Durable,
                Some(Persisted::Lost) => Persist::Lost,
                None => {
                    crate::log::log_fmt(
                        "[TELEMETRY] ",
                        format_args!(
                            "persist unconfirmed after {} ms",
                            PERSIST_CONFIRM_WITHIN.as_millis()
                        ),
                    );
                    Persist::Lost
                }
            };
        }
    }
}

/// Read the persisted telemetry target, or `None` if its record is blank,
/// corrupt, or written by a different format version — all of which mean
/// "no target", which is telemetry off, which is the default.
///
/// Internal flash is memory-mapped on the nRF52840, so this is an ordinary
/// read and is safe at any point in boot, including before
/// `Softdevice::enable`.
pub fn load(page: u32) -> Option<TelemetryTargetWire> {
    decode_telemetry_target(&read_target_record(page))
}

/// Read the persisted fixed position, or `None` if its record is blank,
/// corrupt, or an explicit clear — all of which mean sensor reporting,
/// the default. Same read-safety argument as [`load`].
pub fn load_fixed_position(page: u32) -> Option<FixedPositionWire> {
    decode_fixed_position(&read_fixed_record(page))
}

/// Read the persisted media profile, or `None` if its record is blank,
/// corrupt, or names a carrier this firmware does not know. The caller's
/// answer to `None` is [`MediaProfileWire::BOTH`] — see
/// [`crate::media`], which owns that decision and the boot banner that
/// states which of the two it took. Same read-safety argument as
/// [`load`], and it matters more here: the profile is read *before*
/// `Softdevice::enable`, because it decides whether the BLE protocol
/// tasks are spawned at all.
pub fn load_media_profile(page: u32) -> Option<MediaProfileWire> {
    decode_media_profile(&read_media_record(page))
}

/// Read the persisted node name, or `None` if its record is blank,
/// corrupt, an explicit clear, or carries a name this firmware cannot
/// display. Every one of those means "no operator-set name", which is the
/// derived `LNode-<hex8>` / `LN-<hex8>` pair — see [`crate::name`], which
/// owns that decision. Same read-safety argument as [`load`], and it
/// matters here for the same reason it does for the media profile: the
/// name is read *before* `Softdevice::enable`, because it decides what
/// the BLE advertisement is built with.
pub fn load_node_name(page: u32) -> Option<NodeName> {
    decode_node_name(&read_name_record(page))
}

fn read_target_record(
    page: u32,
) -> [u8; leviculum_core::telemetry_target_store::ENCODED_SIZE_ALIGNED] {
    read_record(page + TARGET_OFFSET)
}

fn read_fixed_record(
    page: u32,
) -> [u8; leviculum_core::fixed_position_store::ENCODED_SIZE_ALIGNED] {
    read_record(page + FIXED_POSITION_OFFSET)
}

fn read_media_record(page: u32) -> [u8; leviculum_core::media_profile_store::ENCODED_SIZE_ALIGNED] {
    read_record(page + MEDIA_OFFSET)
}

fn read_name_record(page: u32) -> [u8; leviculum_core::node_name_store::ENCODED_SIZE_ALIGNED] {
    read_record(page + NAME_OFFSET)
}

fn read_record<const N: usize>(addr: u32) -> [u8; N] {
    let mut buf = [0u8; N];
    // SAFETY: `addr` is inside a flash page supplied by the board config,
    // outside the linker's FLASH region but inside the 1 MiB flash map
    // (memory.x). Flash is readable as normal memory on this part.
    let stored = unsafe { core::slice::from_raw_parts(addr as *const u8, N) };
    buf.copy_from_slice(stored);
    buf
}

/// Ask the store task to persist `target`. Never blocks and never writes
/// flash on the caller's stack; the returned [`PendingSave`] is how the
/// caller finds out when the record is actually on the page ([`confirm`]).
///
/// The displace-then-send pair cannot both fail: the executor is
/// cooperative and single-core, so nothing runs between the `try_receive`
/// and the `try_send` that could refill a depth-1 channel. If it somehow
/// did, the ticket would go unanswered and [`confirm`] would time out and
/// report [`Persist::Lost`] — slow, but still the truth.
pub fn request_save(target: &TelemetryTargetWire) -> PendingSave {
    let save = TARGET_PERSIST.issue();
    if PENDING_SAVE.try_send((save.ticket, *target)).is_err() {
        let _ = PENDING_SAVE.try_receive();
        let _ = PENDING_SAVE.try_send((save.ticket, *target));
    }
    save
}

/// Ask the store task to persist the fixed position (`None` persists the
/// explicit clear). Never blocks, like [`request_save`].
pub fn request_save_fixed_position(position: Option<FixedPositionWire>) -> PendingSave {
    let save = FIXED_PERSIST.issue();
    if PENDING_SAVE_FIXED
        .try_send((save.ticket, position))
        .is_err()
    {
        let _ = PENDING_SAVE_FIXED.try_receive();
        let _ = PENDING_SAVE_FIXED.try_send((save.ticket, position));
    }
    save
}

/// Ask the store task to persist the media profile. Never blocks, like
/// [`request_save`].
pub fn request_save_media_profile(profile: MediaProfileWire) -> PendingSave {
    let save = MEDIA_PERSIST.issue();
    if PENDING_SAVE_MEDIA.try_send((save.ticket, profile)).is_err() {
        let _ = PENDING_SAVE_MEDIA.try_receive();
        let _ = PENDING_SAVE_MEDIA.try_send((save.ticket, profile));
    }
    save
}

/// Ask the store task to persist the node name (`None` persists the
/// explicit clear, back to the derived default). Never blocks, like
/// [`request_save`].
pub fn request_save_node_name(name: Option<NodeName>) -> PendingSave {
    let save = NAME_PERSIST.issue();
    if PENDING_SAVE_NAME.try_send((save.ticket, name)).is_err() {
        let _ = PENDING_SAVE_NAME.try_receive();
        let _ = PENDING_SAVE_NAME.try_send((save.ticket, name));
    }
    save
}

/// 4-byte-aligned record buffer. `sd_flash_write` writes whole 32-bit
/// words and rejects an unaligned source pointer.
#[repr(align(4))]
struct Aligned<const N: usize>([u8; N]);

/// How often a failed flash operation is retried; the SoftDevice refuses
/// flash access while the radio is busy.
const SAVE_RETRIES: u8 = 3;
const SAVE_RETRY_MS: u64 = 250;

#[cfg(feature = "softdevice")]
#[embassy_executor::task]
pub async fn store_task(flash: &'static crate::flash::SharedFlash, page: u32) {
    use embassy_futures::select::{select4, Either4};
    use embedded_storage_async::nor_flash::NorFlash;

    loop {
        let request = select4(
            PENDING_SAVE.receive(),
            PENDING_SAVE_FIXED.receive(),
            PENDING_SAVE_MEDIA.receive(),
            PENDING_SAVE_NAME.receive(),
        )
        .await;
        // Whichever record the request names, the others are read back off
        // the page and rewritten with it: the erase is page-wide, so a
        // save of one record must carry the rest across it.
        let mut target = Aligned(read_target_record(page));
        let mut fixed = Aligned(read_fixed_record(page));
        let mut media = Aligned(read_media_record(page));
        let mut name = Aligned(read_name_record(page));
        // Which record this request names, its ticket, and the word the
        // log line uses. The ticket goes back through the slot on every
        // exit path below — that is what the requester's ack waits on.
        let (slot, ticket, what) = match request {
            Either4::First((ticket, wire)) => {
                target = Aligned(encode_telemetry_target(&wire));
                (&TARGET_PERSIST, ticket, "target")
            }
            Either4::Second((ticket, position)) => {
                fixed = Aligned(encode_fixed_position(position.as_ref()));
                (&FIXED_PERSIST, ticket, "fixed-position")
            }
            Either4::Third((ticket, profile)) => {
                media = Aligned(encode_media_profile(&profile));
                (&MEDIA_PERSIST, ticket, "media-profile")
            }
            Either4::Fourth((ticket, chosen)) => {
                name = Aligned(encode_node_name(chosen.as_ref()));
                (&NAME_PERSIST, ticket, "node-name")
            }
        };

        // Read-compare-write: an unchanged page is never erased. A host
        // tool that re-sends the same value on every connect must not
        // burn a flash cycle for it. Durable, not skipped, as far as the
        // requester is concerned: the value it asked for is on the page,
        // it simply did not need writing.
        if read_target_record(page) == target.0
            && read_fixed_record(page) == fixed.0
            && read_media_record(page) == media.0
            && read_name_record(page) == name.0
        {
            crate::log::log_fmt("[TELEMETRY] ", format_args!("persist skipped, unchanged"));
            slot.finish(ticket, Persisted::Durable);
            continue;
        }

        let mut written = false;
        for attempt in 1..=SAVE_RETRIES {
            let result = async {
                let mut flash = flash.lock().await;
                flash.erase(page, page + 4096).await?;
                flash.write(page + TARGET_OFFSET, &target.0).await?;
                flash.write(page + FIXED_POSITION_OFFSET, &fixed.0).await?;
                flash.write(page + MEDIA_OFFSET, &media.0).await?;
                flash.write(page + NAME_OFFSET, &name.0).await
            }
            .await;
            match result {
                Ok(()) => {
                    written = true;
                    break;
                }
                Err(_) => {
                    crate::log::log_fmt(
                        "[TELEMETRY] ",
                        format_args!("persist write failed, attempt {}/{}", attempt, SAVE_RETRIES),
                    );
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(SAVE_RETRY_MS))
                        .await;
                }
            }
        }

        if written {
            crate::log::log_fmt("[TELEMETRY] ", format_args!("persist saved {}", what));
        } else {
            crate::log::log_fmt(
                "[TELEMETRY] ",
                format_args!("persist gave up after retries"),
            );
        }
        // After the log line and on both paths: the requester is blocked
        // on this, and what it is owed is the outcome, not silence.
        slot.finish(
            ticket,
            if written {
                Persisted::Durable
            } else {
                Persisted::Lost
            },
        );
    }
}

/// Spawn the store task, borrowing the shared SoftDevice flash handle
/// ([`crate::flash::shared_flash`]). Call after `Softdevice::enable`.
#[cfg(feature = "softdevice")]
pub fn spawn_store_task(
    spawner: &embassy_executor::Spawner,
    flash: &'static crate::flash::SharedFlash,
    page: u32,
) {
    spawner.must_spawn(store_task(flash, page));
}

// ---------------------------------------------------------------------------
// Sensor readings
// ---------------------------------------------------------------------------

/// Horizontal position error in metres per unit of HDOP.
///
/// The wire's accuracy slot is metres (`Location.pack`, Sideband
/// `2000d81`; Columba fills it from Android's `Location.accuracy`), and
/// standard NMEA carries no accuracy field at all — only HDOP, which is a
/// geometry factor and not a distance. Turning one into the other needs a
/// user-equivalent range error, and 5 m is the conventional open-sky
/// figure for a single-frequency consumer receiver.
///
/// This is a **model, stated as one**, in the sense
/// `docs/src/concepts/regulatory-airtime.md` fixes for any such figure:
/// it is not a measurement of this antenna in this housing, and a
/// deployment that has measured its own may say so. What it is not is a
/// placeholder — it is derived from a number the receiver actually
/// reported, and when the receiver reports no HDOP the position is not
/// sent at all.
pub const HORIZONTAL_UERE_M: f32 = 5.0;

/// What this board could read at the moment a report was due.
///
/// Every field is optional and an absent one contributes no sensor key —
/// the concept's absence encoding, applied at the point the readings are
/// collected rather than deep inside the codec.
#[derive(Debug, Clone, Copy, Default)]
pub struct Readings {
    /// Unix seconds from the node's calendar. `None` is a node whose
    /// calendar was never seeded; it reports nothing, because a telemetry
    /// row with no time is a row a collector cannot order.
    pub unix_secs: Option<u64>,
    /// Decimal degrees, from a receiver in presence state `Fix`.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Metres above mean sea level (GGA).
    pub altitude_m: Option<f32>,
    /// Ground speed in metres per second (RMC).
    pub speed_mps: Option<f32>,
    /// Course over ground in degrees (RMC).
    pub bearing_deg: Option<f32>,
    /// Horizontal dilution of precision (GGA).
    pub hdop: Option<f32>,
    /// Battery charge, percent.
    pub battery_percent: Option<u8>,
    /// The nRF52's own die temperature in **quarter-degrees Celsius**, as
    /// [`die_temperature_quarter_c`] read it. The raw fixed-point unit is
    /// kept all the way to the codec on purpose — see
    /// [`Readings::telemetry`] for why turning it into a float early would
    /// invent precision the sensor does not have.
    pub die_temperature_quarter_c: Option<i32>,
}

impl Readings {
    /// The position as the policy wants it: scaled integers plus the
    /// accuracy number the threshold is applied to.
    pub fn fix(&self) -> Option<Fix> {
        let latitude = self.latitude?;
        let longitude = self.longitude?;
        Some(Fix {
            latitude_e6: (latitude * 1e6) as i32,
            longitude_e6: (longitude * 1e6) as i32,
            hdop_e2: self.hdop.map(|h| (h * 100.0).clamp(0.0, 65535.0) as u16),
        })
    }

    /// The location sensor built from the GNSS readings, or `None` when
    /// the receiver contributed nothing usable. Whether it goes into the
    /// report at all is the policy's answer, not this function's — the
    /// caller ([`Reporter::tick`]) applies that gate and the fixed
    /// position's precedence before asking for it.
    fn sensor_location(&self) -> Option<Location> {
        let fix = self.fix()?;
        let accuracy_m = self.hdop? * HORIZONTAL_UERE_M;
        Some(Location::saturating(
            fix.latitude_e6 as i64,
            fix.longitude_e6 as i64,
            (self.altitude_m.unwrap_or(0.0) * 100.0) as i64,
            (self.speed_mps.unwrap_or(0.0) * 100.0) as i64,
            (self.bearing_deg.unwrap_or(0.0) * 100.0) as i64,
            (accuracy_m * 100.0) as i64,
            self.unix_secs.unwrap_or(0) as i64,
        ))
    }

    /// Assemble the Telemeter for one report, with the location the
    /// caller decided on (`None` for a report that carries no position).
    ///
    /// The temperature is packed as a bare number in degrees Celsius
    /// (`SID_TEMPERATURE`), by the codec's own converter — the rounding
    /// convention is Sideband's and belongs beside the encoder that has to
    /// match it, which is why the quarter-degrees travel this far
    /// unconverted.
    pub fn telemetry(&self, location: Option<Location>) -> Telemetry {
        Telemetry {
            time: self.unix_secs.map(|s| s as i64),
            location,
            temperature: self
                .die_temperature_quarter_c
                .map(celsius_from_quarter_degrees),
            battery: self.battery_percent.map(|percent| Battery {
                charge_percent: Number::Int(percent as i64),
                // The baseboard reads a voltage divider, which cannot tell
                // charging from discharging. Nil is the reference's own
                // "platform did not say", not a claim that it is idle.
                charging: None,
                temperature: None,
            }),
            ..Telemetry::default()
        }
    }
}

/// The nRF52's die temperature in quarter-degrees Celsius, or `None` if
/// the SoftDevice refused the call.
///
/// # The constraint
///
/// **The SoftDevice owns the TEMP peripheral.** It is one of the
/// peripherals handed to [`crate::ble::init`] and never touched again
/// (`_temp`), because the S140 uses it for its own calibration and blocks
/// application access: `NRF_TEMP->TASKS_START` from our side while the
/// stack is enabled reads back garbage or hangs. The one legal path is the
/// `sd_temp_get` syscall, which is what `nrf_softdevice::temperature_celsius`
/// wraps (`nrf-softdevice/src/temperature.rs`). Blocks ~50 µs, which is why
/// it is called once per telemetry evaluation and not per poll.
///
/// `I30F2` is Q30.2 — the raw register unit, 0.25 °C per step — and
/// `to_bits` is those quarter-degrees.
#[cfg(feature = "softdevice")]
pub fn die_temperature_quarter_c(sd: &nrf_softdevice::Softdevice) -> Option<i32> {
    nrf_softdevice::temperature_celsius(sd)
        .ok()
        .map(|celsius| celsius.to_bits())
}

/// Wire accuracy of a user-set fixed position: 0.01 m, `accuracy_e2 = 1`.
///
/// This is Sideband's own convention for exactly this feature, not our
/// aesthetics: its fixed-location setting synthesizes the location sensor
/// (`SidebandCore.update_telemeter_config`, Sideband `2000d81`) and the
/// synthesized branch of `Location.update_data` (same commit) fills the
/// unset fields as `altitude = 0.0`, `accuracy = 0.01`, `speed = 0.0`,
/// `bearing = 0.0` before packing — so a Columba/Sideband renderer already
/// treats a 0.01 m reading as "a stated location". [`fixed_location`]
/// reproduces that shape byte for byte.
pub const FIXED_ACCURACY_E2: u16 = 1;

/// The [`Location`] a user-set fixed position reports.
///
/// Speed and bearing are 0 and the altitude defaults to 0 — the
/// reference's synthesized-location fill-ins (see [`FIXED_ACCURACY_E2`]).
/// `last_update` is the report's own timebase: the position is a standing
/// assertion, current as of every report that carries it.
pub fn fixed_location(wire: &FixedPositionWire, unix_secs: u64) -> Location {
    Location::saturating(
        wire.latitude_e6 as i64,
        wire.longitude_e6 as i64,
        wire.altitude_e2.unwrap_or(0) as i64,
        0,
        0,
        FIXED_ACCURACY_E2 as i64,
        unix_secs as i64,
    )
}

/// The fixed position as the policy wants it: the coordinates, with the
/// no-dilution HDOP that passes every profile's accuracy gate by
/// construction ([`FIXED_POSITION_HDOP_E2`]).
fn fixed_fix(wire: &FixedPositionWire) -> Fix {
    Fix {
        latitude_e6: wire.latitude_e6,
        longitude_e6: wire.longitude_e6,
        hdop_e2: Some(FIXED_POSITION_HDOP_E2),
    }
}

// ---------------------------------------------------------------------------
// The reporter
// ---------------------------------------------------------------------------

/// How often the node re-asks for a path while it is waiting for a
/// target's key. A path request is one small packet and the answer
/// carries the identity, so this is the key-resolution loop; 60 s keeps
/// it well under any announce cadence it might be racing.
const KEY_REQUEST_INTERVAL_MS: u64 = 60_000;

/// The reporting half of a node: policy, configured target, and the
/// delivery destination reports are signed by.
pub struct Reporter {
    policy: SendPolicy,
    target: Option<TelemetryTargetWire>,
    /// The user-set fixed position. While set it replaces the sensor as
    /// the reported position entirely ([`choose_position`]); the decided
    /// semantics, not a preference.
    fixed_position: Option<FixedPositionWire>,
    delivery_hash: DestinationHash,
    /// Monotonic time of the last path request issued while awaiting a
    /// key; `None` before the first one.
    last_key_request_ms: Option<u64>,
    /// Why the last tick did not send, so the reason is logged when it
    /// changes and not twelve times a minute for as long as it lasts. A
    /// blocked reporter must be legible in a log tail, which a flood is
    /// not.
    last_withheld: Option<&'static str>,
    /// The state the last `[TELEMETRY] target=… state=…` line named, so a
    /// standing state is stated once and not every five seconds. Same
    /// argument as [`last_withheld`](Self::last_withheld): a blocked
    /// reporter has to be legible in a log tail, which a flood is not.
    last_state_line: Option<TargetState>,
    /// What the pending report says once its dispatch is settled: the
    /// reason it was sent, the position it carried and the timebase it
    /// stamped. Held only between [`tick`](Self::tick) and
    /// [`note_dispatch`](Self::note_dispatch) — the report line is written
    /// there, not here, because until then it is not known whether there
    /// is a report to write a line about.
    pending_line: Option<PendingLine>,
    /// The flash record still lacks the target's public key, and the
    /// record must gain it once the key is known (Codeberg #370). Armed by
    /// every hash-only [`apply_target`](Self::apply_target), spent by the
    /// first tick that finds the target usable.
    ///
    /// The rule this carries: **a resolved key survives the reboot.** The
    /// host sends hash-only targets (the #236 UX decision — users know the
    /// LXMF address, not the key) and the identity store is RAM, so a
    /// power cycle used to forget the key the node had already resolved
    /// and land every restored target back in awaiting-key, silent until
    /// the target's next announce happened to reach it — thirty-seven
    /// minutes of nothing on walk 6. Rewriting the record with the key
    /// makes the next boot restore `ready`, which owes the immediate
    /// report like any other newly usable target.
    key_persist_owed: bool,
    /// The proof wait of the report last counted as sent (#373): the
    /// transport tracks a receipt per single packet and its events —
    /// `PacketDeliveryConfirmed` on a verified proof, `DeliveryFailed`
    /// on `ReceiptTimeout` — drive this machine. First loss owes one
    /// retransmission, second loss gives the report up. State only; the
    /// bytes to resend are [`retry_payload`](Self::retry_payload).
    proof: ProofTracker,
    /// The exact LXMF bytes of the report awaiting proof, kept so the
    /// retransmission is IDENTICAL — same position, same time; a
    /// retransmission, not a new reading. Dropped on proof, on give-up,
    /// and when a new report supersedes the wait.
    retry_payload: Option<Vec<u8>>,
    /// Whether the owed retransmission has already asked for a path.
    /// One request per owed retry: `request_path` is unthrottled, the
    /// telemetry tick is five-secondly, and the answer (or a peer-up
    /// pull) flips `has_path` — asking again every tick would be a storm.
    retry_path_requested: bool,
    /// The LXMF bytes of the report between [`tick`](Self::tick) and
    /// [`note_dispatch`](Self::note_dispatch), promoted to
    /// [`retry_payload`](Self::retry_payload) only if the dispatch
    /// settles as sent — a report that never left the board gets the
    /// ordinary cadence re-emission, not the proof machinery.
    pending_payload: Option<Vec<u8>>,
}

/// The report line of a report that has been handed to transport.
#[derive(Clone, Copy)]
struct PendingLine {
    reason: ReportReason,
    include_position: bool,
    /// Which source the position slot answered from — `fixed` while a
    /// fixed position is set, `gnss` otherwise, whether or not a position
    /// was carried. The honest source marker the batch requires.
    possrc: PositionSource,
    unix_secs: u64,
    /// The interfaces this report's own frames were handed to, which is
    /// what decides whether it went out (#348). The announce that shares
    /// the dispatch is not in here: it is a broadcast toward everyone and
    /// its fate on a third interface says nothing about this report.
    route: EmissionRoute,
    /// The packet hash the transport tracks the receipt under, and when
    /// it was emitted — what arms the proof wait once the dispatch
    /// settles as sent (#373).
    packet_hash: PacketHash,
    sent_ms: u64,
}

/// The first four bytes of a packet hash for the `pkt=` slot of the
/// `[TELEMETRY]` proof lines, same shape as [`Reporter::target_short`].
fn pkt_short(hash: &PacketHash) -> u32 {
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
}

impl Reporter {
    /// A reporter with no target. `delivery_hash` is the node's own
    /// registered `lxmf.delivery` destination — the one a receiver
    /// verifies our signature against, which is why announcing it is not
    /// optional.
    ///
    /// The position sources come from [`declare_position_sources`], which
    /// the binary called before USB came up — one owner for the fact, so
    /// the answer a host query already got cannot disagree with the one the
    /// policy runs on.
    pub fn new(delivery_hash: DestinationHash) -> Self {
        let mut policy = SendPolicy::new();
        policy.set_position_source(has_position_source());
        Self {
            policy,
            target: None,
            fixed_position: None,
            delivery_hash,
            last_key_request_ms: None,
            last_withheld: None,
            last_state_line: None,
            pending_line: None,
            key_persist_owed: false,
            proof: ProofTracker::new(),
            retry_payload: None,
            retry_path_requested: false,
            pending_payload: None,
        }
    }

    /// Say what state the target is in, once per change.
    ///
    /// The same line [`log_banner`](Self::log_banner) writes, on the same
    /// cadence rule the awaiting-key line has always had: emitted when the
    /// state becomes true and not again while it stays true. That rule is
    /// what makes `state=no-position-source` visible to an operator who
    /// attached a capture after boot without turning the log into a
    /// five-second drum.
    fn note_state(&mut self) {
        let state = self.policy.state();
        if self.last_state_line != Some(state) {
            self.last_state_line = Some(state);
            crate::log::log_fmt_critical(
                "[INFO!] ",
                format_args!(
                    "[TELEMETRY] target={:08x} state={}",
                    self.target_short(),
                    state.as_str()
                ),
            );
        }
    }

    /// Log `reason` once, until something else happens.
    fn withhold(&mut self, reason: &'static str) {
        if self.last_withheld != Some(reason) {
            self.last_withheld = Some(reason);
            crate::log::log_fmt_critical(
                "[INFO!] ",
                format_args!(
                    "[TELEMETRY] report withheld target={:08x} reason={}",
                    self.target_short(),
                    reason
                ),
            );
        }
    }

    pub fn state(&self) -> TargetState {
        self.policy.state()
    }

    pub fn profile(&self) -> Profile {
        self.policy.profile()
    }

    /// The configured target's destination hash, if any.
    pub fn target_hash(&self) -> Option<DestinationHash> {
        self.target.map(|t| DestinationHash::new(t.dest_hash))
    }

    /// Whether telemetry is switched off, which is what the caller uses
    /// to decide it has nothing to wake up for.
    pub fn is_off(&self) -> bool {
        self.policy.state() == TargetState::Off
    }

    /// The first four bytes of the target hash, for the `[TELEMETRY]`
    /// events. Zero when there is no target.
    pub fn target_short(&self) -> u32 {
        self.target
            .map(|t| {
                u32::from_be_bytes([
                    t.dest_hash[0],
                    t.dest_hash[1],
                    t.dest_hash[2],
                    t.dest_hash[3],
                ])
            })
            .unwrap_or(0)
    }

    /// Apply a target frame from the host or from flash.
    ///
    /// `profile == TELEMETRY_PROFILE_OFF` clears; anything else sets. An
    /// unknown profile id falls back to the default profile rather than
    /// refusing the destination — a newer host's cadence preference is
    /// not worth losing the target over, and the state the node reports
    /// says which profile it actually runs.
    ///
    /// If the frame carried a public key, it is remembered here, which is
    /// what lets a key-bearing target go straight to `ready`.
    pub fn apply_target<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        wire: TelemetryTargetWire,
    ) -> TargetOutcome
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        self.last_key_request_ms = None;
        self.last_withheld = None;
        self.last_state_line = None;
        // Whatever was awaiting proof was a report to the OLD target (or
        // to the old cadence's satisfaction) — its proof belongs to
        // nobody now, and retransmitting it would report to a recipient
        // the operator just changed away from.
        self.proof.clear();
        self.retry_payload = None;
        self.retry_path_requested = false;
        let command = command_from_wire(wire.profile);
        // A hash-only frame leaves a hash-only record on the page, and the
        // record is owed the key once it is known ([`key_persist_owed`]
        // (Self::key_persist_owed)) — whether that is on the next tick (a
        // re-applied target whose key is already held) or when it arrives
        // over the air. A frame that carried its key owes nothing: the
        // serial task persists it as sent.
        self.key_persist_owed =
            matches!(command, TargetCommand::Set(_)) && wire.public_key.is_none();
        let key_known = match command {
            TargetCommand::Clear => {
                self.target = None;
                false
            }
            TargetCommand::Set(_) => {
                let hash = DestinationHash::new(wire.dest_hash);
                // A frame that carried a key skips the over-the-air
                // resolution entirely, which is the whole benefit of
                // carrying one.
                if let Some(key) = wire.public_key {
                    if let Ok(identity) = Identity::from_public_key_bytes(&key) {
                        node.remember_identity(hash, identity);
                    }
                }
                self.target = Some(wire);
                node.storage().get_identity(hash.as_bytes()).is_some()
            }
        };
        self.policy.apply(command, key_known)
    }

    /// Apply a fixed position from the host or from flash; `None` is the
    /// explicit clear, returning the node to sensor reporting.
    ///
    /// Both paths — boot load and runtime frame — come through here, like
    /// the target's `apply_target`. The policy re-arms the immediate
    /// report when the target is usable, so the operator who changed what
    /// the node claims about itself sees the confirmation; a boot with a
    /// persisted position behaves like a boot with a persisted key-bearing
    /// target, which already reports once on coming up.
    /// A set pin is also the position source the send condition wants, so
    /// this is the runtime path out of [`TargetState::NoPositionSource`]
    /// and — on a board with no receiver — back into it. No reboot either
    /// way: the state is recomputed here and stated on the next tick.
    pub fn apply_fixed_position(&mut self, position: Option<FixedPositionWire>) {
        self.fixed_position = position;
        note_fixed_position(position.is_some());
        self.policy.set_position_source(has_position_source());
        self.policy.note_position_config_changed();
    }

    /// The user-set fixed position, if one is set.
    pub fn fixed_position(&self) -> Option<FixedPositionWire> {
        self.fixed_position
    }

    /// One evaluation step. Returns the packets the caller must dispatch;
    /// an empty vector is the common case.
    ///
    /// The caller decides the tick rate, and that rate is the retry rate
    /// for a report the radio could not take — the policy re-arms until
    /// [`SendPolicy::note_sent`] confirms one went out.
    pub fn tick<R, C, S>(
        &mut self,
        node: &mut NodeCore<R, C, S>,
        now_ms: u64,
        presence_has_fix: bool,
        readings: &Readings,
    ) -> Vec<Action>
    where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        let mut actions = Vec::new();
        let Some(target) = self.target else {
            return actions;
        };
        let hash = DestinationHash::new(target.dest_hash);

        // Say where the target stands whenever that changes — which is
        // what makes both directions of the position-source switch visible
        // to a log tail, the return trip as much as the block. Deduped, so
        // a standing state costs one line and not one every five seconds.
        self.note_state();

        if self.policy.state() == TargetState::NoPositionSource {
            // A target and no answer to "where am I": the node sends
            // nothing. Ahead of the key resolution below on purpose — a
            // path request is airtime spent chasing a key for reports that
            // will never be built.
            return actions;
        }

        if self.policy.state() == TargetState::AwaitingKey {
            if node.storage().get_identity(hash.as_bytes()).is_some() {
                if self.policy.note_key_available() {
                    self.note_state();
                }
            } else {
                // Resolve the key over the air. A path request is
                // answered with the destination's announce, which carries
                // the identity — that is the whole mechanism behind
                // hash-only configuration.
                let due = match self.last_key_request_ms {
                    None => true,
                    Some(last) => now_ms.saturating_sub(last) >= KEY_REQUEST_INTERVAL_MS,
                };
                if due {
                    self.last_key_request_ms = Some(now_ms);
                    actions.extend(node.request_path(&hash).actions);
                }
                return actions;
            }
        }

        // The target is usable from here on. If the flash record is still
        // hash-only, rewrite it with the key, so the next boot restores
        // `ready` instead of an awaiting-key that has forgotten what this
        // boot resolved (#370, [`key_persist_owed`](Self::key_persist_owed)).
        // On a tick rather than inside `apply_target`: the serial task
        // persists the host's own hash-only frame and blocks on that save
        // before answering, so a save requested here — at least one tick
        // later — lands after it and is the one the page keeps.
        if self.key_persist_owed {
            if let Some(identity) = node.storage().get_identity(hash.as_bytes()) {
                let enriched = TelemetryTargetWire {
                    public_key: Some(identity.public_key_bytes()),
                    ..target
                };
                self.target = Some(enriched);
                self.key_persist_owed = false;
                // Nobody acks this save: it is owed to the next boot, not
                // to a waiting host, so the ticket is dropped rather than
                // confirmed. A lost write degrades to the old behaviour —
                // one more over-the-air resolution — and the next
                // transition to usable retries it.
                let _ = request_save(&enriched);
                crate::log::log_fmt(
                    "[TELEMETRY] ",
                    format_args!(
                        "target={:08x} key resolved, persisting with record",
                        self.target_short()
                    ),
                );
            }
        }

        // An owed retransmission (#373): the report that got no proof
        // goes out ONCE more, same bytes, through the ordinary send path
        // — so a path that went offline in between is re-resolved by the
        // ordinary rules (75b14c8: a path over an offline interface is no
        // path; 482572a: the relay re-originates the request toward its
        // live peers). It does not touch the cadence: `note_emitted` is
        // not called, so `min_interval_ms` for the next report still
        // counts from the original attempt — neither shortened nor
        // stretched — and the interface applies its airtime/CSMA rules to
        // the retransmission like to any packet. Returns early: one
        // telemetry emission per tick.
        if let Some(first) = self.proof.awaiting_retry() {
            let Some(payload) = self.retry_payload.take() else {
                // Unreachable by construction (the payload is kept as
                // long as the debt is), but a debt without bytes can only
                // be forgotten, not paid.
                self.proof.clear();
                return actions;
            };
            if !node.has_path(&hash) {
                self.retry_payload = Some(payload);
                if !self.retry_path_requested {
                    self.retry_path_requested = true;
                    actions.extend(node.request_path(&hash).actions);
                }
                // The debt stands; the path answer (or a peer-up pull)
                // flips `has_path` and a later tick pays it. A next
                // scheduled report supersedes it via `note_report_sent`.
                return actions;
            }
            match node.send_single_packet(&hash, &payload) {
                Ok((retry_hash, out)) => {
                    actions.extend(out.actions);
                    // The payload is spent: exactly one retransmission,
                    // whatever becomes of it. The retry's own receipt is
                    // the backstop — its timeout is the give-up.
                    self.proof.note_retry_sent(retry_hash);
                    crate::log::log_fmt_critical(
                        "[INFO!] ",
                        format_args!(
                            "[TELEMETRY] retry pkt={:08x} reason=no-proof",
                            pkt_short(&first)
                        ),
                    );
                }
                Err(_) => {
                    // The path vanished between `has_path` and the send:
                    // keep the debt, next tick re-resolves.
                    self.retry_payload = Some(payload);
                }
            }
            return actions;
        }

        // Only a receiver in presence state Fix may contribute a sensor
        // position — and a set fixed position replaces the sensor
        // entirely, whatever the receiver is doing ([`choose_position`]).
        let sensor_fix = if presence_has_fix {
            readings.fix()
        } else {
            None
        };
        let (fix, possrc) =
            choose_position(self.fixed_position.as_ref().map(fixed_fix), sensor_fix);
        let Some(reason) = self.policy.poll(now_ms, fix) else {
            return actions;
        };

        let include_position = fix.map(|f| self.policy.position_is_reportable(f)) == Some(true);
        let location = match (include_position, possrc, &self.fixed_position) {
            (false, ..) => None,
            (true, PositionSource::Fixed, Some(wire)) => {
                Some(fixed_location(wire, readings.unix_secs.unwrap_or_default()))
            }
            (true, ..) => readings.sensor_location(),
        };
        let telemetry = readings.telemetry(location);
        let Some(unix_secs) = readings.unix_secs else {
            // The emission timebase is still below the plausibility floor,
            // which means it is uptime seconds and not a calendar estimate
            // at all — the anchor model's "never ahead" rule has nothing
            // to be applied to. Two concrete harms, not one cosmetic one:
            // the row lands in 1970 at the receiver, and uptime seconds
            // repeat across reboots, so two reports from two boots can
            // collide on Sideband's `(source, ts)` dedup key.
            self.withhold("no-clock");
            return actions;
        };

        // Announce our delivery destination first: a receiver verifies the
        // LXMF signature against our public key, which it can only have
        // from an announce. Sending the report to a peer that has never
        // heard us is sending an unverifiable reading.
        if let Ok(out) = node.announce_destination(
            &self.delivery_hash,
            Some(&announce_app_data(node.identity())),
        ) {
            actions.extend(out.actions);
        }

        let message = match build_report(
            target.dest_hash,
            self.delivery_hash.into_bytes(),
            node.identity(),
            unix_secs as f64,
            &telemetry,
        ) {
            Ok(message) => message,
            Err(_) => {
                self.withhold("no-readings");
                return actions;
            }
        };
        let Ok(on_air) = message.on_air() else {
            return actions;
        };

        if !node.has_path(&hash) {
            // No usable route. Distinguish "no entry at all" from "entry
            // over an offline interface" (#365) — both fall through to a
            // path request, which the broadcast puts on the carriers that
            // are still online.
            let reason = if node.path_route(&hash).is_some() {
                "iface-offline"
            } else {
                "no-path"
            };
            actions.extend(node.request_path(&hash).actions);
            self.withhold(reason);
            return actions;
        }

        // The routing decision, stated at the moment it is made (#365):
        // a capture must show "sent to a live carrier" (this line,
        // online=y, then the `report` line once the dispatch settles),
        // "handed to a dead one" (online=n — unreachable while the
        // has_path gate above holds, kept honest in case of a race) and
        // "not sent" (the `report withheld` line) as three different
        // shapes. Frozen in docs/src/structured-event-logs.md.
        if let Some((iface_idx, next_hop, online)) = node.path_route(&hash) {
            let mut next_hop_hex = [0u8; 4];
            let next_hop = match next_hop {
                Some(nh) => {
                    next_hop_hex.copy_from_slice(&nh[..4]);
                    Some(u32::from_be_bytes(next_hop_hex))
                }
                None => None,
            };
            match next_hop {
                Some(nh) => crate::log::log_fmt_critical(
                    "[INFO!] ",
                    format_args!(
                        "[TELEMETRY] send dst={:08x} via={} next_hop={:08x} online={}",
                        self.target_short(),
                        node.interface_name(iface_idx).unwrap_or("?"),
                        nh,
                        if online { "y" } else { "n" }
                    ),
                ),
                None => crate::log::log_fmt_critical(
                    "[INFO!] ",
                    format_args!(
                        "[TELEMETRY] send dst={:08x} via={} next_hop=direct online={}",
                        self.target_short(),
                        node.interface_name(iface_idx).unwrap_or("?"),
                        if online { "y" } else { "n" }
                    ),
                ),
            }
        }

        match node.send_single_packet(&hash, &on_air) {
            Ok((packet_hash, out)) => {
                // Note the route before the actions are merged with the
                // announce's: after the merge there is no telling which
                // frame was whose, and that distinction is the whole of
                // the #348 fix.
                let mut route = EmissionRoute::new();
                for action in &out.actions {
                    if let Action::SendPacket { iface, .. } = action {
                        route.add(iface.0);
                    }
                }
                actions.extend(out.actions);
                // Handed to transport, not yet on the air. The cadence is
                // consumed in `note_dispatch`, once the dispatch has said
                // whether the frame reached an interface at all (#344) —
                // a full `LORA_OUTGOING` makes this arm succeed and the
                // dispatch that follows drop the packet, and the report
                // used to count as sent anyway.
                self.policy
                    .note_emitted(now_ms, if include_position { fix } else { None });
                self.pending_line = Some(PendingLine {
                    reason,
                    include_position,
                    possrc,
                    unix_secs,
                    route,
                    packet_hash,
                    sent_ms: now_ms,
                });
                self.pending_payload = Some(on_air);
            }
            Err(_) => {
                // The core could not build or route it at all. This path
                // already leaves the cadence unconsumed — `note_emitted`
                // is never reached — so it needs no settlement, and it
                // keeps the reason string `note_dispatch` reuses.
                self.withhold("send-failed");
            }
        }
        actions
    }

    /// Act on the events one dispatch produced: a Sideband
    /// `TELEMETRY_REQUEST` delivered to our own `lxmf.delivery`
    /// destination arms the immediate report (Codeberg #371), and the
    /// delivery events of the tracked report — `PacketDeliveryConfirmed`
    /// from a verified proof, `DeliveryFailed` from a receipt timeout —
    /// drive the proof wait (#373). The binaries therefore feed it the
    /// RX arms' events AND the `handle_timeout` arm's: the receipt
    /// timeout fires on a timeout tick, not on a reception.
    ///
    /// The gate is the configured target and nothing else for now — the
    /// requester the feature exists for *is* the target, and a general
    /// allow list is a later step (`docs/src/concepts/telemetry.md`). The
    /// screen is `leviculum_lxmf`'s: the sender's hash must match and the
    /// message's signature must verify against the target's identity, so
    /// a spoofed source hash buys nothing. The request's timebase is
    /// ignored: a node holds no history, and its answer is the current
    /// reading either way.
    ///
    /// Nothing is sent from here. An accepted request arms the policy's
    /// immediate report and the ordinary [`tick`](Self::tick) emits it,
    /// which is what subjects it to every existing rule — attempt floor,
    /// announce-first, dispatch settlement. The policy also rate-limits:
    /// one request-triggered report per `min_interval_ms` of the active
    /// profile, a request inside the window is logged and dropped.
    pub fn handle_inbound_events<R, C, S>(
        &mut self,
        node: &NodeCore<R, C, S>,
        events: &[NodeEvent],
        now_ms: u64,
    ) where
        R: CryptoRngCore,
        C: Clock,
        S: Storage,
    {
        for event in events {
            match event {
                NodeEvent::PacketDeliveryConfirmed { packet_hash } => {
                    if let Some(proven) = self.proof.note_proof(packet_hash, now_ms) {
                        // Delivered: the kept bytes have done their duty,
                        // proven whether the first send or the
                        // retransmission was the one that landed.
                        self.retry_payload = None;
                        crate::log::log_fmt_critical(
                            "[INFO!] ",
                            format_args!(
                                "[TELEMETRY] proof pkt={:08x} after={}",
                                pkt_short(&proven.first),
                                proven.after_ms
                            ),
                        );
                    }
                    continue;
                }
                NodeEvent::DeliveryFailed { packet_hash, .. } => {
                    match self.proof.note_failure(packet_hash, now_ms) {
                        FailureVerdict::RetryDue { .. } => {
                            // The debt is armed; the next `tick` pays it
                            // and writes the `retry` line at the moment
                            // the retransmission actually goes out.
                        }
                        FailureVerdict::GaveUp { first, after_ms } => {
                            self.retry_payload = None;
                            crate::log::log_fmt_critical(
                                "[INFO!] ",
                                format_args!(
                                    "[TELEMETRY] gave up pkt={:08x} after={}",
                                    pkt_short(&first),
                                    after_ms
                                ),
                            );
                        }
                        FailureVerdict::NotTracked => {}
                    }
                    continue;
                }
                _ => {}
            }
            let NodeEvent::PacketReceived {
                destination, data, ..
            } = event
            else {
                continue;
            };
            if *destination != self.delivery_hash {
                continue;
            }
            let allowed = self.target.map(|t| t.dest_hash);
            let identity = allowed.and_then(|hash| node.storage().get_identity(&hash));
            let verdict =
                screen_telemetry_request(data, self.delivery_hash.into_bytes(), allowed, identity);
            let (source, accepted, reason) = match verdict {
                // Ordinary inbound traffic — none of this feature's
                // business, and not worth a line.
                TelemetryRequestVerdict::NotARequest => continue,
                TelemetryRequestVerdict::NotAllowed { source } => (source, false, "not-allowed"),
                // No key for the target yet: the request cannot be
                // authenticated, and the policy is awaiting-key anyway.
                TelemetryRequestVerdict::Unverifiable { source } => (source, false, "not-ready"),
                TelemetryRequestVerdict::BadSignature { source } => {
                    (source, false, "bad-signature")
                }
                TelemetryRequestVerdict::Request { source, .. } => {
                    let outcome = self.policy.note_report_request(now_ms);
                    (source, outcome == RequestOutcome::Armed, outcome.as_str())
                }
            };
            crate::log::log_fmt_critical(
                "[INFO!] ",
                format_args!(
                    "[TELEMETRY] request from={:08x} {} reason={}",
                    u32::from_be_bytes([source[0], source[1], source[2], source[3]]),
                    if accepted { "accepted" } else { "rejected" },
                    reason
                ),
            );
        }
    }

    /// Settle the report `tick` handed over against what the dispatch did
    /// with it (#344).
    ///
    /// The caller passes the `DispatchResult` of the dispatch that carried
    /// this tick's actions. A report that did not go out leaves the
    /// cadence unconsumed, so the next ordinary tick emits it again;
    /// nothing is re-sent here and nothing is queued.
    ///
    /// **The report went out when every interface its own frames were
    /// addressed to accepted them** ([`EmissionRoute`]) — not when the
    /// dispatch was *clean*. `DispatchResult::is_clean` asks whether
    /// anything was lost anywhere, which is the right question for the
    /// `[DISPATCH_LOSS]` line `settle` writes and the wrong one here: on a
    /// board advertising BLE with no phone attached, the announce that
    /// shares this dispatch is refused by the BLE queue while LoRa puts the
    /// report on the air. Reading that refusal as "not emitted" left
    /// `last_report_ms` for ever unset, so a report was always due and only
    /// the attempt floor stood between two of them — one per minute against
    /// a fifteen-minute profile (#348).
    ///
    /// The announce's own fate is therefore no longer this report's: it is
    /// a broadcast toward everyone, and an interface with nobody behind it
    /// refusing a copy is not this report's failure. A loss on the
    /// interface the report itself used still is one, announce or report,
    /// and that keeps the older argument intact where it applies — a
    /// receiver on that interface that missed the announce cannot verify
    /// the report anyway.
    ///
    /// Silent when there is nothing pending — the common case, since the
    /// tick that sends is one in hundreds.
    pub fn note_dispatch(&mut self, result: &DispatchResult) {
        let Some(line) = self.pending_line.take() else {
            // No report this tick. `note_dispatch` still runs so a
            // caller never has to know whether one was emitted.
            let _ = self.policy.note_dispatch(false);
            return;
        };
        // `retries` repeats what `errors` already recorded for a
        // `BufferFull`; chaining all three costs one extra comparison and
        // keeps this total if that ever stops being true.
        let losses = result
            .errors
            .iter()
            .map(|(iface, _)| iface.0)
            .chain(result.drops.iter().map(|(iface, _)| iface.0))
            .chain(result.retries.iter().map(|retry| retry.iface_idx));
        if !self.policy.note_dispatch(line.route.went_out(losses)) {
            // Never left the board: the ordinary cadence re-emission
            // covers it, the proof machinery has nothing to wait for.
            // (The receipt the send created still times out in transport;
            // its failure event matches no tracked hash and falls through
            // `note_failure` as NotTracked.)
            self.pending_payload = None;
            self.withhold("send-failed");
            return;
        }
        // On the air: from here the proof decides. A report that gets no
        // proof within the receipt timeout is retransmitted once from
        // these kept bytes (#373).
        self.proof.note_report_sent(line.packet_hash, line.sent_ms);
        self.retry_payload = self.pending_payload.take();
        self.retry_path_requested = false;
        self.last_withheld = None;
        crate::log::log_fmt_critical(
            "[INFO!] ",
            format_args!(
                "[TELEMETRY] report target={:08x} reason={} position={} possrc={} unix={} src={}",
                self.target_short(),
                line.reason.as_str(),
                line.include_position as u8,
                line.possrc.as_str(),
                line.unix_secs,
                crate::time_source_str()
            ),
        );
    }

    /// The banner line, emitted beside `[TIME_SOURCE]` so a log tail
    /// always carries the current answer.
    ///
    /// Counts as the state having been stated, so the next tick does not
    /// repeat it: this line already carries `state=`, and two lines for one
    /// transition is how a log stops being read.
    pub fn log_banner(&mut self) {
        let state = self.state();
        self.last_state_line = Some(state);
        crate::log::log_fmt_critical(
            "[INFO!] ",
            format_args!(
                "[TELEMETRY] target={:08x} state={} profile={}",
                self.target_short(),
                state.as_str(),
                self.profile().as_str()
            ),
        );
    }
}

/// The `lxmf.delivery` announce payload: a display name, so a receiver
/// shows a name instead of a hex string.
///
/// The name is whatever [`crate::name::mesh_name`] says it is — the one
/// an operator set over the control envelope (#235/#238), or the derived
/// `LNode-<8 hex>` for a board nobody has named, because such a board
/// still has to be distinguishable from the next one on the bench. Read
/// on every announce rather than captured once, so a name set at runtime
/// is on the air from the next announce and not from the next boot.
///
/// The name's length is airtime: this payload is `5 + name` bytes and it
/// rides in every announce. `leviculum_core::node_name` derives the
/// bound from that cost and
/// `leviculum-lxmf/tests/announce_name_airtime.rs` pins the numbers.
pub fn announce_app_data(identity: &Identity) -> Vec<u8> {
    use leviculum_lxmf::announce::DeliveryAnnounce;
    let name = crate::name::mesh_name(identity.hash());
    // Stamp cost 0: the node mines nothing (the `pow` feature is off), so
    // advertising a cost it cannot pay itself would be a lie to every
    // peer that reads it.
    DeliveryAnnounce {
        display_name: Some(name.as_bytes().to_vec()),
        stamp_cost: None,
        compression_supported: false,
    }
    .encode()
}

/// Build and register the node's `lxmf.delivery` destination.
///
/// Returns its hash, which is the source a receiver verifies against.
/// Fails only if the identity cannot be cloned out of transport, which is
/// the same failure mode the probe destination has.
pub fn register_delivery_destination<R, C, S>(
    node: &mut NodeCore<R, C, S>,
) -> Option<DestinationHash>
where
    R: CryptoRngCore,
    C: Clock,
    S: Storage,
{
    let identity_bytes = node.identity().private_key_bytes().ok()?;
    let identity = Identity::from_private_key_bytes(&identity_bytes).ok()?;
    let destination = leviculum_lxmf::LxmfNode::delivery_destination(identity).ok()?;
    let hash = *destination.hash();
    node.register_destination(destination);
    Some(hash)
}
