//! Boot-phase breadcrumbs in noinit RAM (ledger local-pocket-dark-d66209e).
//!
//! The volatile half of `leviculum-boot-trace`: one fixed-layout
//! [`RawRecord`] in `.retained` (a NOLOAD region in memory.x that the
//! Adafruit bootloader provably never touches, so soft and pin resets
//! preserve RAM contents; power loss does not), a phase byte advanced at
//! named boot milestones, and the boot-time read of the PREVIOUS record
//! before this boot overwrites it. Everything decodable or printable
//! lives in the pure crate where it is host-tested; this module only
//! moves bytes.
//!
//! Discipline: [`phase`] is a single volatile byte store — interrupt-safe
//! (an ARM byte store is atomic), no flash write, no allocation, no
//! await. It is legal from any context, including between
//! `Softdevice::enable` and the first task spawn.

pub use leviculum_boot_trace::Phase;
use leviculum_boot_trace::{PrevBoot, RawRecord, TraceLine, MAGIC};

/// Survives `sys_reset` because `.retained` (memory.x) is NOLOAD —
/// cortex-m-rt's startup neither zeroes nor initialises it — AND lies
/// outside every RAM range the Adafruit bootloader uses on its way to
/// the app. `.uninit` failed the second condition: flip-link packs it
/// against the top of RAM, where the bootloader's stack starts
/// (`__StackTop` = 0x20040000 in its linker script), so every boot
/// clobbered the record before `capture` could read it — the rig showed
/// `prev_magic=absent` across commanded resets from a running system.
/// Same section as the post-mortems and the panic counter.
#[link_section = ".retained"]
static mut BOOT_TRACE: core::mem::MaybeUninit<RawRecord> = core::mem::MaybeUninit::uninit();

/// POWER.RESETREAS (0x40000000 + 0x400). embassy-nrf 0.9 keeps its pac
/// crate-private, so raw volatile access, as in the rest of this crate.
const POWER_RESETREAS: *mut u32 = 0x4000_0400 as *mut u32;

/// What [`capture`] read before arming this boot's record.
pub struct Captured {
    /// The previous boot's record, decoded.
    pub prev: PrevBoot,
    /// Raw RESETREAS at entry to `main`, cleared in the register after
    /// the read (write-1-to-clear: writing back the read value clears
    /// exactly the latched bits) so every boot reports only its own
    /// cause. POR and brownout latch NO bit — raw=0 on a boot that was
    /// clearly a reset points at the supply.
    pub reset_reason: u32,
}

/// Read the previous boot's breadcrumbs and RESETREAS, then arm this
/// boot's record at [`Phase::EnterMain`].
///
/// Call once, as the FIRST statement of `main` — before
/// `embassy_nrf::init`, so a hang waiting on the HFXO is attributable
/// (`prev_phase=enter-main`), and long before `Softdevice::enable`,
/// after which POWER belongs to the SD and this read would be a MEMACC
/// fault (#249). Touches only RAM and one pre-SD register; safe this
/// early.
pub fn capture() -> Captured {
    use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
    // SAFETY: single-shot, before any concurrent task exists; `.retained`
    // reads are of a possibly-never-written record, which is exactly what
    // the magic check in `decode` gates.
    unsafe {
        let p = addr_of_mut!(BOOT_TRACE).cast::<RawRecord>();
        let raw = RawRecord {
            magic: read_volatile(addr_of!((*p).magic)),
            boot: read_volatile(addr_of!((*p).boot)),
            phase: read_volatile(addr_of!((*p).phase)),
        };
        let prev = leviculum_boot_trace::decode(&raw);

        let reset_reason = read_volatile(POWER_RESETREAS);
        write_volatile(POWER_RESETREAS, reset_reason);

        // Arm this boot's record: body first, magic last, so a reset
        // mid-sequence can never leave a valid record with a stale body.
        write_volatile(addr_of_mut!((*p).phase), Phase::EnterMain as u8);
        write_volatile(
            addr_of_mut!((*p).boot),
            leviculum_boot_trace::next_boot(&prev),
        );
        write_volatile(addr_of_mut!((*p).magic), MAGIC);

        Captured { prev, reset_reason }
    }
}

/// Advance the phase byte: this milestone is now COMPLETED. One volatile
/// byte store, nothing else.
pub fn phase(p: Phase) {
    // SAFETY: a single aligned byte store is atomic on this core; the
    // record itself was armed by `capture` before any task could run.
    unsafe {
        let r = core::ptr::addr_of_mut!(BOOT_TRACE).cast::<RawRecord>();
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*r).phase), p as u8);
    }
}

/// Emit the previous boot's breadcrumbs as one structured line on the
/// debug port (if00):
///
/// ```text
/// BOOT_TRACE prev_magic=ok prev_phase=main-loop prev_boot=17 reset_reason=0x00000004 t=12
/// ```
///
/// Critical path: rides the boot-banner ring, so it is replayed from the
/// persistent tail too. The ` t=<ms>` stamp comes from the log formatter
/// like on every other line.
pub fn log_prev(c: &Captured) {
    crate::log::log_fmt_critical(
        "",
        format_args!(
            "{}",
            TraceLine {
                prev: c.prev,
                reset_reason: c.reset_reason,
            }
        ),
    );
}
