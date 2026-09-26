//! The node's name: the one an operator chose, or the derived default
//! (Codeberg #235/#238).
//!
//! # One name, both surfaces
//!
//! A board is visible under a name in two places, and until #235 they
//! were two independently derived strings:
//!
//! * the **mesh display name**, the LXMF announce's `app_data` — what
//!   Columba lists (`crate::telemetry::announce_app_data`), default
//!   `LNode-<hex8>`;
//! * the **BLE name**, the GAP device name and the advertisement's
//!   Complete Local Name — what a phone shows in its Bluetooth settings
//!   (`leviculum_ble_tx::gap_name`), default `LN-<hex8>`.
//!
//! An operator who sets a name sets **both**. A feature that named only
//! one of them would leave the same board answering to two names in two
//! places, which is worse than the two hex defaults it replaced: at least
//! those share their hex.
//!
//! The name is display only. It never touches the identity, so two boards
//! may carry the same name and stay distinguishable — the hash is still
//! what addresses, dedupes and disambiguates.
//!
//! # When each surface adopts it
//!
//! * **Mesh: at once.** [`crate::telemetry::announce_app_data`] reads
//!   this module on every announce, so the next one carries the new name.
//! * **BLE: at the next boot.** Two BLE surfaces carry the name and they
//!   are not equally rewritable, which is exactly why both wait:
//!
//!   * The **advertisement's** Complete Local Name is built once into a
//!     `StaticCell` the SoftDevice holds a `&'static` to for the life of
//!     the advertising loop (`crate::ble::columba`). Rewriting it under a
//!     live stack means mutating a buffer the SoftDevice is reading, and
//!     it is what a phone's Bluetooth list actually shows. So it cannot
//!     follow before the next reset.
//!   * The **GAP attribute** could follow: `sd_ble_gap_device_name_set`
//!     copies the bytes out of its argument and is legal at runtime. It
//!     deliberately does not. Updating only the half a peer reads *after*
//!     connecting would leave the board advertising one name and
//!     answering with another — a third state, invented to avoid saying
//!     "reset the board", and harder to explain than the reset is.
//!
//!   So both BLE surfaces read the name at boot, together, and the
//!   control frame's report says plainly that they are one reset behind.
//!
//! That difference is what [`report_flags`] publishes as
//! `NODE_NAME_FLAG_BLE_PENDING`, and it is decided here rather than by
//! the host: only the board knows what its advertisement was built with,
//! and only the board knows that `LN-<hex8>` is a *different* default
//! from `LNode-<hex8>` rather than a truncation of it.
//!
//! The record itself lives on the telemetry flash page (layout in
//! [`crate::telemetry`]); the bytes are
//! [`leviculum_core::node_name_store`], and the policy — length,
//! character set, the airtime the length costs — is
//! [`leviculum_core::node_name`].

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;

use leviculum_ble_tx::{gap_name, DEVICE_NAME_LEN};
use leviculum_core::envelope::{NODE_NAME_FLAG_BLE_PENDING, NODE_NAME_FLAG_STORED};
use leviculum_core::node_name::NodeName;

/// Whether [`load_at_boot`] ran — the capability the serial task's name
/// answers are gated on.
///
/// The same ack-honesty rule as the telemetry reporter's and the media
/// gate's: a binary that never wired the name must refuse rather than
/// ack, because an operator told "this board is now Balkon-Nord" who then
/// finds it under neither name on either surface has been lied to, and
/// no retry or reboot fixes that.
static WIRED: AtomicBool = AtomicBool::new(false);

/// The operator-set name, or `None` for the derived defaults. Written by
/// the boot path and by the serial task, read by the announce path.
///
/// A critical-section `Cell` rather than the atomics
/// [`crate::media`] uses: the value is 33 bytes, so there is no atomic
/// wide enough and the read must not be able to see half of one name and
/// half of another. Uncontended in practice — the executor is
/// cooperative and single-core, so no holder is ever preempted.
static CONFIGURED: Mutex<CriticalSectionRawMutex, Cell<Option<NodeName>>> =
    Mutex::new(Cell::new(None));

/// The BLE name this boot actually came up on, recorded by
/// [`note_boot_name`] before `ble::init`. Length and bytes rather than a
/// [`NodeName`] because that is exactly the shape
/// [`leviculum_ble_tx::gap_name`] hands out and both consumers want.
static BOOT_GAP: Mutex<CriticalSectionRawMutex, Cell<([u8; DEVICE_NAME_LEN], usize)>> =
    Mutex::new(Cell::new(([0u8; DEVICE_NAME_LEN], 0)));

/// This node's identity hash, published by [`note_boot_name`].
///
/// The serial task needs it to state the *derived* names, and it comes up
/// long before the node does: `usb::init` runs a handful of statements
/// into `main`, the identity only after the LoRa bring-up's awaited SPI
/// transactions. Until it is here the board cannot say what an unnamed
/// board is called, and [`report`] answers `None` rather than reporting a
/// name built from sixteen zero bytes.
static IDENTITY: Mutex<CriticalSectionRawMutex, Cell<Option<[u8; 16]>>> =
    Mutex::new(Cell::new(None));

/// Read the persisted name, declare this binary's name gate, and return
/// what the boot should honour.
///
/// Call **before** `usb::init` — so a host frame can never be answered
/// against the derived default while the record is still unread — and
/// before the first announce and before `ble::init`, which both read the
/// answer. The read is an ordinary memory-mapped flash read and is legal
/// this early, including before `Softdevice::enable`.
pub fn load_at_boot(page: u32) -> Option<NodeName> {
    let stored = crate::telemetry::load_node_name(page);
    CONFIGURED.lock(|cell| cell.set(stored));
    WIRED.store(true, Ordering::Release);
    stored
}

/// Record the BLE name this boot is coming up on. Call once, immediately
/// before `ble::init`, with the identity hash that init is given.
///
/// Unconditional — including on a board whose media profile holds BLE
/// down. What it publishes is "the name the BLE surfaces of this boot
/// carry", and on a board with BLE off that is still the honest answer to
/// "would a reset change the advertised name": no. Whether anything is
/// advertising at all is the media report's question, not this one.
pub fn note_boot_name(identity_hash: &[u8; 16]) {
    let configured = configured();
    let name = gap_name(identity_hash, configured.as_ref().map(NodeName::as_str));
    BOOT_GAP.lock(|cell| cell.set(name));
    IDENTITY.lock(|cell| cell.set(Some(*identity_hash)));
}

/// Whether [`load_at_boot`] was called.
pub fn name_wired() -> bool {
    WIRED.load(Ordering::Acquire)
}

/// The operator-set name, or `None` when both surfaces are derived.
pub fn configured() -> Option<NodeName> {
    CONFIGURED.lock(|cell| cell.get())
}

/// The mesh display name in force right now: the operator's name, or the
/// derived `LNode-<hex8>`. This is what the next announce carries.
///
/// The derived form is built here rather than in
/// [`crate::telemetry::announce_app_data`] so that the announce path and
/// the control-frame report cannot disagree about what an unnamed board
/// is called.
pub fn mesh_name(identity_hash: &[u8; 16]) -> NodeName {
    match configured() {
        Some(name) => name,
        None => {
            let mut derived = *b"LNode-00000000";
            write_hex8(&mut derived[6..], identity_hash);
            // Ten graphic ASCII bytes at most — inside every bound
            // `decode` checks — so the fallback arm is unreachable.
            NodeName::decode(&derived).unwrap_or(NodeName::EMPTY)
        }
    }
}

/// The BLE name the advertisement and the GAP attribute are carrying
/// **right now**, as [`note_boot_name`] recorded it.
///
/// Before `ble::init` (and on a binary that never calls
/// [`note_boot_name`]) this is the empty name, which is the truthful "no
/// BLE name is on the air".
pub fn boot_gap_name() -> NodeName {
    let (buf, len) = BOOT_GAP.lock(|cell| cell.get());
    NodeName::decode(&buf[..len]).unwrap_or(NodeName::EMPTY)
}

/// The BLE name a reset would come up on, given the name configured
/// right now. Compared against [`boot_gap_name`] to decide
/// `NODE_NAME_FLAG_BLE_PENDING`.
pub fn pending_gap_name(identity_hash: &[u8; 16]) -> NodeName {
    let configured = configured();
    let (buf, len) = gap_name(identity_hash, configured.as_ref().map(NodeName::as_str));
    NodeName::decode(&buf[..len]).unwrap_or(NodeName::EMPTY)
}

/// Everything a
/// [`TYPE_NODE_NAME_REPORT`](leviculum_core::envelope::TYPE_NODE_NAME_REPORT)
/// carries: the flags, the mesh name in force, and the BLE name on the
/// air.
///
/// `None` means the board cannot state its names yet — [`note_boot_name`]
/// has not run, so the identity hash the derived defaults are built from
/// is not known. The serial task turns that into `REFUSE_BUSY` and the
/// host retries, which is the truth: it is a boot-order window of a few
/// hundred milliseconds, not a missing capability. Reporting
/// `LNode-00000000` instead would send an operator looking for a board
/// that does not exist.
///
/// `BLE_PENDING` compares the name BLE came up on against the one it
/// would come up on now — never the mesh name against the BLE name, which
/// differ both by truncation and by having two unrelated derived
/// defaults, and would report a freshly cleared board as pending forever.
pub fn report() -> Option<(u8, NodeName, NodeName)> {
    let identity_hash = IDENTITY.lock(|cell| cell.get())?;
    let stored = if configured().is_some() {
        NODE_NAME_FLAG_STORED
    } else {
        0
    };
    let ble = boot_gap_name();
    let pending = if ble == pending_gap_name(&identity_hash) {
        0
    } else {
        NODE_NAME_FLAG_BLE_PENDING
    };
    Some((stored | pending, mesh_name(&identity_hash), ble))
}

/// Apply a name a host sent — `None` clears it back to the derived
/// default — and persist it.
///
/// Runs on the serial task rather than being handed to the main loop:
/// the whole apply is one guarded store and a non-blocking save request,
/// and nothing here needs the node. So [`mesh_name`] reads back correct
/// the instant this returns, and the report the serial task writes
/// describes the state already in force rather than predicting one.
///
/// The returned [`crate::telemetry::PendingSave`] is the other half of
/// the answer, and the caller must not write the report before waiting on
/// it (`crate::telemetry::confirm`, Codeberg #358). The report says what
/// the next boot brings up on the BLE surfaces; sent while the page write
/// was still owed, it is a claim about a reboot the reboot would
/// disprove.
pub fn apply(name: Option<NodeName>) -> crate::telemetry::PendingSave {
    CONFIGURED.lock(|cell| cell.set(name));
    crate::telemetry::request_save_node_name(name)
}

/// **The proof line.** One per boot beside the `[MEDIA]` banner, and
/// re-emitted with the firmware build banner, so a capture says under
/// which names the board is visible without an operator having to
/// remember what they set.
///
/// ```text
/// [NAME ] mesh=Balkon-Nord ble=Balkon-Nord src=flash t=1183
/// ```
///
/// `mesh=` is the LXMF display name the announces carry, `ble=` the name
/// on the air for scanners, and `src=` whether the two came off the flash
/// page or from the derived defaults. The two differ when the name is
/// longer than `DEVICE_NAME_LEN` — visibly, which is the point.
///
/// Read live from [`report`] rather than from a value captured at boot,
/// for the reason the `[MEDIA]` line reads its carriers live: a name set
/// at runtime has to show up in the periodic line, and `src=` is part of
/// what changed. Silent until [`note_boot_name`] has published the
/// identity hash — the alternative is a banner naming a board that does
/// not exist, and the periodic task is spawned before the node is built.
pub fn log_banner() {
    let Some((_, mesh, ble)) = report() else {
        return;
    };
    crate::log::log_fmt_critical(
        "[NAME ] ",
        format_args!(
            "mesh={} ble={} src={}",
            mesh.as_str(),
            ble.as_str(),
            if configured().is_some() {
                "flash"
            } else {
                "derived"
            }
        ),
    );
}

/// Write the leading four bytes of `hash` as eight lowercase hex digits.
fn write_hex8(out: &mut [u8], hash: &[u8; 16]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, byte) in hash[..4].iter().enumerate() {
        out[2 * i] = HEX[usize::from(byte >> 4)];
        out[2 * i + 1] = HEX[usize::from(byte & 0x0F)];
    }
}
