//! The board-side media profile: which carriers this node meshes over,
//! and what a carrier that is down does with the packets handed to it.
//!
//! Extracted from `leviculum_nrf::media` because the firmware crate
//! cross-compiles and runs no host tests, and both halves of this file
//! have a failure mode that only shows up in a *sequence*:
//!
//! * [`MediaState`] — the running-versus-configured pair, and the boot
//!   window between "the profile has been read" and "the carriers'
//!   spawn decisions have been noted". A host frame answered inside that
//!   window used to read `configured=on running=off`, which the frame's
//!   own contract defines as "that carrier did not come up and cannot
//!   start before the next reset" (`TYPE_MEDIA_REPORT`). It was
//!   observable: run 4 of the BLE acceptance
//!   (`declared="lora=on ble=off" running="lora=off ble=off"`, #255).
//! * [`DropRun`] — a carrier that is switched off drops what the core
//!   hands it, and the drop used to be logged per packet. Every log line
//!   also goes into the 2 KiB post-crash tail, so ~45 dropped packets
//!   displaced the whole boot/fault diagnostic the tail exists for — in
//!   exactly the runs (single-carrier measurement) where a crash most
//!   needs explaining.
//!
//! Neither is arithmetic: both are state machines whose bug is in the
//! order of events, so both belong where a host test can drive that
//! order.

#![cfg_attr(not(test), no_std)]

use core::sync::atomic::{AtomicBool, Ordering};

/// Which carriers a statement is about. The wire type
/// (`leviculum_core::envelope::MediaProfileWire`) says the same thing;
/// this crate keeps its own so it stays dependency-free like its sibling
/// policy crates, and the firmware converts at the one boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Carriers {
    pub lora: bool,
    pub ble: bool,
}

impl Carriers {
    /// Both carriers on: the default profile, and the state every
    /// fielded board is in.
    pub const BOTH: Self = Self {
        lora: true,
        ble: true,
    };

    /// Neither carrier. Only ever a *measured* answer, never a prior —
    /// see [`MediaState::declare`].
    pub const NONE: Self = Self {
        lora: false,
        ble: false,
    };
}

/// The two profiles a board can be asked about, and the declaration gate
/// in front of them.
///
/// # The boot window
///
/// Boot runs in this order, and cannot run in any other: the profile is
/// read from flash *before* USB comes up (so no frame is ever answered
/// against an unread record), and the carriers are brought up *after*
/// USB (LoRa's init alone awaits several times over SPI). Between those
/// two points the serial task is already answering host frames while no
/// carrier has started yet.
///
/// [`declare`](Self::declare) therefore seeds the boot state with the
/// profile itself, so the window answers "running what you configured".
/// The alternative — starting the boot state at "nothing came up" —
/// makes the window's answer indistinguishable from the one terminal
/// state this pair exists to express, and `lnflash` prints "configured
/// on but not running … Reset the board" for it. The seeded prior can
/// only ever be *narrowed* by [`note_boot_state`](Self::note_boot_state),
/// which is the measured value and always wins; a carrier that turns out
/// not to have started is corrected within the same boot, milliseconds
/// later, and its terminal claim is then true.
#[derive(Debug)]
pub struct MediaState {
    wired: AtomicBool,
    configured_lora: AtomicBool,
    configured_ble: AtomicBool,
    booted_lora: AtomicBool,
    booted_ble: AtomicBool,
}

impl Default for MediaState {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaState {
    /// Undeclared: [`wired`](Self::wired) is false, so every media frame
    /// is refused as unsupported and the other fields are not yet
    /// anybody's answer.
    pub const fn new() -> Self {
        Self {
            wired: AtomicBool::new(false),
            configured_lora: AtomicBool::new(true),
            configured_ble: AtomicBool::new(true),
            booted_lora: AtomicBool::new(false),
            booted_ble: AtomicBool::new(false),
        }
    }

    /// Declare the profile this boot honours, before USB and before the
    /// carriers. Seeds the boot state with the same value — see the type
    /// docs for why the honest prior is the profile and not "nothing".
    pub fn declare(&self, configured: Carriers) {
        self.configured_lora
            .store(configured.lora, Ordering::Relaxed);
        self.configured_ble.store(configured.ble, Ordering::Relaxed);
        self.booted_lora.store(configured.lora, Ordering::Relaxed);
        self.booted_ble.store(configured.ble, Ordering::Relaxed);
        // Last, so no reader can see the gate open on a half-written
        // profile.
        self.wired.store(true, Ordering::Release);
    }

    /// Record what this boot actually brought up, once both carriers'
    /// spawn decisions have been made. The measured value, and the one
    /// that makes "a carrier that did not come up cannot be started" a
    /// fact rather than a hope.
    pub fn note_boot_state(&self, started: Carriers) {
        self.booted_lora.store(started.lora, Ordering::Relaxed);
        self.booted_ble.store(started.ble, Ordering::Relaxed);
    }

    /// Whether [`declare`](Self::declare) has run — the capability the
    /// serial task's media answers are gated on.
    pub fn wired(&self) -> bool {
        self.wired.load(Ordering::Acquire)
    }

    /// Apply a profile a host sent. Takes effect at once where that is
    /// possible; switching a carrier on that never started does not
    /// change [`running`](Self::running), which is the whole point of the
    /// two values.
    pub fn set_configured(&self, configured: Carriers) {
        self.configured_lora
            .store(configured.lora, Ordering::Relaxed);
        self.configured_ble.store(configured.ble, Ordering::Relaxed);
    }

    /// What a reboot would come up with.
    pub fn configured(&self) -> Carriers {
        Carriers {
            lora: self.configured_lora.load(Ordering::Relaxed),
            ble: self.configured_ble.load(Ordering::Relaxed),
        }
    }

    /// What this boot is carrying traffic on: configured AND booted.
    pub fn running(&self) -> Carriers {
        Carriers {
            lora: self.lora_active(),
            ble: self.ble_active(),
        }
    }

    /// Whether LoRa is carrying Reticulum traffic right now.
    pub fn lora_active(&self) -> bool {
        self.booted_lora.load(Ordering::Relaxed) && self.configured_lora.load(Ordering::Relaxed)
    }

    /// Whether BLE is carrying Reticulum traffic right now.
    pub fn ble_active(&self) -> bool {
        self.booted_ble.load(Ordering::Relaxed) && self.configured_ble.load(Ordering::Relaxed)
    }
}

/// One interface's run of packets dropped because its carrier is off.
///
/// # Why this is not one line per packet
///
/// Every `log_fmt` call also writes the 2 KiB persistent tail that
/// survives a reset, which is where a post-crash reader finds the boot
/// and fault diagnostics. A ~45-byte drop line per packet means the tail
/// holds nothing else after ~45 drops, and a node with a carrier
/// switched off drops one per announce per interface — so the state this
/// feature *exists to produce* is the state that erases the crash
/// evidence.
///
/// The house pattern is `dispatch::settle` and `Reporter::note_state`:
/// log the transition, not the traffic. This adds the one thing a pure
/// transition would lose — how many packets the run swallowed — by
/// reporting again at each decade (the 1st, 10th, 100th, 1000th …), so
/// the count in the log is never more than a factor of ten below the
/// truth and the number of lines is logarithmic. The exact total is on
/// the resume line when the carrier comes back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DropRun {
    packets: u32,
    bytes: u32,
    /// The decade the next report is due at: 1, 10, 100 … Zero means the
    /// run is over and the next drop starts a new one.
    next_report: u32,
}

/// What a drop run has swallowed so far, for the line the caller logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Swallowed {
    pub packets: u32,
    pub bytes: u32,
}

impl DropRun {
    /// No run in progress.
    pub const fn new() -> Self {
        Self {
            packets: 0,
            bytes: 0,
            next_report: 0,
        }
    }

    /// Account for a packet the carrier could not take. `Some` on the
    /// first drop of a run and at every decade after it — that, and
    /// nothing else, is what the caller logs.
    pub fn dropped(&mut self, len: usize) -> Option<Swallowed> {
        if self.next_report == 0 {
            // A fresh run: report immediately, so a capture attached to a
            // board that just went single-carrier says so on the first
            // packet rather than on the tenth.
            self.packets = 0;
            self.bytes = 0;
            self.next_report = 1;
        }
        self.packets = self.packets.saturating_add(1);
        self.bytes = self.bytes.saturating_add(len as u32);
        if self.packets < self.next_report {
            return None;
        }
        // Saturating: a run long enough to overflow this stops reporting
        // rather than reporting every packet from then on.
        self.next_report = self.next_report.saturating_mul(10);
        Some(self.swallowed())
    }

    /// Account for a packet the carrier took. `Some` exactly once per
    /// run, carrying the run's exact totals — the carrier came back, and
    /// this is the only place the untruncated count is stated.
    pub fn resumed(&mut self) -> Option<Swallowed> {
        if self.next_report == 0 {
            return None;
        }
        let swallowed = self.swallowed();
        *self = Self::new();
        Some(swallowed)
    }

    fn swallowed(&self) -> Swallowed {
        Swallowed {
            packets: self.packets,
            bytes: self.bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LORA_ONLY: Carriers = Carriers {
        lora: true,
        ble: false,
    };

    #[test]
    fn nothing_is_declared_before_the_profile_is_read() {
        let state = MediaState::new();
        assert!(!state.wired());
    }

    /// The #255 defect: between `declare` (before USB) and
    /// `note_boot_state` (after both carriers) the serial task is already
    /// answering. The answer in that window must not be the terminal
    /// "configured on, not running" — that is `lnflash`'s "Reset the
    /// board", and periculum's media proof reads it as a carrier that
    /// failed to start.
    #[test]
    fn the_boot_window_does_not_report_a_configured_carrier_as_not_running() {
        let state = MediaState::new();
        state.declare(LORA_ONLY);

        // Inside the window: USB is up, no carrier has been spawned yet.
        assert!(state.wired());
        assert_eq!(state.configured(), LORA_ONLY);
        assert_eq!(state.running(), LORA_ONLY);
        assert!(state.lora_active());
        assert!(!state.ble_active());

        // And the window's answer is the one the boot settles on.
        state.note_boot_state(LORA_ONLY);
        assert_eq!(state.running(), LORA_ONLY);
    }

    #[test]
    fn the_window_answer_is_the_declared_profile_not_a_both_on_default() {
        let state = MediaState::new();
        state.declare(Carriers::NONE);
        assert_eq!(state.running(), Carriers::NONE);
        assert_eq!(state.configured(), Carriers::NONE);
    }

    /// The measured value always wins over the seeded prior: a carrier
    /// whose spawn was skipped is `configured` without being `running`,
    /// and that terminal claim is exactly what the frame is for.
    #[test]
    fn a_carrier_that_did_not_come_up_is_configured_but_not_running() {
        let state = MediaState::new();
        state.declare(Carriers::BOTH);
        state.note_boot_state(LORA_ONLY);
        assert_eq!(state.configured(), Carriers::BOTH);
        assert_eq!(state.running(), LORA_ONLY);
    }

    #[test]
    fn switching_a_carrier_off_at_runtime_takes_effect_at_once() {
        let state = MediaState::new();
        state.declare(Carriers::BOTH);
        state.note_boot_state(Carriers::BOTH);
        state.set_configured(LORA_ONLY);
        assert_eq!(state.running(), LORA_ONLY);
        assert_eq!(state.configured(), LORA_ONLY);
    }

    #[test]
    fn switching_a_carrier_back_on_only_runs_if_it_came_up_at_boot() {
        let booted = MediaState::new();
        booted.declare(Carriers::BOTH);
        booted.note_boot_state(Carriers::BOTH);
        booted.set_configured(LORA_ONLY);
        booted.set_configured(Carriers::BOTH);
        assert_eq!(booted.running(), Carriers::BOTH);

        let held_down = MediaState::new();
        held_down.declare(LORA_ONLY);
        held_down.note_boot_state(LORA_ONLY);
        held_down.set_configured(Carriers::BOTH);
        assert_eq!(held_down.configured(), Carriers::BOTH);
        assert_eq!(held_down.running(), LORA_ONLY);
    }

    /// The persistent tail holds ~45 of these lines. A thousand dropped
    /// packets must not be a thousand lines.
    #[test]
    fn a_thousand_drops_are_four_lines_not_a_thousand() {
        let mut run = DropRun::new();
        let reported: Vec<Swallowed> = (0..1000).filter_map(|_| run.dropped(45)).collect();
        assert_eq!(
            reported.len(),
            4,
            "expected the 1st, 10th, 100th and 1000th drop only, got {reported:?}"
        );
        assert_eq!(
            reported.iter().map(|s| s.packets).collect::<Vec<_>>(),
            vec![1, 10, 100, 1000]
        );
        assert_eq!(reported[3].bytes, 45 * 1000);
    }

    #[test]
    fn the_first_drop_of_a_run_is_always_reported() {
        let mut run = DropRun::new();
        assert_eq!(
            run.dropped(120),
            Some(Swallowed {
                packets: 1,
                bytes: 120
            })
        );
    }

    #[test]
    fn the_resume_line_carries_the_exact_count_the_decades_truncated() {
        let mut run = DropRun::new();
        for _ in 0..37 {
            run.dropped(10);
        }
        assert_eq!(
            run.resumed(),
            Some(Swallowed {
                packets: 37,
                bytes: 370
            })
        );
        // Once, not on every packet the carrier now takes.
        assert_eq!(run.resumed(), None);
    }

    #[test]
    fn a_carrier_that_never_dropped_anything_says_nothing_when_it_sends() {
        let mut run = DropRun::new();
        assert_eq!(run.resumed(), None);
    }

    #[test]
    fn a_second_run_reports_from_its_own_first_packet() {
        let mut run = DropRun::new();
        for _ in 0..15 {
            run.dropped(10);
        }
        run.resumed();
        assert_eq!(
            run.dropped(10),
            Some(Swallowed {
                packets: 1,
                bytes: 10
            })
        );
    }
}
