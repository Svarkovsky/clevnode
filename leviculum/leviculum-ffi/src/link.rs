//! Path discovery, link establishment, and link data transfer.
//!
//! `lev_link_t` wraps a `leviculum_std::api::LinkHandle` plus a runtime handle
//! so sends can be driven from the calling C thread. Link data arrives as
//! `LEV_EVENT_LINK_DATA` events. See `docs/leviculum-api-design.md` §10.

use std::os::raw::c_int;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use leviculum_std::api::{DestinationHash, LinkHandle, LinkId};
use leviculum_std::{Error, SendError};

use crate::error::*;
use crate::identity::lev_identity_t;
use crate::node::{block_on_timeout, leviculum_t};
use crate::{guard, read_array, write_out, LEV_ADDR_LEN, LEV_SIGNING_KEY_LEN};

/// Opaque link handle.
pub struct lev_link_t {
    inner: LinkHandle,
    rt: tokio::runtime::Handle,
}

/// Map a link send error: the retryable cases become `LEV_ERR_AGAIN`.
fn map_link_send_err(e: &Error) -> c_int {
    match e {
        Error::Send(SendError::Busy) | Error::Send(SendError::PacingDelay { .. }) => LEV_ERR_AGAIN,
        other => map_error(other),
    }
}

/// Borrow `(data, len)` as a slice, treating a NULL pointer with length 0 as
/// empty. Returns `None` (a NULL-pointer error) for NULL with a non-zero length.
unsafe fn data_slice<'a>(data: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        Some(&[])
    } else if data.is_null() {
        None
    } else {
        Some(std::slice::from_raw_parts(data, len))
    }
}

// --- Path discovery ---

/// Return 1 if a path to the destination (16-byte hash) is known, else 0
/// (negative on a NULL argument).
#[no_mangle]
pub unsafe extern "C" fn lev_has_path(node: *const leviculum_t, dest_hash: *const u8) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if dest_hash.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let dh = DestinationHash::new(read_array::<LEV_ADDR_LEN>(dest_hash));
        i32::from(h.node().has_path(&dh))
    })
}

/// Write the hop count to a destination into `*out`. `LEV_ERR_NO_PATH` if no
/// path is known.
#[no_mangle]
pub unsafe extern "C" fn lev_hops_to(
    node: *const leviculum_t,
    dest_hash: *const u8,
    out: *mut u8,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if dest_hash.is_null() || out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let dh = DestinationHash::new(read_array::<LEV_ADDR_LEN>(dest_hash));
        match h.node().hops_to(&dh) {
            Some(hops) => {
                *out = hops;
                LEV_OK
            }
            None => LEV_ERR_NO_PATH,
        }
    })
}

/// Request a path to a destination. The result arrives as an event and
/// `lev_has_path` then returns 1. Blocks up to `timeout_ms`.
#[no_mangle]
pub unsafe extern "C" fn lev_request_path(
    node: *const leviculum_t,
    dest_hash: *const u8,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if dest_hash.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let dh = DestinationHash::new(read_array::<LEV_ADDR_LEN>(dest_hash));
        match block_on_timeout(h.runtime(), h.node().request_path(&dh), timeout_ms) {
            Ok(Ok(())) => LEV_OK,
            Ok(Err(e)) => map_error(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}

// --- Link establishment ---

/// Finish a connect: open the link with `signing_key` and box the handle.
unsafe fn finish_connect(
    h: &leviculum_t,
    dh: &DestinationHash,
    signing_key: &[u8; 32],
    timeout_ms: c_int,
    out: *mut *mut lev_link_t,
) -> c_int {
    let rt = h.runtime().handle().clone();
    let fut = h.node().connect_with_key(dh, signing_key);
    match block_on_timeout(h.runtime(), fut, timeout_ms) {
        Ok(Ok(link)) => {
            *out = Box::into_raw(Box::new(lev_link_t { inner: link, rt }));
            LEV_OK
        }
        Ok(Err(e)) => map_error(&e),
        Err(()) => LEV_ERR_TIMEOUT,
    }
}

/// Open a link to a destination (16-byte hash), resolving its signing key from
/// the identity cached from an announce. On success `*out` is the link handle.
///
/// `LEV_ERR_UNKNOWN_DEST` if no identity is cached, `LEV_ERR_NO_PATH` if no path
/// is known (request one first; connect does not auto-request).
#[no_mangle]
pub unsafe extern "C" fn lev_connect(
    node: *const leviculum_t,
    dest_hash: *const u8,
    timeout_ms: c_int,
    out: *mut *mut lev_link_t,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if dest_hash.is_null() || out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        *out = std::ptr::null_mut();
        let dh = DestinationHash::new(read_array::<LEV_ADDR_LEN>(dest_hash));
        let identity = match h.node().get_identity(&dh) {
            Some(id) => id,
            None => {
                set_last_error("no cached identity for destination");
                return LEV_ERR_UNKNOWN_DEST;
            }
        };
        if !h.node().has_path(&dh) {
            set_last_error("no path to destination");
            return LEV_ERR_NO_PATH;
        }
        // The signing key is the Ed25519 half, bytes 32..64 of the public key.
        let pubkey = identity.public_key_bytes();
        let mut signing_key = [0u8; 32];
        signing_key.copy_from_slice(&pubkey[32..64]);
        finish_connect(h, &dh, &signing_key, timeout_ms, out)
    })
}

/// Open a link with an explicit Ed25519 signing key (32 bytes), for out-of-band
/// keys where no announce was seen. `LEV_ERR_NO_PATH` if no path is known.
#[no_mangle]
pub unsafe extern "C" fn lev_connect_with_key(
    node: *const leviculum_t,
    dest_hash: *const u8,
    signing_key: *const u8,
    timeout_ms: c_int,
    out: *mut *mut lev_link_t,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if dest_hash.is_null() || signing_key.is_null() || out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        *out = std::ptr::null_mut();
        let dh = DestinationHash::new(read_array::<LEV_ADDR_LEN>(dest_hash));
        if !h.node().has_path(&dh) {
            set_last_error("no path to destination");
            return LEV_ERR_NO_PATH;
        }
        let key = read_array::<LEV_SIGNING_KEY_LEN>(signing_key);
        finish_connect(h, &dh, &key, timeout_ms, out)
    })
}

/// Obtain a handle to an already-established inbound link (16-byte link id
/// from a `LEV_EVENT_LINK_ESTABLISHED` event whose `inbound` flag is set).
///
/// Inbound links are auto-accepted and auto-proved by the stack (Python-RNS
/// parity): the establishment proof is already sent by the time the event
/// fires, so this no longer "accepts" anything. It synchronously mints a
/// writable handle for the live link; on success `*out` is that handle. To
/// decline inbound links, configure the destination not to accept them rather
/// than refraining from calling this. `timeout_ms` is retained for ABI
/// stability and is unused (the call does not block).
#[no_mangle]
pub unsafe extern "C" fn lev_accept_link(
    node: *const leviculum_t,
    link_id: *const u8,
    timeout_ms: c_int,
    out: *mut *mut lev_link_t,
) -> c_int {
    let _ = timeout_ms;
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() || out.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        *out = std::ptr::null_mut();
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        let rt = h.runtime().handle().clone();
        let link = h.node().accept_link(&lid);
        *out = Box::into_raw(Box::new(lev_link_t { inner: link, rt }));
        LEV_OK
    })
}

// --- Link data ---

/// Send data on a link, blocking up to `timeout_ms`. `LEV_ERR_AGAIN` is not
/// returned here; `lev_link_send` retries backpressure internally up to the
/// deadline, then `LEV_ERR_TIMEOUT`.
#[no_mangle]
pub unsafe extern "C" fn lev_link_send(
    link: *const lev_link_t,
    data: *const u8,
    len: usize,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let l = match link.as_ref() {
            Some(l) => l,
            None => return LEV_ERR_NULL_PTR,
        };
        let slice = match data_slice(data, len) {
            Some(s) => s,
            None => return LEV_ERR_NULL_PTR,
        };
        let fut = l.inner.send(slice);
        let res = if timeout_ms < 0 {
            Ok(l.rt.block_on(fut))
        } else {
            let dur = Duration::from_millis(timeout_ms as u64);
            l.rt.block_on(async { tokio::time::timeout(dur, fut).await })
                .map_err(|_| ())
        };
        match res {
            Ok(Ok(())) => LEV_OK,
            Ok(Err(e)) => map_link_send_err(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}

/// Try to send without blocking. `LEV_ERR_AGAIN` on backpressure (retry later).
#[no_mangle]
pub unsafe extern "C" fn lev_link_try_send(
    link: *const lev_link_t,
    data: *const u8,
    len: usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let l = match link.as_ref() {
            Some(l) => l,
            None => return LEV_ERR_NULL_PTR,
        };
        let slice = match data_slice(data, len) {
            Some(s) => s,
            None => return LEV_ERR_NULL_PTR,
        };
        match l.rt.block_on(l.inner.try_send(slice)) {
            Ok(()) => LEV_OK,
            Err(e) => map_link_send_err(&e),
        }
    })
}

/// Write the link id (16 bytes) into `buf`, read(2) style.
#[no_mangle]
pub unsafe extern "C" fn lev_link_id(
    link: *const lev_link_t,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let l = match link.as_ref() {
            Some(l) => l,
            None => return LEV_ERR_NULL_PTR,
        };
        write_out(l.inner.link_id().as_bytes(), buf, cap, out_len)
    })
}

/// Prove an identity to the peer on a link (16-byte link id). The peer sees a
/// `LEV_EVENT_LINK_IDENTIFIED` event and can read it via
/// `lev_link_remote_identity`. Blocks up to `timeout_ms`.
#[no_mangle]
pub unsafe extern "C" fn lev_link_identify(
    node: *const leviculum_t,
    link_id: *const u8,
    identity: *const lev_identity_t,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        let id = match identity.as_ref() {
            Some(id) => id,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        match block_on_timeout(
            h.runtime(),
            h.node().identify_link(&lid, &id.inner),
            timeout_ms,
        ) {
            Ok(Ok(())) => LEV_OK,
            Ok(Err(e)) => map_error(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}

/// Return the peer's identity on a link (16-byte link id) as a new identity
/// handle the caller frees, or NULL if the peer has not identified.
#[no_mangle]
pub unsafe extern "C" fn lev_link_remote_identity(
    node: *const leviculum_t,
    link_id: *const u8,
) -> *mut lev_identity_t {
    guard(std::ptr::null_mut(), || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return std::ptr::null_mut(),
        };
        if link_id.is_null() {
            return std::ptr::null_mut();
        }
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        match h.node().get_remote_identity(&lid) {
            Some(inner) => Box::into_raw(Box::new(lev_identity_t { inner })),
            None => std::ptr::null_mut(),
        }
    })
}

/// Return 1 if the link is closed, 0 otherwise (also 0 on NULL).
#[no_mangle]
pub unsafe extern "C" fn lev_link_is_closed(link: *const lev_link_t) -> c_int {
    guard(0, || match link.as_ref() {
        Some(l) if l.inner.is_closed() => 1,
        _ => 0,
    })
}

/// Close a link gracefully (idempotent). Blocks up to `timeout_ms`.
#[no_mangle]
pub unsafe extern "C" fn lev_close_link(link: *mut lev_link_t, timeout_ms: c_int) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let l = match link.as_mut() {
            Some(l) => l,
            None => return LEV_ERR_NULL_PTR,
        };
        let fut = l.inner.close();
        let res = if timeout_ms < 0 {
            Ok(l.rt.block_on(fut))
        } else {
            let dur = Duration::from_millis(timeout_ms as u64);
            l.rt.block_on(async { tokio::time::timeout(dur, fut).await })
                .map_err(|_| ())
        };
        match res {
            Ok(Ok(())) => LEV_OK,
            Ok(Err(e)) => map_error(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}

/// Free a link handle, closing it first if still open. `lev_link_free(NULL)` is
/// a no-op.
#[no_mangle]
pub unsafe extern "C" fn lev_link_free(link: *mut lev_link_t) {
    guard((), || {
        if link.is_null() {
            return;
        }
        let mut boxed = Box::from_raw(link);
        if !boxed.inner.is_closed() {
            // Best-effort graceful close; contain any panic so teardown is
            // deterministic.
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = boxed.rt.block_on(boxed.inner.close());
            }));
        }
        drop(boxed);
    })
}
