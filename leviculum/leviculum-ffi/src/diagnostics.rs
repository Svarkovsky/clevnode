//! Read-only diagnostics, so a C program can build an `rnstatus`-style view of
//! a node: the transport counters and the path-table size.

use std::os::raw::c_int;

use leviculum_std::api::LinkId;
use leviculum_std::{InterfaceStatusSnapshot, PathTableExport};

use crate::error::*;
use crate::node::leviculum_t;
use crate::{guard, write_out, LEV_ADDR_LEN};

/// Write the bound address (`ip:port`) of the node's TCP server listener
/// number `index` (in start order) into `buf`, read(2) style (a NULL `buf`
/// queries the length). A server added with port 0 reports the
/// kernel-assigned port here once the node has started — bind `:0` and read
/// the chosen port back instead of probing a free port up front and racing
/// every co-tenant for the re-bind (Codeberg #221). `LEV_ERR_INVALID_ARG` if
/// `index` is out of range (including "before start": the listeners bind
/// during `lev_start`).
#[no_mangle]
pub unsafe extern "C" fn lev_tcp_listen_addr(
    node: *const leviculum_t,
    index: usize,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        match h.node().tcp_listen_addrs().get(index) {
            Some(addr) => write_out(addr.to_string().as_bytes(), buf, cap, out_len),
            None => {
                set_last_error("TCP listener index out of range");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Read the transport counters into the provided out-parameters: packets sent,
/// received, forwarded, announces processed, packets dropped, and the current
/// path-table size. Any out-pointer may be NULL to skip that counter. Returns
/// `LEV_OK`, or `LEV_ERR_NULL_PTR` if `node` is NULL.
#[no_mangle]
pub unsafe extern "C" fn lev_transport_stats(
    node: *const leviculum_t,
    out_packets_sent: *mut u64,
    out_packets_received: *mut u64,
    out_packets_forwarded: *mut u64,
    out_announces_processed: *mut u64,
    out_packets_dropped: *mut u64,
    out_path_count: *mut u64,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        let stats = h.node().transport_stats();
        if !out_packets_sent.is_null() {
            *out_packets_sent = stats.packets_sent();
        }
        if !out_packets_received.is_null() {
            *out_packets_received = stats.packets_received();
        }
        if !out_packets_forwarded.is_null() {
            *out_packets_forwarded = stats.packets_forwarded();
        }
        if !out_announces_processed.is_null() {
            *out_announces_processed = stats.announces_processed();
        }
        if !out_packets_dropped.is_null() {
            *out_packets_dropped = stats.packets_dropped();
        }
        if !out_path_count.is_null() {
            *out_path_count = h.node().path_count() as u64;
        }
        LEV_OK
    })
}

/// An owned, point-in-time snapshot of the path table. Take it with
/// `lev_path_table_snapshot`, read entries by index, and release it with
/// `lev_path_table_free`. Because it is a frozen copy, reads never race with a
/// changing table.
pub struct lev_path_table_t {
    entries: Vec<PathTableExport>,
}

/// Capture a snapshot of the node's path table. Returns an owned handle (free
/// with `lev_path_table_free`), or NULL on a NULL node.
#[no_mangle]
pub unsafe extern "C" fn lev_path_table_snapshot(
    node: *const leviculum_t,
) -> *mut lev_path_table_t {
    guard(std::ptr::null_mut(), || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return std::ptr::null_mut(),
        };
        let entries = h.node().path_table();
        Box::into_raw(Box::new(lev_path_table_t { entries }))
    })
}

/// Number of entries in a path-table snapshot, or 0 on NULL.
#[no_mangle]
pub unsafe extern "C" fn lev_path_table_count(table: *const lev_path_table_t) -> c_int {
    guard(0, || match table.as_ref() {
        Some(t) => t.entries.len() as c_int,
        None => 0,
    })
}

/// Read one entry of a path-table snapshot by index into the out-parameters.
/// `dest_hash` and `next_hop`, if non-NULL, must be at least `LEV_ADDR_LEN`
/// (16) bytes; `has_next_hop` reports whether `next_hop` was written (a relayed
/// path has one, a direct path does not). Any out-pointer may be NULL to skip
/// it. `LEV_ERR_INVALID_ARG` if `index` is out of range.
#[no_mangle]
pub unsafe extern "C" fn lev_path_table_entry(
    table: *const lev_path_table_t,
    index: usize,
    dest_hash: *mut u8,
    hops: *mut u8,
    next_hop: *mut u8,
    has_next_hop: *mut c_int,
    interface_index: *mut u64,
    expires_ms: *mut u64,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let t = match table.as_ref() {
            Some(t) => t,
            None => return LEV_ERR_NULL_PTR,
        };
        let entry = match t.entries.get(index) {
            Some(e) => e,
            None => {
                set_last_error("path entry index out of range");
                return LEV_ERR_INVALID_ARG;
            }
        };
        if !dest_hash.is_null() {
            std::ptr::copy_nonoverlapping(entry.hash.as_ptr(), dest_hash, LEV_ADDR_LEN);
        }
        if !hops.is_null() {
            *hops = entry.hops;
        }
        if !has_next_hop.is_null() {
            *has_next_hop = c_int::from(entry.next_hop.is_some());
        }
        if let Some(nh) = entry.next_hop.as_ref() {
            if !next_hop.is_null() {
                std::ptr::copy_nonoverlapping(nh.as_ptr(), next_hop, LEV_ADDR_LEN);
            }
        }
        if !interface_index.is_null() {
            *interface_index = entry.interface_index as u64;
        }
        if !expires_ms.is_null() {
            *expires_ms = entry.expires_ms;
        }
        LEV_OK
    })
}

/// Release a path-table snapshot. `lev_path_table_free(NULL)` is a no-op.
#[no_mangle]
pub unsafe extern "C" fn lev_path_table_free(table: *mut lev_path_table_t) {
    guard((), || {
        if !table.is_null() {
            drop(Box::from_raw(table));
        }
    })
}

/// An owned, point-in-time snapshot of every interface. Take it with
/// `lev_interface_stats_snapshot`, read entries by index, and release it with
/// `lev_interface_stats_free`.
pub struct lev_interface_stats_t {
    entries: Vec<InterfaceStatusSnapshot>,
}

/// Capture a snapshot of the node's interfaces (name, online status, byte
/// counters). Returns an owned handle (free with `lev_interface_stats_free`),
/// or NULL on a NULL node.
#[no_mangle]
pub unsafe extern "C" fn lev_interface_stats_snapshot(
    node: *const leviculum_t,
) -> *mut lev_interface_stats_t {
    guard(std::ptr::null_mut(), || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return std::ptr::null_mut(),
        };
        let entries = h.node().interface_stats();
        Box::into_raw(Box::new(lev_interface_stats_t { entries }))
    })
}

/// Number of interfaces in a snapshot, or 0 on NULL.
#[no_mangle]
pub unsafe extern "C" fn lev_interface_stats_count(table: *const lev_interface_stats_t) -> c_int {
    guard(0, || match table.as_ref() {
        Some(t) => t.entries.len() as c_int,
        None => 0,
    })
}

/// Write the name of interface `index` into `buf`, read(2) style (the name is
/// variable length; a NULL `buf` queries the length). `LEV_ERR_INVALID_ARG` if
/// `index` is out of range.
#[no_mangle]
pub unsafe extern "C" fn lev_interface_stats_name(
    table: *const lev_interface_stats_t,
    index: usize,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let t = match table.as_ref() {
            Some(t) => t,
            None => return LEV_ERR_NULL_PTR,
        };
        match t.entries.get(index) {
            Some(e) => write_out(e.name.as_bytes(), buf, cap, out_len),
            None => {
                set_last_error("interface index out of range");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Read the scalar fields of interface `index` into the out-parameters: online
/// (1/0), is_local_client (1/0), and the byte counters. Any out-pointer may be
/// NULL to skip it. `LEV_ERR_INVALID_ARG` if `index` is out of range. Read the
/// name with `lev_interface_stats_name`.
#[no_mangle]
pub unsafe extern "C" fn lev_interface_stats_entry(
    table: *const lev_interface_stats_t,
    index: usize,
    online: *mut c_int,
    is_local_client: *mut c_int,
    rx_bytes: *mut u64,
    tx_bytes: *mut u64,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let t = match table.as_ref() {
            Some(t) => t,
            None => return LEV_ERR_NULL_PTR,
        };
        let entry = match t.entries.get(index) {
            Some(e) => e,
            None => {
                set_last_error("interface index out of range");
                return LEV_ERR_INVALID_ARG;
            }
        };
        if !online.is_null() {
            *online = c_int::from(entry.online);
        }
        if !is_local_client.is_null() {
            *is_local_client = c_int::from(entry.is_local_client);
        }
        if !rx_bytes.is_null() {
            *rx_bytes = entry.rx_bytes;
        }
        if !tx_bytes.is_null() {
            *tx_bytes = entry.tx_bytes;
        }
        LEV_OK
    })
}

/// Write the node-assigned id of interface `index` into `*out_id`.
///
/// The snapshot is indexed by *position*, which is not an interface's identity:
/// the node numbers interfaces as they are registered and never renumbers, so a
/// removed interface leaves a gap and every later position is off by it. This is
/// the accessor that resolves an id to an entry — the ids the rest of the API
/// hands out (`interface_index` from `lev_path_table_entry`,
/// `lev_event_interface_id` on an announce, path or packet event) name *this*
/// value, not a position. Walk the snapshot comparing it, then read the name
/// with `lev_interface_stats_name`.
///
/// `LEV_ERR_NULL_PTR` if `table` or `out_id` is NULL, `LEV_ERR_INVALID_ARG` if
/// `index` is out of range.
#[no_mangle]
pub unsafe extern "C" fn lev_interface_stats_id(
    table: *const lev_interface_stats_t,
    index: usize,
    out_id: *mut u64,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let t = match table.as_ref() {
            Some(t) => t,
            None => return LEV_ERR_NULL_PTR,
        };
        if out_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        match t.entries.get(index) {
            Some(e) => {
                *out_id = e.interface_id.0 as u64;
                LEV_OK
            }
            None => {
                set_last_error("interface index out of range");
                LEV_ERR_INVALID_ARG
            }
        }
    })
}

/// Release an interface-stats snapshot. `lev_interface_stats_free(NULL)` is a
/// no-op.
#[no_mangle]
pub unsafe extern "C" fn lev_interface_stats_free(table: *mut lev_interface_stats_t) {
    guard((), || {
        if !table.is_null() {
            drop(Box::from_raw(table));
        }
    })
}

/// Read the per-link delivery telemetry (leviculum#35) for the link with the
/// given 16-byte id into the provided out-parameters. Any out-pointer may be
/// NULL to skip that value. Counters are cumulative; sample periodically and
/// difference consecutive readings.
///
/// - `out_bytes_delivered` — proof-confirmed bytes (channel envelopes plus
///   completed outgoing resources), the delivery-rate numerator.
/// - `out_srtt_ms` / `out_rttvar_ms` — RFC 6298 smoothed delivery RTT and
///   variance in milliseconds; `-1.0` until the first Karn-valid sample.
/// - `out_min_rtt_ms` — floor of Karn-valid delivery RTT samples in
///   milliseconds; `-1` until the first sample.
/// - `out_rtt_ms` — handshake RTT from link establishment in milliseconds;
///   `-1` when not measured.
/// - `out_busy_rejections` / `out_pacing_rejections` /
///   `out_iface_pacing_rejections` — sends rejected by a full channel window,
///   the link pacer, and the interface airtime gate respectively: a non-zero
///   interval delta marks the interval congestion-limited rather than
///   app-limited.
/// - `out_tx_ring_size`, `out_window`, `out_window_max`,
///   `out_pacing_interval_ms` — the channel flow-control state.
///
/// Returns `LEV_OK`; `LEV_ERR_NULL_PTR` on a NULL node or link id;
/// `LEV_ERR_LINK` when no link with that id exists.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn lev_link_stats(
    node: *const leviculum_t,
    link_id: *const u8,
    out_bytes_delivered: *mut u64,
    out_srtt_ms: *mut f64,
    out_rttvar_ms: *mut f64,
    out_min_rtt_ms: *mut i64,
    out_rtt_ms: *mut i64,
    out_busy_rejections: *mut u64,
    out_pacing_rejections: *mut u64,
    out_iface_pacing_rejections: *mut u64,
    out_tx_ring_size: *mut u64,
    out_window: *mut u64,
    out_window_max: *mut u64,
    out_pacing_interval_ms: *mut u64,
) -> c_int {
    guard(LEV_ERR_PANIC, || {
        let h = match node.as_ref() {
            Some(h) => h,
            None => return LEV_ERR_NULL_PTR,
        };
        if link_id.is_null() {
            return LEV_ERR_NULL_PTR;
        }
        let lid = LinkId::new(crate::read_array::<LEV_ADDR_LEN>(link_id));
        let stats = match h.node().engine().link_stats(&lid) {
            Some(s) => s,
            None => return LEV_ERR_LINK,
        };
        if !out_bytes_delivered.is_null() {
            *out_bytes_delivered = stats.bytes_delivered();
        }
        if !out_srtt_ms.is_null() {
            *out_srtt_ms = stats.srtt_ms().unwrap_or(-1.0);
        }
        if !out_rttvar_ms.is_null() {
            *out_rttvar_ms = stats.rttvar_ms().unwrap_or(-1.0);
        }
        if !out_min_rtt_ms.is_null() {
            *out_min_rtt_ms = stats.min_rtt_ms().map(|v| v as i64).unwrap_or(-1);
        }
        if !out_rtt_ms.is_null() {
            *out_rtt_ms = stats.rtt_ms().map(|v| v as i64).unwrap_or(-1);
        }
        if !out_busy_rejections.is_null() {
            *out_busy_rejections = stats.busy_rejections();
        }
        if !out_pacing_rejections.is_null() {
            *out_pacing_rejections = stats.pacing_rejections();
        }
        if !out_iface_pacing_rejections.is_null() {
            *out_iface_pacing_rejections = stats.iface_pacing_rejections();
        }
        if !out_tx_ring_size.is_null() {
            *out_tx_ring_size = stats.tx_ring_size() as u64;
        }
        if !out_window.is_null() {
            *out_window = stats.window() as u64;
        }
        if !out_window_max.is_null() {
            *out_window_max = stats.window_max() as u64;
        }
        if !out_pacing_interval_ms.is_null() {
            *out_pacing_interval_ms = stats.pacing_interval_ms();
        }
        LEV_OK
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot whose entries carry the given node-assigned ids, in the given
    /// order. Everything else is filler: the point is the id/position split.
    fn snapshot(ids: &[usize]) -> lev_interface_stats_t {
        lev_interface_stats_t {
            entries: ids
                .iter()
                .map(|&id| InterfaceStatusSnapshot {
                    interface_id: leviculum_std::InterfaceId(id),
                    name: format!("iface{id}"),
                    is_local_client: false,
                    online: true,
                    rx_bytes: 0,
                    tx_bytes: 0,
                    held_announces: 0,
                    burst_active: false,
                    configured_bitrate: None,
                    kind: Default::default(),
                })
                .collect(),
        }
    }

    /// Position is not identity. The node numbers interfaces on registration and
    /// never renumbers, so a snapshot of interfaces 3 and 7 has them at
    /// positions 0 and 1 — and until this accessor existed the C ABI surfaced
    /// only the position, leaving the ids it hands out elsewhere
    /// (`lev_path_table_entry`'s `interface_index`, `lev_event_interface_id`)
    /// naming nothing a C app could resolve.
    #[test]
    fn stats_id_reports_the_node_assigned_id_not_the_position() {
        let table = snapshot(&[3, 7]);
        let p = &table as *const lev_interface_stats_t;
        unsafe {
            let mut id = u64::MAX;
            assert_eq!(lev_interface_stats_id(p, 0, &mut id), LEV_OK);
            assert_eq!(id, 3, "position 0 holds interface id 3, not id 0");
            assert_eq!(lev_interface_stats_id(p, 1, &mut id), LEV_OK);
            assert_eq!(id, 7, "position 1 holds interface id 7, not id 1");
        }
    }

    /// The guards, in the shape every other snapshot accessor uses.
    #[test]
    fn stats_id_rejects_null_and_out_of_range() {
        let table = snapshot(&[0]);
        let p = &table as *const lev_interface_stats_t;
        unsafe {
            let mut id = 0u64;
            assert_eq!(
                lev_interface_stats_id(std::ptr::null(), 0, &mut id),
                LEV_ERR_NULL_PTR
            );
            assert_eq!(
                lev_interface_stats_id(p, 0, std::ptr::null_mut()),
                LEV_ERR_NULL_PTR
            );
            assert_eq!(lev_interface_stats_id(p, 1, &mut id), LEV_ERR_INVALID_ARG);
        }
    }
}
