//! Boot-phase breadcrumb record for the noinit-RAM boot trace
//! (bug ledger local-pocket-dark-d66209e).
//!
//! A board that dies during boot before USB comes up is invisible from
//! outside: no enumeration, no log, no post-mortem — the capture shows
//! nothing at all. The breadcrumb record closes that gap. The firmware
//! keeps a tiny fixed-layout record in a `.uninit` section (NOLOAD in
//! cortex-m-rt's link.x, so soft and pin resets preserve it; power loss
//! does not), advances a phase byte at named boot milestones, and on
//! every boot reads the PREVIOUS record before overwriting it. The
//! resulting `BOOT_TRACE` line says exactly how far the dying boot got —
//! the button reset that recovers the board delivers the evidence.
//!
//! This crate is the pure, host-testable half: record layout, magic
//! handling, phase names, RESETREAS decode, and the byte-exact shape of
//! the emitted line. The volatile `.uninit` I/O stays in
//! `leviculum-nrf/src/boot_trace.rs`; the firmware crate cross-compiles
//! and runs no host tests, so everything greppable is asserted here
//! (same split as `leviculum-log-line`).

#![cfg_attr(not(test), no_std)]

use core::fmt;

/// Record-valid marker. Versioned like `PANIC_PM_MAGIC`: bump it if
/// [`RawRecord`]'s layout or the [`Phase`] byte assignment ever changes,
/// so a record left in `.uninit` by an older image reads as absent
/// rather than misparsed.
pub const MAGIC: u32 = 0xB007_7ACE;

/// Named boot milestones, in the order both bins pass them.
///
/// The byte value stored in the record is the last milestone COMPLETED:
/// `sd-enabled` is written after `Softdevice::enable` returns, so a boot
/// that hangs inside the enable still reads the milestone before it.
/// Zero is deliberately unassigned — freshly powered RAM that happens to
/// hold the magic must not decode to a plausible first phase.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Phase {
    /// First statement of `main`, before even `embassy_nrf::init` (which
    /// waits on the HFXO and is therefore itself a milestone boundary).
    EnterMain = 1,
    /// Media profile read off its flash page (`media::load_at_boot`).
    PersistRead = 2,
    /// `usb::init` returned: USB tasks spawned. Enumeration is async and
    /// later; a board stuck before THIS marker never enumerates.
    UsbUp = 3,
    /// LoRa task spawned per profile.
    LoraTask = 4,
    /// LoRa held down by the media profile (`lora=off`), task not spawned.
    LoraSkipped = 5,
    /// `Softdevice::enable` returned.
    SdEnabled = 6,
    /// SoftDevice event task spawned (`ble::init` about to return).
    BleTask = 7,
    /// Main event loop entered. The healthy steady state: every clean
    /// boot's record reads this until the next reset.
    MainLoop = 8,
}

impl Phase {
    /// The name the `BOOT_TRACE` line prints for this milestone.
    pub fn name(self) -> &'static str {
        match self {
            Phase::EnterMain => "enter-main",
            Phase::PersistRead => "persist-read",
            Phase::UsbUp => "usb-up",
            Phase::LoraTask => "lora-task",
            Phase::LoraSkipped => "lora-skipped",
            Phase::SdEnabled => "sd-enabled",
            Phase::BleTask => "ble-task",
            Phase::MainLoop => "main-loop",
        }
    }

    /// Decode a stored phase byte. `None` for a byte this image assigns
    /// no meaning to (older/newer image left it) — rendered as
    /// `unknown-0x<byte>`, never as a fabricated milestone.
    pub fn from_byte(b: u8) -> Option<Phase> {
        Some(match b {
            1 => Phase::EnterMain,
            2 => Phase::PersistRead,
            3 => Phase::UsbUp,
            4 => Phase::LoraTask,
            5 => Phase::LoraSkipped,
            6 => Phase::SdEnabled,
            7 => Phase::BleTask,
            8 => Phase::MainLoop,
            _ => return None,
        })
    }
}

/// The record as it sits in `.uninit` RAM. `repr(C)` because the layout
/// is a cross-boot contract: the reader of this boot parses what the
/// writer of the previous boot (possibly a different image) laid down,
/// with [`MAGIC`] as the version gate.
///
/// Field order is load-bearing for the writer: the firmware writes
/// `phase` and `boot` first and `magic` last, so a reset mid-write can
/// never leave a valid-looking record with a stale body.
#[repr(C)]
pub struct RawRecord {
    /// [`MAGIC`] when the record is valid.
    pub magic: u32,
    /// Monotonically bumped boot counter. Restarts at 1 after power loss.
    pub boot: u32,
    /// Last completed [`Phase`], as its `repr(u8)` value.
    pub phase: u8,
}

/// What the previous boot's record decodes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrevBoot {
    /// No valid record: first boot after power loss or after a flash of
    /// an image with a different layout version.
    Absent,
    /// A record left by the previous boot of a same-version image.
    Present {
        /// The previous boot's counter value.
        boot: u32,
        /// The stored phase byte, decoded lazily via [`Phase::from_byte`]
        /// so an unknown value stays visible instead of being dropped.
        phase_byte: u8,
    },
}

/// Decode a record read out of `.uninit`. Magic mismatch — including
/// never-written RAM — is [`PrevBoot::Absent`], never a fabricated phase.
pub fn decode(raw: &RawRecord) -> PrevBoot {
    if raw.magic == MAGIC {
        PrevBoot::Present {
            boot: raw.boot,
            phase_byte: raw.phase,
        }
    } else {
        PrevBoot::Absent
    }
}

/// The boot counter this boot writes into its fresh record: previous
/// counter plus one, or 1 on the first counted boot.
pub fn next_boot(prev: &PrevBoot) -> u32 {
    match prev {
        PrevBoot::Absent => 1,
        PrevBoot::Present { boot, .. } => boot.wrapping_add(1),
    }
}

/// Decoded nRF52840 `POWER.RESETREAS` bits. Bit layout from the product
/// spec (matches nrf-pac's `Resetreas`): 0 RESETPIN, 1 DOG, 2 SREQ,
/// 3 LOCKUP, 16 OFF, 17 LPCOMP, 18 DIF, 19 NFC, 20 VBUS.
///
/// An all-zero raw value on a boot that was clearly a reset is itself a
/// diagnosis: POR and brownout latch NO bit, so raw=0 in a boot loop
/// points at the supply.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ResetReasons {
    pub raw: u32,
    pub resetpin: bool,
    pub dog: bool,
    pub sreq: bool,
    pub lockup: bool,
    pub off: bool,
    pub lpcomp: bool,
    pub dif: bool,
    pub nfc: bool,
    pub vbus: bool,
}

impl ResetReasons {
    pub fn decode(raw: u32) -> Self {
        ResetReasons {
            raw,
            resetpin: raw & 1 << 0 != 0,
            dog: raw & 1 << 1 != 0,
            sreq: raw & 1 << 2 != 0,
            lockup: raw & 1 << 3 != 0,
            off: raw & 1 << 16 != 0,
            lpcomp: raw & 1 << 17 != 0,
            dif: raw & 1 << 18 != 0,
            nfc: raw & 1 << 19 != 0,
            vbus: raw & 1 << 20 != 0,
        }
    }
}

/// Body of the `[RESET_REASON]` line both bins emit at boot. The shape
/// predates this crate (captures grep it); only the decode moved here so
/// it could be host-asserted.
impl fmt::Display for ResetReasons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "raw=0x{:08x} resetpin={} dog={} sreq={} lockup={} off={} lpcomp={} dif={} nfc={} vbus={}",
            self.raw,
            self.resetpin as u8,
            self.dog as u8,
            self.sreq as u8,
            self.lockup as u8,
            self.off as u8,
            self.lpcomp as u8,
            self.dif as u8,
            self.nfc as u8,
            self.vbus as u8,
        )
    }
}

/// The `BOOT_TRACE` line body (everything except the ` t=<ms>` stamp the
/// firmware log formatter appends to every line).
///
/// Key order is the contract from the bug ledger, not the alphabetical
/// std-side rule: `prev_magic`, `prev_phase`, `prev_boot`,
/// `reset_reason`. `prev_magic=absent` renders `prev_phase=absent
/// prev_boot=0` — stable keys for the grep, no fabricated phase. A valid
/// record with a phase byte this image does not assign renders
/// `prev_phase=unknown-0x<byte>`.
pub struct TraceLine {
    pub prev: PrevBoot,
    pub reset_reason: u32,
}

impl fmt::Display for TraceLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.prev {
            PrevBoot::Absent => {
                write!(
                    f,
                    "BOOT_TRACE prev_magic=absent prev_phase=absent prev_boot=0"
                )?;
            }
            PrevBoot::Present { boot, phase_byte } => {
                write!(f, "BOOT_TRACE prev_magic=ok prev_phase=")?;
                match Phase::from_byte(phase_byte) {
                    Some(phase) => write!(f, "{}", phase.name())?,
                    None => write!(f, "unknown-0x{phase_byte:02x}")?,
                }
                write!(f, " prev_boot={boot}")?;
            }
        }
        write!(f, " reset_reason=0x{:08x}", self.reset_reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_the_cross_boot_contract() {
        // Any change here invalidates records in the field: bump MAGIC.
        assert_eq!(core::mem::offset_of!(RawRecord, magic), 0);
        assert_eq!(core::mem::offset_of!(RawRecord, boot), 4);
        assert_eq!(core::mem::offset_of!(RawRecord, phase), 8);
        assert_eq!(core::mem::size_of::<RawRecord>(), 12);
    }

    #[test]
    fn valid_magic_decodes_to_present() {
        let raw = RawRecord {
            magic: MAGIC,
            boot: 17,
            phase: Phase::MainLoop as u8,
        };
        assert_eq!(
            decode(&raw),
            PrevBoot::Present {
                boot: 17,
                phase_byte: 8
            }
        );
    }

    #[test]
    fn wrong_magic_is_absent_even_with_plausible_body() {
        let raw = RawRecord {
            magic: MAGIC ^ 1,
            boot: 17,
            phase: Phase::MainLoop as u8,
        };
        assert_eq!(decode(&raw), PrevBoot::Absent);
    }

    #[test]
    fn boot_counter_bumps_and_restarts() {
        assert_eq!(next_boot(&PrevBoot::Absent), 1);
        assert_eq!(
            next_boot(&PrevBoot::Present {
                boot: 17,
                phase_byte: 8
            }),
            18
        );
        // Wrap instead of panic: a counter that survives long enough to
        // wrap must not take the board down doing it.
        assert_eq!(
            next_boot(&PrevBoot::Present {
                boot: u32::MAX,
                phase_byte: 8
            }),
            0
        );
    }

    #[test]
    fn phase_names_round_trip() {
        let all = [
            (Phase::EnterMain, "enter-main"),
            (Phase::PersistRead, "persist-read"),
            (Phase::UsbUp, "usb-up"),
            (Phase::LoraTask, "lora-task"),
            (Phase::LoraSkipped, "lora-skipped"),
            (Phase::SdEnabled, "sd-enabled"),
            (Phase::BleTask, "ble-task"),
            (Phase::MainLoop, "main-loop"),
        ];
        for (phase, name) in all {
            assert_eq!(phase.name(), name);
            assert_eq!(Phase::from_byte(phase as u8), Some(phase));
        }
        assert_eq!(Phase::from_byte(0), None);
        assert_eq!(Phase::from_byte(9), None);
        assert_eq!(Phase::from_byte(0xCE), None);
    }

    #[test]
    fn resetreas_decode_fixtures() {
        // One bit at a time, positions from the product spec.
        assert!(ResetReasons::decode(0x0000_0001).resetpin);
        assert!(ResetReasons::decode(0x0000_0002).dog);
        assert!(ResetReasons::decode(0x0000_0004).sreq);
        assert!(ResetReasons::decode(0x0000_0008).lockup);
        assert!(ResetReasons::decode(0x0001_0000).off);
        assert!(ResetReasons::decode(0x0002_0000).lpcomp);
        assert!(ResetReasons::decode(0x0004_0000).dif);
        assert!(ResetReasons::decode(0x0008_0000).nfc);
        assert!(ResetReasons::decode(0x0010_0000).vbus);

        // POR/brownout latch no bit at all.
        let por = ResetReasons::decode(0);
        assert_eq!(
            por,
            ResetReasons {
                raw: 0,
                resetpin: false,
                dog: false,
                sreq: false,
                lockup: false,
                off: false,
                lpcomp: false,
                dif: false,
                nfc: false,
                vbus: false,
            }
        );

        // Combined: button press while VBUS latched.
        let combo = ResetReasons::decode(0x0010_0001);
        assert!(combo.resetpin && combo.vbus && !combo.sreq);
    }

    #[test]
    fn reset_reason_line_body_is_byte_exact() {
        assert_eq!(
            ResetReasons::decode(0x0000_0004).to_string(),
            "raw=0x00000004 resetpin=0 dog=0 sreq=1 lockup=0 off=0 lpcomp=0 dif=0 nfc=0 vbus=0"
        );
        // The two adjacent high-word bits, each alone: a swapped pair in
        // the format arguments cannot pass both.
        assert_eq!(
            ResetReasons::decode(0x0002_0000).to_string(),
            "raw=0x00020000 resetpin=0 dog=0 sreq=0 lockup=0 off=0 lpcomp=1 dif=0 nfc=0 vbus=0"
        );
        assert_eq!(
            ResetReasons::decode(0x0004_0000).to_string(),
            "raw=0x00040000 resetpin=0 dog=0 sreq=0 lockup=0 off=0 lpcomp=0 dif=1 nfc=0 vbus=0"
        );
    }

    /// The line a healthy reboot prints: the previous boot reached the
    /// main loop, the reset was a software request (commanded reset).
    #[test]
    fn trace_line_healthy_boot() {
        let line = TraceLine {
            prev: PrevBoot::Present {
                boot: 17,
                phase_byte: Phase::MainLoop as u8,
            },
            reset_reason: 0x0000_0004,
        };
        assert_eq!(
            line.to_string(),
            "BOOT_TRACE prev_magic=ok prev_phase=main-loop prev_boot=17 reset_reason=0x00000004"
        );
    }

    /// The line the button-press recovery prints after a boot that hung
    /// between `Softdevice::enable` returning and the SD task spawn.
    #[test]
    fn trace_line_hang_at_phase() {
        let line = TraceLine {
            prev: PrevBoot::Present {
                boot: 18,
                phase_byte: Phase::SdEnabled as u8,
            },
            reset_reason: 0x0000_0001,
        };
        assert_eq!(
            line.to_string(),
            "BOOT_TRACE prev_magic=ok prev_phase=sd-enabled prev_boot=18 reset_reason=0x00000001"
        );
    }

    /// First boot after power loss or a layout-version flash.
    #[test]
    fn trace_line_absent() {
        let line = TraceLine {
            prev: PrevBoot::Absent,
            reset_reason: 0x0010_0000,
        };
        assert_eq!(
            line.to_string(),
            "BOOT_TRACE prev_magic=absent prev_phase=absent prev_boot=0 reset_reason=0x00100000"
        );
    }

    /// A record from a same-magic image with a phase byte this image
    /// does not assign: visible, not fabricated.
    #[test]
    fn trace_line_unknown_phase_byte() {
        let line = TraceLine {
            prev: PrevBoot::Present {
                boot: 3,
                phase_byte: 0x2a,
            },
            reset_reason: 0,
        };
        assert_eq!(
            line.to_string(),
            "BOOT_TRACE prev_magic=ok prev_phase=unknown-0x2a prev_boot=3 reset_reason=0x00000000"
        );
    }
}
