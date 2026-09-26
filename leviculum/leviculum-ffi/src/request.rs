//! Request and response over an established link.
//!
//! A responder registers a handler for a path on a local destination; a
//! requester sends a request on a link, gets a request id back, and the
//! response (or a timeout) arrives as an event. Request and response payloads
//! are msgpack-encoded values. See `docs/leviculum-api-design.md` §10.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

use leviculum_std::api::{DestinationHash, LinkId, RequestPolicy};

use crate::error::*;
use crate::node::{block_on_timeout, leviculum_t};
use crate::{guard, read_array, LEV_ADDR_LEN};

/// Drop all requests.
pub const LEV_REQUEST_POLICY_ALLOW_NONE: c_int = 0;
/// Allow requests from any identity.
pub const LEV_REQUEST_POLICY_ALLOW_ALL: c_int = 1;
/// Allow only identities in `allow_identity_hashes`.
pub const LEV_REQUEST_POLICY_ALLOW_LIST: c_int = 2;

/// Build a `RequestPolicy` from the flat code and the optional allowlist.
unsafe fn policy_from(
    policy: c_int,
    allow_identity_hashes: *const u8,
    n_ids: usize,
) -> Option<RequestPolicy> {
    match policy {
        LEV_REQUEST_POLICY_ALLOW_NONE => Some(RequestPolicy::AllowNone),
        LEV_REQUEST_POLICY_ALLOW_ALL => Some(RequestPolicy::AllowAll),
        LEV_REQUEST_POLICY_ALLOW_LIST => {
            if n_ids == 0 {
                return Some(RequestPolicy::AllowList(Vec::new()));
            }
            if allow_identity_hashes.is_null() {
                return None;
            }
            // `n_ids * LEV_ADDR_LEN` would wrap in release for a huge `n_ids`,
            // feeding a truncated length into from_raw_parts and reading out of
            // bounds. Reject the overflow before allocating or slicing (also
            // keeps `Vec::with_capacity(n_ids)` below from a capacity overflow).
            let total = n_ids.checked_mul(LEV_ADDR_LEN)?;
            let mut ids: Vec<[u8; LEV_ADDR_LEN]> = Vec::with_capacity(n_ids);
            let bytes = std::slice::from_raw_parts(allow_identity_hashes, total);
            for chunk in bytes.chunks_exact(LEV_ADDR_LEN) {
                let mut id = [0u8; LEV_ADDR_LEN];
                id.copy_from_slice(chunk);
                ids.push(id);
            }
            Some(RequestPolicy::AllowList(ids))
        }
        _ => None,
    }
}

/// Register a request handler for `path` on a local destination (16-byte hash).
///
/// `allow_identity_hashes` is `n_ids * 16` bytes of identity hashes, read only
/// for `LEV_REQUEST_POLICY_ALLOW_LIST`. Registering overwrites any previous
/// handler for the same destination and path; there is no unregister.
#[no_mangle]
pub unsafe extern "C" fn lev_register_request_handler(
    node: *const leviculum_t,
    dest_hash: *const u8,
    path: *const c_char,
    policy: c_int,
    allow_identity_hashes: *const u8,
    n_ids: usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if dest_hash.is_null() || path.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let path = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error("path must be a valid UTF-8 string");
                return LEV_ERR_INVALID_ARG;
            }
        };
        let policy = match policy_from(policy, allow_identity_hashes, n_ids) {
            Some(p) => p,
            None => {
                set_last_error("invalid request policy or allowlist");
                return LEV_ERR_INVALID_ARG;
            }
        };
        let dh = DestinationHash::new(read_array::<LEV_ADDR_LEN>(dest_hash));
        h.node().register_request_handler(dh, path, policy);
        LEV_OK
    })
}

/// Send a request on an established link (16-byte link id) to `path`, writing
/// the 16-byte request id into `out_request_id`. `data`/`data_len` is the
/// msgpack-encoded payload (NULL/0 for none). `response_timeout_ms` is the
/// request-response deadline (negative for none); the response or a timeout
/// arrives as an event.
#[no_mangle]
pub unsafe extern "C" fn lev_send_request(
    node: *const leviculum_t,
    link_id: *const u8,
    path: *const c_char,
    data: *const u8,
    data_len: usize,
    response_timeout_ms: c_int,
    out_request_id: *mut u8,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() || path.is_null() || out_request_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if data.is_null() && data_len > 0 {
            return LEV_ERR_NULL_PTR;
        }
        let path = match CStr::from_ptr(path).to_str() {
            Ok(s) => s,
            Err(_) => {
                set_last_error("path must be a valid UTF-8 string");
                return LEV_ERR_INVALID_ARG;
            }
        };
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        let payload: Option<&[u8]> = if data_len == 0 {
            None
        } else {
            Some(std::slice::from_raw_parts(data, data_len))
        };
        let timeout = if response_timeout_ms < 0 {
            None
        } else {
            Some(response_timeout_ms as u64)
        };
        // Dispatch is bounded by loop liveness; the response deadline is carried
        // by the request itself, not this call.
        match h
            .runtime()
            .block_on(h.node().send_request(&lid, path, payload, timeout))
        {
            Ok(request_id) => {
                std::ptr::copy_nonoverlapping(request_id.as_ptr(), out_request_id, LEV_ADDR_LEN);
                LEV_OK
            }
            Err(e) => map_error(&e),
        }
    })
}

/// Send a response to a received request (link id and request id, each 16
/// bytes). `data`/`data_len` must be one valid msgpack-encoded value. Blocks up
/// to `timeout_ms`.
#[no_mangle]
pub unsafe extern "C" fn lev_send_response(
    node: *const leviculum_t,
    link_id: *const u8,
    request_id: *const u8,
    data: *const u8,
    data_len: usize,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() || request_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if data.is_null() && data_len > 0 {
            return LEV_ERR_NULL_PTR;
        }
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        let rid = read_array::<LEV_ADDR_LEN>(request_id);
        let payload: &[u8] = if data_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data, data_len)
        };
        match block_on_timeout(
            h.runtime(),
            h.node().send_response(&lid, &rid, payload),
            timeout_ms,
        ) {
            Ok(Ok(())) => LEV_OK,
            Ok(Err(e)) => map_error(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}

/// Send a response resource to a received request (link id and request id, each 16
/// bytes). `data`/`data_len` must be one valid msgpack-encoded value. Blocks up
/// to `timeout_ms`.
#[no_mangle]
pub unsafe extern "C" fn lev_send_response_resource(
    node: *const leviculum_t,
    link_id: *const u8,
    request_id: *const u8,
    data: *const u8,
    data_len: usize,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() || request_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if data.is_null() && data_len > 0 {
            return LEV_ERR_NULL_PTR;
        }
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        let rid = read_array::<LEV_ADDR_LEN>(request_id);
        let payload: &[u8] = if data_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data, data_len)
        };
        match block_on_timeout(
            h.runtime(),
            h.node().send_response_resource(&lid, &rid, payload),
            timeout_ms,
        ) {
            Ok(Ok(())) => LEV_OK,
            Ok(Err(e)) => map_error(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // An allowlist length that overflows `n_ids * LEV_ADDR_LEN` must be rejected
    // (None -> LEV_ERR_INVALID_ARG at the call site) before any from_raw_parts,
    // never silently truncated into an out-of-bounds read.
    #[test]
    fn allowlist_length_overflow_is_rejected() {
        let dummy = [0u8; LEV_ADDR_LEN];
        // n_ids * 16 overflows usize; the pointer is non-null so the only thing
        // that can stop us reading it is the checked multiply.
        let n_ids = usize::MAX / 4;
        let policy = unsafe { policy_from(LEV_REQUEST_POLICY_ALLOW_LIST, dummy.as_ptr(), n_ids) };
        assert!(policy.is_none(), "overflowing allowlist must be rejected");
    }

    // A well-formed allowlist still parses into the expected ids.
    #[test]
    fn allowlist_parses_small_input() {
        let ids = [[1u8; LEV_ADDR_LEN], [2u8; LEV_ADDR_LEN]];
        let flat: Vec<u8> = ids.iter().flatten().copied().collect();
        let policy =
            unsafe { policy_from(LEV_REQUEST_POLICY_ALLOW_LIST, flat.as_ptr(), ids.len()) };
        match policy {
            Some(RequestPolicy::AllowList(parsed)) => assert_eq!(parsed, ids),
            other => panic!("expected AllowList, got {other:?}"),
        }
    }
}
