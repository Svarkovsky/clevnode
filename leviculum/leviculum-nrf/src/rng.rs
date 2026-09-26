//! Hardware RNG access for NodeCore.
//!
//! The Nordic S140 SoftDevice owns the RNG peripheral exclusively at
//! runtime (PREGION-protected); any direct register access at 0x4000_D000
//! traps as `NRF_FAULT_ID_APP_MEMACC` and panics via
//! `nrf_softdevice::softdevice::fault_handler`. Post-`Softdevice::enable`,
//! we MUST use `sd_rand_application_vector_get`. **Pre**-enable, the
//! peripheral is unprotected and direct register access works — and we
//! need it to, because identity generation on a fresh device runs in
//! `NodeCoreBuilder::build` which is called before `ble::init`.
//!
//! Strategy: call the SD syscall first. If it returns success, use it.
//! On any error, `leviculum_sd_policy::rand_error_action` decides from
//! (error code, `sd_softdevice_is_enabled`) what is legal: direct
//! register access only while the SD is off; while it is on, a drained
//! pool (`NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES`) means polling the
//! available-byte count and consuming bytes as the pool refills
//! (~120 µs/byte — a 32-byte link keygen takes single-digit
//! milliseconds). Never, under any error, direct register access while
//! the SD is enabled (Codeberg #250).
//!
//! Bug #32 spike: the previous unconditional direct-register-access
//! version cycled the device every ~26 s with a SoftDevice MEMACC panic.

use leviculum_sd_policy::{rand_error_action, RandAction};
use rand_core::{CryptoRng, RngCore};

/// Hardware RNG that prefers the SoftDevice's CSPRNG syscall, with a
/// pre-enable fallback to direct register access.
pub struct RawHwRng;

const RNG_BASE: u32 = 0x4000_D000;
// Register table mirrors the nRF52840 reference manual offsets; the
// explicit `+ 0x000` keeps the offset column symmetric and greppable.
#[allow(clippy::identity_op)]
const RNG_TASKS_START: *mut u32 = (RNG_BASE + 0x000) as *mut u32;
const RNG_TASKS_STOP: *mut u32 = (RNG_BASE + 0x004) as *mut u32;
const RNG_EVENTS_VALRDY: *mut u32 = (RNG_BASE + 0x100) as *mut u32;
const RNG_VALUE: *const u32 = (RNG_BASE + 0x508) as *const u32;
const RNG_CONFIG: *mut u32 = (RNG_BASE + 0x504) as *mut u32;

impl Default for RawHwRng {
    fn default() -> Self {
        Self::new()
    }
}

impl RawHwRng {
    pub fn new() -> Self {
        Self
    }

    /// Direct hardware register read — safe ONLY pre-`Softdevice::enable`.
    /// Used as the fallback when the SD syscall returns INVALID_STATE
    /// because identity generation happens in `NodeCoreBuilder::build`
    /// before `ble::init` runs. Post-enable, this would fault the SD.
    fn direct_byte() -> u8 {
        unsafe {
            // Enable bias correction once, idempotent.
            core::ptr::write_volatile(RNG_CONFIG, 1);
            core::ptr::write_volatile(RNG_EVENTS_VALRDY, 0);
            core::ptr::write_volatile(RNG_TASKS_START, 1);
            while core::ptr::read_volatile(RNG_EVENTS_VALRDY) == 0 {}
            let val = core::ptr::read_volatile(RNG_VALUE) as u8;
            core::ptr::write_volatile(RNG_TASKS_STOP, 1);
            val
        }
    }

    /// Runtime SoftDevice state, straight from the SoftDevice itself.
    /// `sd_softdevice_is_enabled` is one of the few calls that is legal
    /// in every state and documented to always return NRF_SUCCESS.
    fn sd_enabled() -> bool {
        let mut enabled: u8 = 0;
        unsafe { nrf_softdevice_s140::sd_softdevice_is_enabled(&mut enabled) };
        enabled != 0
    }

    /// Pool-drain path (`RandAction::Wait`): poll the available-byte
    /// count and consume whatever is there, returning the number of
    /// bytes written into `dest` (≥ 1). The pool refills at ~120 µs/
    /// byte, so a 32-byte request completes in single-digit
    /// milliseconds of these round trips.
    ///
    /// This is a busy-wait: `fill` is reached through the sync
    /// `RngCore` trait (NodeCore owns the RNG), so yielding to the
    /// executor is not an option here. The wait is bounded only by a
    /// diagnostic: after `STALL_LOG_SPINS` polls without a single byte
    /// appearing, log once and keep polling — returning weak randomness
    /// is not acceptable, and the direct register path would
    /// MEMACC-fault the SoftDevice. A permanently dry pool means the
    /// SoftDevice itself is broken; the log makes that stall
    /// attributable.
    fn wait_consume(dest: &mut [u8]) -> usize {
        // ~10^6 iterations, each at least a syscall round trip — a
        // deliberately generous multiple of the ~120 µs/byte refill
        // interval, so the log can only mean "the pool is not refilling
        // at all", never "the pool is merely slow".
        const STALL_LOG_SPINS: u32 = 1_000_000;
        let mut spins: u32 = 0;
        loop {
            let mut avail: u8 = 0;
            unsafe { nrf_softdevice_s140::sd_rand_application_bytes_available_get(&mut avail) };
            if avail > 0 {
                let take = (avail as usize).min(dest.len());
                let ret = unsafe {
                    nrf_softdevice_s140::sd_rand_application_vector_get(
                        dest.as_mut_ptr(),
                        take as u8,
                    )
                };
                if ret == 0 {
                    return take;
                }
                // Lost a race for the just-counted bytes (or another
                // transient error): fall through and poll again.
            }
            spins = spins.saturating_add(1);
            if spins == STALL_LOG_SPINS {
                crate::log::log_fmt(
                    "[RNG ] ",
                    format_args!("sd_rand pool not refilling, still waiting"),
                );
            }
            cortex_m::asm::nop();
        }
    }

    fn fill(dest: &mut [u8]) {
        if dest.is_empty() {
            return;
        }
        let mut offset = 0usize;
        while offset < dest.len() {
            let chunk_len = (dest.len() - offset).min(u8::MAX as usize);
            let ret = unsafe {
                nrf_softdevice_s140::sd_rand_application_vector_get(
                    dest[offset..].as_mut_ptr(),
                    chunk_len as u8,
                )
            };
            if ret == 0 {
                offset += chunk_len;
                continue;
            }
            match rand_error_action(ret, Self::sd_enabled()) {
                RandAction::Direct => {
                    // SD off — typically NRF_ERROR_INVALID_STATE during
                    // identity generation before `ble::init`. No PREGION
                    // protection yet, direct register access is legal.
                    for byte in &mut dest[offset..offset + chunk_len] {
                        *byte = Self::direct_byte();
                    }
                    offset += chunk_len;
                }
                RandAction::Wait => {
                    offset += Self::wait_consume(&mut dest[offset..]);
                }
                RandAction::WaitAndLog => {
                    crate::log::log_fmt(
                        "[RNG ] ",
                        format_args!("unexpected sd_rand error {}, waiting on pool", ret),
                    );
                    offset += Self::wait_consume(&mut dest[offset..]);
                }
            }
        }
    }
}

impl RngCore for RawHwRng {
    fn next_u32(&mut self) -> u32 {
        let mut buf = [0u8; 4];
        Self::fill(&mut buf);
        u32::from_le_bytes(buf)
    }

    fn next_u64(&mut self) -> u64 {
        let mut buf = [0u8; 8];
        Self::fill(&mut buf);
        u64::from_le_bytes(buf)
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        Self::fill(dest);
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        Self::fill(dest);
        Ok(())
    }
}

// The SoftDevice's CSPRNG is seeded from the hardware TRNG with bias correction.
impl CryptoRng for RawHwRng {}
