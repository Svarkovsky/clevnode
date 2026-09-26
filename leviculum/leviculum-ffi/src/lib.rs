//! C API for Leviculum.
//!
//! A thin, Unix-idiomatic C surface over the curated `leviculum_std::api`
//! facade. Conventions:
//!
//! - Every symbol is prefixed `lev_` (functions) or `LEV_` (constants).
//! - Complex objects are opaque handles with a `_free` counterpart.
//! - Functions return `int`: `0` success, negative `LEV_ERR_*` on failure.
//! - Output buffers are caller-owned, read(2) style (`buf` + `cap` + `out_len`).
//! - No panic ever crosses the boundary; every function runs under `guard`.
//!
//! The design of record is `docs/leviculum-api-design.md`.

#![allow(non_camel_case_types)]
#![allow(clippy::missing_safety_doc)]
#![warn(unreachable_pub)]

use std::os::raw::{c_char, c_int};
use std::panic::AssertUnwindSafe;
use std::sync::Once;

mod destination;
// mod diagnostics;
mod error;
mod events;
mod identity;
// mod link;
mod log;
mod node;
mod request;
mod resource;

pub use destination::*;
// pub use diagnostics::*;
pub use error::*;
pub use events::*;
pub use identity::*;
// pub use link::*;
pub use log::*;
pub use node::*;
pub use request::*;
pub use resource::*;

/// Runs process-global setup exactly once.
static INIT: Once = Once::new();

/// Ensure one-time process setup has run: install the logging subscriber and
/// panic hook. Idempotent and thread-safe; the lazy path taken by other entry
/// points goes through the same `Once` as the explicit `lev_init`.
pub(crate) fn ensure_init() {
    INIT.call_once(log::install);
}

/// Perform one-time process setup (logging subscriber, panic hook).
///
/// Idempotent and safe to call from multiple threads. Optional: other entry
/// points run it lazily, but call it explicitly to configure logging before
/// the first node is built.
#[no_mangle]
pub extern "C" fn lev_init() -> c_int {
    guard(LEV_ERR_PANIC, || {
        ensure_init();
        LEV_OK
    })
}

/// Run an FFI body under `catch_unwind`, converting a panic into `default`.
///
/// Unwinding into C is undefined behaviour, so every `extern "C"` function
/// wraps its body in this guard. `default` is `LEV_ERR_PANIC` for the int
/// returning functions and a null pointer for constructors.
pub(crate) fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            // Allocation-free: a panic handler must not do work that could
            // itself panic (e.g. allocate after an allocation failure).
            error::set_last_error_static(c"panic in libleviculum");
            default
        }
    }
}

/// Copy `src` into a caller-owned buffer, read(2) style.
///
/// Sets `*out_len` to `src.len()`. If `buf` is NULL or `cap` is too small,
/// writes nothing and returns `LEV_ERR_BUFFER_TOO_SMALL` (so a NULL buffer is a
/// valid size query). Returns `LEV_ERR_NULL_PTR` if `out_len` is NULL.
pub(crate) unsafe fn write_out(src: &[u8], buf: *mut u8, cap: usize, out_len: *mut usize) -> c_int {
    if out_len.is_null() {
        return LEV_ERR_NULL_PTR;
    }
    *out_len = src.len();
    if buf.is_null() || cap < src.len() {
        return LEV_ERR_BUFFER_TOO_SMALL;
    }
    std::ptr::copy_nonoverlapping(src.as_ptr(), buf, src.len());
    LEV_OK
}

/// Copy a fixed-size byte array out of a C pointer. The caller must ensure
/// `src` points to at least `N` readable bytes.
pub(crate) unsafe fn read_array<const N: usize>(src: *const u8) -> [u8; N] {
    let mut out = [0u8; N];
    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), N);
    out
}

/// Encode `data` as lowercase hex into `buf`, read(2) style. Writes `2 * len`
/// bytes (not NUL-terminated); `*out_len` is set to the required size.
#[no_mangle]
pub unsafe extern "C" fn lev_hex_encode(
    data: *const u8,
    len: usize,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        if out_len.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if data.is_null() && len > 0 {
            return LEV_ERR_NULL_PTR;
        }
        // `len * 2` would wrap in release for a huge `len`, yielding an
        // undersized `needed` and an out-of-bounds writer loop. Reject it before
        // any allocation, slice construction, or write.
        let needed = match len.checked_mul(2) {
            Some(n) => n,
            None => {
                error::set_last_error("hex length overflow");
                return LEV_ERR_INVALID_ARG;
            }
        };
        *out_len = needed;
        if buf.is_null() || cap < needed {
            return LEV_ERR_BUFFER_TOO_SMALL;
        }
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let src = if len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(data, len)
        };
        let out = std::slice::from_raw_parts_mut(buf, needed);
        for (i, &byte) in src.iter().enumerate() {
            out[i * 2] = HEX[(byte >> 4) as usize];
            out[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
        }
        LEV_OK
    })
}

/// Decode hex (`hex_len` bytes, even) into `buf`, read(2) style. Writes
/// `hex_len / 2` bytes; `*out_len` is set to the required size.
/// `LEV_ERR_INVALID_ARG` on an odd length or a non-hex digit.
#[no_mangle]
pub unsafe extern "C" fn lev_hex_decode(
    hex: *const u8,
    hex_len: usize,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        if out_len.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if hex.is_null() && hex_len > 0 {
            return LEV_ERR_NULL_PTR;
        }
        if !hex_len.is_multiple_of(2) {
            error::set_last_error("hex length must be even");
            return LEV_ERR_INVALID_ARG;
        }
        // Division cannot overflow; the writer indexes `src[i*2]`/`src[i*2+1]`
        // for `i < needed`, so the max index is `hex_len - 1`, always in bounds.
        let needed = hex_len / 2;
        *out_len = needed;
        if buf.is_null() || cap < needed {
            return LEV_ERR_BUFFER_TOO_SMALL;
        }
        fn nibble(c: u8) -> Option<u8> {
            match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                b'A'..=b'F' => Some(c - b'A' + 10),
                _ => None,
            }
        }
        let src = std::slice::from_raw_parts(hex, hex_len);
        let out = std::slice::from_raw_parts_mut(buf, needed);
        for i in 0..needed {
            match (nibble(src[i * 2]), nibble(src[i * 2 + 1])) {
                (Some(hi), Some(lo)) => out[i] = (hi << 4) | lo,
                _ => {
                    error::set_last_error("invalid hex digit");
                    return LEV_ERR_INVALID_ARG;
                }
            }
        }
        LEV_OK
    })
}

/// Return the library version string, for example `"0.7.0"`.
///
/// Static storage, never freed.
#[no_mangle]
pub extern "C" fn lev_version_string() -> *const c_char {
    guard(std::ptr::null(), || {
        concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
    })
}

/// Return the library version as `(major << 16) | (minor << 8) | patch`.
#[no_mangle]
pub extern "C" fn lev_version_number() -> u32 {
    guard(0, || {
        let (major, minor, patch) = leviculum_std::api::version();
        ((major as u32) << 16) | ((minor as u32) << 8) | (patch as u32)
    })
}
