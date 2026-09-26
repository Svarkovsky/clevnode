//! The T114's switched peripheral rail (VEXT, P0.21) and its warmup.
//!
//! One GPIO, two consumers. VEXT feeds the ST7789 display chain *and*
//! the L76K GNSS receiver on the Heltec Mesh Node T114, so neither
//! driver can own it: whichever task grabbed the pin first would decide
//! whether the other one has power, and a board built without a panel
//! would have a mute GNSS for a reason nothing in the GNSS code could
//! explain. The reference puts it at board level for exactly this
//! reason — Meshtastic raises VEXT in `main.cpp:420-422` ("turn on the
//! display power"), not in its display driver, and the GPS happens to
//! live on the same rail.
//!
//! So the rail is raised once by the binary, before any task that needs
//! it is spawned, and the consumers wait for it with
//! [`wait_ready`]. The `Output` is leaked into a `StaticCell`
//! deliberately: dropping it would return the pin to its reset state and
//! cut power to both peripherals, so the rail stays up for the life of
//! the board.
//!
//! Warmup is the reference's too: `PERIPHERAL_WARMUP_MS 1000`
//! (`variants/nrf52840/heltec_mesh_node_t114/variant.h:167`), mirrored
//! by [`crate::boards::t114::VEXT_WARMUP_MS`]. [`wait_ready`] sleeps
//! only the part of it that is still outstanding, so a task spawned late
//! pays nothing for a rail that came up long ago.
//!
//! On a board with no such rail — the WisMesh Pocket V2, whose 3V3
//! peripheral switch is a different pin its binary raises itself —
//! [`raise`] is never called and [`wait_ready`] returns immediately.
//! That is what lets the shared GNSS driver await it unconditionally.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::Peri;
use embassy_time::{Instant, Timer};
use static_cell::StaticCell;

/// Holds the rail high for the life of the board. Never taken out.
static RAIL: StaticCell<Output<'static>> = StaticCell::new();

/// Monotonic millisecond at which the rail is warm. Zero means the rail
/// was never raised (a board without one): a real deadline is always at
/// least [`VEXT_WARMUP_MS`](crate::boards::t114::VEXT_WARMUP_MS) past
/// boot, so zero cannot collide with one. `u32` because thumbv7em has no
/// 64-bit atomics; `raise` runs within the first seconds of a boot, and
/// the saturating cast keeps the arithmetic honest anyway.
static WARM_AT_MS: AtomicU32 = AtomicU32::new(0);

/// Raise VEXT and start the warmup clock. Call once from the binary,
/// before spawning any task that reads a peripheral on the rail.
///
/// A second call is a no-op beyond the log line: the peripheral is moved
/// in, so the type system already makes it a single call per boot.
pub fn raise(pin: Peri<'static, crate::boards::t114::VextEnable>) {
    let out = Output::new(pin, Level::High, OutputDrive::Standard);
    let warm_at = Instant::now()
        .as_millis()
        .saturating_add(crate::boards::t114::VEXT_WARMUP_MS as u64);
    WARM_AT_MS.store(warm_at.min(u32::MAX as u64) as u32, Ordering::Release);
    if RAIL.try_init(out).is_none() {
        return;
    }
    crate::log::log_fmt(
        "[VEXT] ",
        format_args!(
            "rail high, {} ms warmup",
            crate::boards::t114::VEXT_WARMUP_MS
        ),
    );
}

/// Wait until the rail has been up for its warmup period. Returns
/// immediately on a board where [`raise`] was never called.
pub async fn wait_ready() {
    let warm_at = WARM_AT_MS.load(Ordering::Acquire);
    if warm_at == 0 {
        return;
    }
    // A deadline already past fires immediately, so a task spawned long
    // after the rail came up pays nothing.
    Timer::at(Instant::from_millis(warm_at as u64)).await;
}
