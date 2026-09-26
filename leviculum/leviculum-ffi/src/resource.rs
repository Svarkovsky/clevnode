//! Resource transfer: sending bulk data over a link, and accepting incoming
//! transfers.
//!
//! A sender calls `lev_send_resource`; the receiver either auto-accepts
//! (strategy `LEV_RESOURCE_ACCEPT_ALL`) or is advertised the transfer
//! (`LEV_RESOURCE_ACCEPT_APP`) and calls `lev_accept_resource`. Progress and
//! completion arrive as events. See `docs/leviculum-api-design.md` §10.

use std::os::raw::c_int;

use leviculum_std::api::{LinkId, ResourceStrategy};

use crate::error::*;
use crate::node::{block_on_timeout, leviculum_t};
use crate::{guard, read_array, LEV_ADDR_LEN};

/// Length of a resource hash, in bytes.
pub const LEV_RESOURCE_HASH_LEN: usize = 32;

/// Reject all incoming resources.
pub const LEV_RESOURCE_ACCEPT_NONE: c_int = 0;
/// Accept all incoming resources automatically.
pub const LEV_RESOURCE_ACCEPT_ALL: c_int = 1;
/// Advertise incoming resources to the app, which accepts or rejects.
pub const LEV_RESOURCE_ACCEPT_APP: c_int = 2;

fn strategy_from(s: c_int) -> Option<ResourceStrategy> {
    match s {
        LEV_RESOURCE_ACCEPT_NONE => Some(ResourceStrategy::AcceptNone),
        LEV_RESOURCE_ACCEPT_ALL => Some(ResourceStrategy::AcceptAll),
        LEV_RESOURCE_ACCEPT_APP => Some(ResourceStrategy::AcceptApp),
        _ => None,
    }
}

/// Send a resource over an established link (16-byte link id), writing the
/// 32-byte resource hash into `out_hash`. `metadata`, if present, must be
/// msgpack-encoded. `auto_compress` is a boolean (0 or 1). Blocks up to
/// `timeout_ms` for the initial dispatch; progress and completion are events.
#[no_mangle]
pub unsafe extern "C" fn lev_send_resource(
    node: *const leviculum_t,
    link_id: *const u8,
    data: *const u8,
    data_len: usize,
    metadata: *const u8,
    metadata_len: usize,
    auto_compress: c_int,
    out_hash: *mut u8,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() || out_hash.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        if (data.is_null() && data_len > 0) || (metadata.is_null() && metadata_len > 0) {
            return LEV_ERR_NULL_PTR;
        }
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        let payload: &[u8] = if data_len == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(data, data_len)
        };
        let meta: Option<&[u8]> = if metadata.is_null() || metadata_len == 0 {
            None
        } else {
            Some(std::slice::from_raw_parts(metadata, metadata_len))
        };
        let fut = h
            .node()
            .send_resource(&lid, payload, meta, auto_compress != 0);
        match block_on_timeout(h.runtime(), fut, timeout_ms) {
            Ok(Ok(hash)) => {
                std::ptr::copy_nonoverlapping(hash.as_ptr(), out_hash, LEV_RESOURCE_HASH_LEN);
                LEV_OK
            }
            Ok(Err(e)) => map_error(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}

/// Set the acceptance strategy for incoming resources on a link.
#[no_mangle]
pub unsafe extern "C" fn lev_set_resource_strategy(
    node: *const leviculum_t,
    link_id: *const u8,
    strategy: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let strategy = match strategy_from(strategy) {
            Some(s) => s,
            None => {
                set_last_error("invalid resource strategy");
                return LEV_ERR_INVALID_ARG;
            }
        };
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        match h.node().set_resource_strategy(&lid, strategy) {
            Ok(()) => LEV_OK,
            Err(e) => map_error(&e),
        }
    })
}

/// Accept a resource advertised on a link (after a
/// `LEV_EVENT_RESOURCE_ADVERTISED` event under the AcceptApp strategy).
#[no_mangle]
pub unsafe extern "C" fn lev_accept_resource(
    node: *const leviculum_t,
    link_id: *const u8,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        match block_on_timeout(h.runtime(), h.node().accept_resource(&lid), timeout_ms) {
            Ok(Ok(())) => LEV_OK,
            Ok(Err(e)) => map_error(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}

/// Reject a resource advertised on a link.
#[no_mangle]
pub unsafe extern "C" fn lev_reject_resource(
    node: *const leviculum_t,
    link_id: *const u8,
    timeout_ms: c_int,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let lid = LinkId::new(read_array::<LEV_ADDR_LEN>(link_id));
        match block_on_timeout(h.runtime(), h.node().reject_resource(&lid), timeout_ms) {
            Ok(Ok(())) => LEV_OK,
            Ok(Err(e)) => map_error(&e),
            Err(()) => LEV_ERR_TIMEOUT,
        }
    })
}
