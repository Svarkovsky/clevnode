//! Persistence of the LoRa radio configuration in internal flash.
//!
//! A host sets the radio profile over the serial control channel; without
//! persistence a reset drops the board back to the compiled default and the
//! chosen frequency is gone. This module keeps one flash page
//! ([`BoardConfig::radio_config_flash_page`](crate::boards::BoardConfig::radio_config_flash_page),
//! `0xEB000` on both boards) holding the last applied configuration in the
//! envelope defined by [`leviculum_core::radio_config_store`].
//!
//! Two halves, because the two directions have very different constraints:
//!
//! * **Load** ([`load`]) runs at boot, long before the SoftDevice is
//!   enabled, and is a plain read of memory-mapped flash — no peripheral and
//!   no syscall involved, so it cannot collide with the NVMC instance the
//!   identity store owns.
//! * **Save** ([`request_save`] + [`store_task`]) runs while the SoftDevice
//!   is live. Direct NVMC access is then forbidden (the SoftDevice owns the
//!   flash timing; a page erase stalls the CPU for milliseconds and would
//!   break the BLE radio schedule), so the write goes through the
//!   SoftDevice's own `sd_flash_page_erase` / `sd_flash_write` via
//!   [`nrf_softdevice::Flash`]. The serial task never blocks on flash: it
//!   drops the wanted config into a one-slot channel and returns.
//!
//! Flash wear: `lnsd` re-sends the radio config on every connect, so the
//! store task reads the page back and compares the encoded bytes before
//! erasing anything. An unchanged config never writes.
//!
//! Erase-then-write is not atomic. Power loss in between leaves the page
//! erased, which decodes to `None` and boots the compiled default — the
//! same outcome as a board that was never configured, never a half-written
//! profile applied as if it were real (the checksum covers that case).

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use leviculum_core::radio_config_store::{
    decode_radio_config, encode_radio_config, ENCODED_SIZE_ALIGNED,
};
use leviculum_core::rnode::RadioConfigWire;

/// Pending save requests. Depth 1 and `try_send`: the serial task must never
/// block on a flash write, and a superseded request is worthless anyway —
/// the newest config is the one that must end up on the page.
static PENDING: Channel<CriticalSectionRawMutex, RadioConfigWire, 1> = Channel::new();

/// Read the persisted radio configuration, or `None` if the page is blank,
/// corrupt, or written by a different format version.
///
/// Internal flash is memory-mapped on the nRF52840, so this is an ordinary
/// read; the SoftDevice's own `ReadNorFlash` impl does exactly the same.
/// Safe at any point in boot, including before `Softdevice::enable`.
pub fn load(page: u32) -> Option<RadioConfigWire> {
    decode_radio_config(&read_page(page))
}

/// Copy the stored record out of memory-mapped flash.
fn read_page(page: u32) -> [u8; ENCODED_SIZE_ALIGNED] {
    let mut buf = [0u8; ENCODED_SIZE_ALIGNED];
    // SAFETY: `page` is a flash page address supplied by the board config,
    // outside the linker's FLASH region but inside the 1 MiB flash map
    // (memory.x). Flash is readable as normal memory on this part.
    let stored = unsafe { core::slice::from_raw_parts(page as *const u8, ENCODED_SIZE_ALIGNED) };
    buf.copy_from_slice(stored);
    buf
}

/// Ask the store task to persist `cfg`. Never blocks and never writes flash
/// on the caller's stack; if a request is already queued it is replaced by
/// this newer one.
pub fn request_save(cfg: &RadioConfigWire) {
    // Drop a stale queued request so the freshest config wins the slot.
    if PENDING.try_send(*cfg).is_err() {
        let _ = PENDING.try_receive();
        let _ = PENDING.try_send(*cfg);
    }
}

/// 4-byte-aligned page buffer. `sd_flash_write` writes whole 32-bit words
/// and rejects an unaligned source pointer.
#[repr(align(4))]
struct Aligned([u8; ENCODED_SIZE_ALIGNED]);

/// How often a failed flash operation is retried. The SoftDevice refuses
/// flash access while the radio is busy (`NRF_ERROR_BUSY`), which is a
/// transient condition on a node that is also forwarding packets.
const SAVE_RETRIES: u8 = 3;
/// Delay between retries.
const SAVE_RETRY_MS: u64 = 250;

#[cfg(feature = "softdevice")]
#[embassy_executor::task]
pub async fn store_task(flash: &'static crate::flash::SharedFlash, page: u32) {
    use embedded_storage_async::nor_flash::NorFlash;

    loop {
        let cfg = PENDING.receive().await;
        let encoded = Aligned(encode_radio_config(&cfg));

        // Read-compare-write: an unchanged page is never erased. lnsd sends
        // the same config on every connect, so this is the common case.
        if read_page(page) == encoded.0 {
            crate::log::log_fmt(
                "[RADIO] ",
                format_args!(
                    "persist skipped, unchanged freq={} sf={}",
                    cfg.frequency_hz, cfg.sf
                ),
            );
            continue;
        }

        let mut written = false;
        for attempt in 1..=SAVE_RETRIES {
            let result = async {
                let mut flash = flash.lock().await;
                flash.erase(page, page + 4096).await?;
                flash.write(page, &encoded.0).await
            }
            .await;
            match result {
                Ok(()) => {
                    written = true;
                    break;
                }
                Err(_) => {
                    crate::log::log_fmt(
                        "[RADIO] ",
                        format_args!("persist write failed, attempt {}/{}", attempt, SAVE_RETRIES),
                    );
                    embassy_time::Timer::after(embassy_time::Duration::from_millis(SAVE_RETRY_MS))
                        .await;
                }
            }
        }

        if written {
            crate::log::log_fmt(
                "[RADIO] ",
                format_args!(
                    "persist saved freq={} bw={} sf={} cr={} pwr={}",
                    cfg.frequency_hz, cfg.bandwidth_hz, cfg.sf, cfg.cr, cfg.tx_power_dbm
                ),
            );
        } else {
            crate::log::log_fmt("[RADIO] ", format_args!("persist gave up after retries"));
        }
    }
}

/// Spawn the store task on `spawner`, borrowing the shared SoftDevice
/// flash handle ([`crate::flash::shared_flash`]).
///
/// Call after `Softdevice::enable`. The handle is shared rather than
/// owned because the telemetry target (#236) persists to its own page
/// through its own task and there is only one `Flash` to go round.
#[cfg(feature = "softdevice")]
pub fn spawn_store_task(
    spawner: &embassy_executor::Spawner,
    flash: &'static crate::flash::SharedFlash,
    page: u32,
) {
    spawner.must_spawn(store_task(flash, page));
}
