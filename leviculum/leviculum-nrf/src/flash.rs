//! Identity persistence via internal flash (NVMC) for nRF52840.
//!
//! Implements [`IdentityStore`] using the nRF52840's internal flash.
//! The wire format (magic, version, checksum) is defined in
//! `leviculum_core::identity_store` and shared across all targets.

use embassy_nrf::nvmc::Nvmc;
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use leviculum_core::identity::Identity;
use leviculum_core::identity_store::{self, IdentityStore, ENCODED_SIZE_ALIGNED};

/// NVMC-backed identity store for the T114.
///
/// The flash page address is supplied by the bin file (typically from
/// `BoardConfig::identity_flash_page`). On T114 it is 0xEC000, just below
/// Heltec's reserved area (0xED000); on RAK4631 the same address falls
/// in unused application flash.
pub struct NvmcIdentityStore<'d> {
    nvmc: Nvmc<'d>,
    page: u32,
}

impl<'d> NvmcIdentityStore<'d> {
    pub fn new(nvmc: Nvmc<'d>, page: u32) -> Self {
        Self { nvmc, page }
    }
}

impl IdentityStore for NvmcIdentityStore<'_> {
    type Error = embassy_nrf::nvmc::Error;

    fn load(&mut self) -> Result<Option<Identity>, Self::Error> {
        let mut buf = [0u8; ENCODED_SIZE_ALIGNED];
        self.nvmc.read(self.page, &mut buf)?;
        Ok(identity_store::decode_identity(&buf))
    }

    fn save(&mut self, identity: &Identity) -> Result<(), Self::Error> {
        let buf = match identity_store::encode_identity(identity) {
            Some(b) => b,
            None => return Ok(()),
        };
        self.nvmc.erase(self.page, self.page + 4096)?;
        self.nvmc.write(self.page, &buf)?;
        Ok(())
    }
}

/// The SoftDevice's flash handle, shared between the persistence tasks.
///
/// `nrf_softdevice::Flash::take` is a singleton and panics on a second
/// call, but more than one thing needs to be persisted while the
/// SoftDevice is live (the radio profile, the telemetry target, whatever
/// comes next). One `take`, one mutex, one handle every store task
/// borrows for the duration of its own erase-then-write.
///
/// A mutex rather than one combined task, because the two stores have
/// nothing to do with each other: coupling them would mean a radio-config
/// save could be delayed behind a telemetry save's retry loop for no
/// reason a reader of either module could see.
#[cfg(feature = "softdevice")]
pub type SharedFlash = embassy_sync::mutex::Mutex<
    embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
    nrf_softdevice::Flash,
>;

/// Take the SoftDevice flash handle and wrap it for sharing.
///
/// Call **once**, from the binary, after `Softdevice::enable`, and pass
/// the returned reference to every store task. The one-shot contract is
/// enforced twice over — `StaticCell::init` and `Flash::take` both panic
/// on a second call — which is the right failure for a boot-time wiring
/// mistake: loud, immediate, and impossible to reach in the field on a
/// board that booted once.
#[cfg(feature = "softdevice")]
pub fn shared_flash(sd: &'static nrf_softdevice::Softdevice) -> &'static SharedFlash {
    static CELL: static_cell::StaticCell<SharedFlash> = static_cell::StaticCell::new();
    CELL.init(SharedFlash::new(nrf_softdevice::Flash::take(sd)))
}
