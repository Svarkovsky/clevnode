//! The node's identity hashes, published for the debug port and the
//! control envelope (lnprobe batch).
//!
//! A prober needs a hash before it can address this board: the node
//! identity, the `rnstransport.probe` responder destination, and the
//! LXMF delivery destination. All three exist inside the node the moment
//! the boot sequence has registered its destinations — this module only
//! publishes them, it computes nothing.
//!
//! Two consumers read the publication:
//!
//! * [`log_banner`], called once at publication and then from the BSPs'
//!   periodic `fw_build_banner` task, so the `[IDENTITY]` line reaches a
//!   reader attached at any time, not only inside the boot window. It
//!   uses `log_critical!` and therefore bypasses the
//!   `RUNTIME_DRAIN_OPEN` gate like the other boot-critical lines.
//! * [`report`], the serial task's answer to a
//!   [`TYPE_IDENTITY_QUERY`](leviculum_core::envelope::TYPE_IDENTITY_QUERY)
//!   — `None` before [`note_boot_identity`] has run, which the task
//!   turns into `REFUSE_BUSY` exactly like the node-name query's
//!   boot-order window.

use core::cell::Cell;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;

use leviculum_core::envelope::IdentityReportWire;

/// The published hashes. Same locking rationale as `name`'s statics: the
/// value is wider than an atomic and a reader must never see half of one
/// publication and half of another; uncontended on the single-core
/// cooperative executor.
static HASHES: Mutex<CriticalSectionRawMutex, Cell<Option<IdentityReportWire>>> =
    Mutex::new(Cell::new(None));

/// Lowercase hex of an optional 16-byte hash for the banner line;
/// `none` for a destination this boot did not register.
struct MaybeHex16(Option<[u8; 16]>);

impl core::fmt::Display for MaybeHex16 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.0 {
            None => f.write_str("none"),
            Some(bytes) => {
                for b in bytes {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
    }
}

/// Publish the boot's hashes and emit the `[IDENTITY]` banner line once.
///
/// Call after the boot sequence has registered its destinations (the
/// LXMF delivery destination is the last of the three to exist).
pub fn note_boot_identity(identity: [u8; 16], probe: Option<[u8; 16]>, lxmf: Option<[u8; 16]>) {
    HASHES.lock(|cell| {
        cell.set(Some(IdentityReportWire {
            identity,
            probe,
            lxmf,
        }))
    });
    log_banner();
}

/// Emit the `[IDENTITY]` line. Silent before [`note_boot_identity`] —
/// a banner of zeroes would send an operator probing a destination that
/// does not exist.
pub fn log_banner() {
    if let Some(report) = HASHES.lock(|cell| cell.get()) {
        crate::log_critical!(
            "[IDENTITY] identity={} probe={} lxmf={}",
            MaybeHex16(Some(report.identity)),
            MaybeHex16(report.probe),
            MaybeHex16(report.lxmf)
        );
    }
}

/// The published hashes for the serial task's identity-query answer;
/// `None` is the boot-order window before [`note_boot_identity`].
pub fn report() -> Option<IdentityReportWire> {
    HASHES.lock(|cell| cell.get())
}
