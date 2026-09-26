//! Pure policy decisions for peripherals the S140 SoftDevice owns.
//!
//! Post-`Softdevice::enable`, RNG/POWER/RADIO/… are PREGION-protected:
//! any direct register access traps as `NRF_FAULT_ID_APP_MEMACC` and
//! panics the whole node (Codeberg #249/#250). The firmware therefore
//! has to decide, per failed syscall, whether the legacy direct-register
//! path is legal or whether it must wait for the SoftDevice. That
//! decision is pure (error code + SD-enabled flag → action), so it lives
//! here where the host can unit-test the full table, next to the
//! `leviculum-screen` painter that follows the same pattern.

#![cfg_attr(not(test), no_std)]

/// `NRF_ERROR_INVALID_STATE`: the SoftDevice is not enabled, the syscall
/// cannot serve the request at all.
pub const NRF_ERROR_INVALID_STATE: u32 = 8;

/// `NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES`: the SoftDevice is running but
/// its CSPRNG pool currently holds fewer bytes than requested. The pool
/// refills at roughly 120 µs/byte; this is a transient condition, not a
/// failure.
pub const NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES: u32 = 8199;

/// What the caller must do after `sd_rand_application_vector_get`
/// returned a non-zero error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandAction {
    /// Direct RNG register access. Legal ONLY while the SoftDevice is
    /// disabled (no PREGION protection yet).
    Direct,
    /// Poll `sd_rand_application_bytes_available_get` and consume bytes
    /// as the pool refills. Never touch registers.
    Wait,
    /// Same as [`RandAction::Wait`], but the error code was not one the
    /// table anticipated — emit a debug log so the stall is
    /// diagnosable. Still never touch registers.
    WaitAndLog,
}

/// Decide how to make progress after an RNG syscall error.
///
/// The invariant this function exists to enforce: **while the SoftDevice
/// is enabled, no input maps to [`RandAction::Direct`]**. A MEMACC fault
/// reboots the node and (via GPREGRET-adjacent postmortem machinery)
/// records a bogus panic; waiting merely stalls one task.
pub fn rand_error_action(error_code: u32, sd_enabled: bool) -> RandAction {
    if !sd_enabled {
        // No PREGION protection: the direct register path is legal and
        // is the only one that can make progress (the syscall keeps
        // failing while the SoftDevice is off).
        return RandAction::Direct;
    }
    match error_code {
        NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES => RandAction::Wait,
        _ => RandAction::WaitAndLog,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four-row table from Codeberg #250.
    #[test]
    fn invalid_state_sd_off_goes_direct() {
        assert_eq!(
            rand_error_action(NRF_ERROR_INVALID_STATE, false),
            RandAction::Direct
        );
    }

    #[test]
    fn not_enough_values_sd_on_waits() {
        assert_eq!(
            rand_error_action(NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES, true),
            RandAction::Wait
        );
    }

    #[test]
    fn not_enough_values_sd_off_goes_direct() {
        assert_eq!(
            rand_error_action(NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES, false),
            RandAction::Direct
        );
    }

    #[test]
    fn unknown_error_sd_on_waits_and_logs() {
        assert_eq!(rand_error_action(0xDEAD, true), RandAction::WaitAndLog);
    }

    /// Beyond the table: any error with the SoftDevice off resolves to
    /// the direct path (it is legal there, and the syscall cannot make
    /// progress), including codes the table never anticipated.
    #[test]
    fn unknown_error_sd_off_goes_direct() {
        assert_eq!(rand_error_action(0xDEAD, false), RandAction::Direct);
    }

    /// Contradictory input (INVALID_STATE while the SD claims to be
    /// enabled) must still never touch registers.
    #[test]
    fn invalid_state_sd_on_never_direct() {
        assert_eq!(
            rand_error_action(NRF_ERROR_INVALID_STATE, true),
            RandAction::WaitAndLog
        );
    }

    /// The enforced invariant, exhaustively: sd_enabled == true never
    /// yields Direct, for any error code shape.
    #[test]
    fn sd_on_never_direct_for_any_code() {
        for code in [
            0u32,
            1,
            NRF_ERROR_INVALID_STATE,
            NRF_ERROR_SOC_RAND_NOT_ENOUGH_VALUES,
            8192,
            u32::MAX,
        ] {
            assert_ne!(rand_error_action(code, true), RandAction::Direct);
        }
    }
}
