//! RPC command dispatch: maps requests to node state queries

use std::sync::atomic::Ordering;

use crate::sync_ext::MutexRecover;

use leviculum_core::constants::{
    DEFAULT_PER_HOP_TIMEOUT, MINIMUM_BITRATE, MTU, TRUNCATED_HASHBYTES,
};
use leviculum_core::traits::InterfaceKind;
use serde_pickle::value::{HashableValue, Value};

use super::error::RpcError;
use super::pickle::*;
use crate::driver::StdNodeCore;
use crate::interfaces::inventory::SharedInventory;
use crate::interfaces::InterfaceStatsMap;

/// Dispatch an RPC request against node state and return the pickle-encoded response.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_request(
    request: &RpcRequest,
    core: &mut StdNodeCore,
    start_time: std::time::Instant,
    iface_stats_map: &InterfaceStatsMap,
    inventory: &SharedInventory,
    auto_peer_count: usize,
    discovery_storage: Option<&std::path::Path>,
    codec: Codec,
) -> Result<Vec<u8>, RpcError> {
    let response = match request {
        // Full implementations
        RpcRequest::GetInterfaceStats => build_interface_stats(
            core,
            start_time,
            iface_stats_map,
            inventory,
            auto_peer_count,
        ),
        // The pair `rnstatus -l` renders as "N entries in link table
        // (M active)". Both answer from the TRANSPORT link table — the links
        // this node RELAYS — because that is what the verbs mean upstream:
        // `Reticulum.get_link_count` returns `Transport.link_count()` ==
        // `len(Transport.link_table)`, and `get_active_link_count` returns
        // `Transport.active_link_count()`, its validated subset (Reticulum
        // 1.5.2, Transport.py:3211-3216). Answering with the count of links
        // this node TERMINATES, as we did until this batch, put a different
        // number under the same operator-visible line depending on which
        // daemon rnstatus was pointed at.
        RpcRequest::GetLinkCount => pickle_int(core.transport_link_table_entries().len() as i64),
        // Deliberate deviation, wire- and semantics-preserving: upstream's
        // `Transport.active_link_count` iterates the link table's KEYS and
        // indexes `entry[IDX_LT_VALIDATED]` on the 16-byte link id, so it
        // counts entries whose eighth link-id byte happens to be non-zero
        // rather than entries that validated (1.5.2, Transport.py:3215-3216).
        // We answer the documented meaning; mirroring the accident would make
        // the operator's "(M active)" a function of hash bytes.
        RpcRequest::GetActiveLinkCount => pickle_int(
            core.transport_link_table_entries()
                .iter()
                .filter(|e| e.validated)
                .count() as i64,
        ),
        RpcRequest::GetLinkTable => build_link_table(core),

        // The timing pair every path-request client sizes its wait window
        // with (Codeberg #329). Until they existed, an `rnprobe`/`rnpath`/
        // `rncp` against lnsd took the exception arm of
        // `Reticulum.get_medium_path_timeout` (Reticulum 1.5.2) and
        // waited 0 s where the same client against rnsd waits seconds — a
        // config difference smuggled into every timing A/B.
        RpcRequest::GetLowestInterfaceBitrate => {
            match lowest_online_bitrate(core, iface_stats_map, inventory) {
                Some(bps) => pickle_int(bps),
                None => pickle_none(),
            }
        }
        RpcRequest::GetMediumPathTimeout => pickle_float(medium_path_timeout_secs(
            lowest_online_bitrate(core, iface_stats_map, inventory),
        )),

        // `profiling_results` is deliberately NOT served (declined in the
        // #329 remainder batch, 2026-08-30). Upstream fills it from a
        // profiler we do not run (`Reticulum.get_profiling_results`,
        // Reticulum 1.5.2), and its only consumer degrades to silence
        // rather than crashing: `rnstatus --profiling` wraps the call in
        // `except Exception: pass` and renders nothing when it fails
        // (`rnstatus --profiling`, 1.5.2). The blast radius is one call: the
        // client opens a FRESH `multiprocessing.connection.Client` per verb
        // (`Reticulum.get_rpc_client`), so the closed connection cannot poison
        // the `interface_stats` read two lines later. Serving a fabricated
        // shape would be a worse answer than not knowing the question.
        RpcRequest::GetTransportTables => build_transport_tables(core, start_time),
        RpcRequest::GetIdentityTable => build_identity_table(core, start_time),
        RpcRequest::GetDiscoveredInterfaces => build_discovered_interfaces(discovery_storage),
        RpcRequest::GetPathTable { max_hops } => build_path_table(core, start_time, *max_hops),
        RpcRequest::GetRateTable => build_rate_table(core, start_time),
        RpcRequest::GetNextHop { destination_hash } => get_next_hop(core, destination_hash),
        RpcRequest::GetNextHopIfName { destination_hash } => {
            get_next_hop_if_name(core, destination_hash)
        }
        RpcRequest::GetFirstHopTimeout { destination_hash } => {
            // Mirror Python Transport.first_hop_timeout (Transport.py:2700):
            // scales with the next-hop interface bitrate, falling back to the
            // flat DEFAULT_PER_HOP_TIMEOUT when the path/bitrate is unknown.
            let bitrate =
                try_into_hash(destination_hash).and_then(|h| core.next_hop_interface_bitrate(&h));
            pickle_float(first_hop_timeout_secs(bitrate))
        }
        RpcRequest::DropPath { destination_hash } => drop_path(core, destination_hash),
        RpcRequest::DropAllVia { destination_hash } => drop_all_via(core, destination_hash),
        RpcRequest::DropAnnounceQueues => pickle_bool(true),

        // Radio-only (always None for TCP/UDP/Auto)
        RpcRequest::GetPacketRssi { .. } => pickle_none(),
        RpcRequest::GetPacketSnr { .. } => pickle_none(),
        RpcRequest::GetPacketQ { .. } => pickle_none(),

        // Blackhole set (Codeberg #67 + #88). Real membership-backed set on
        // Transport; matches the Python wire contract (Reticulum.py:1699-1742,
        // Transport.py:3409-3448). Enforcement mirrors Python's three read
        // sites: inbound announces from blackholed identities are dropped
        // (Identity.py:574-577), matching path-table rows are removed on
        // blackhole (Transport.py:3423, 3494-3513), and incoming links that
        // identify as a blackholed identity are torn down (Link.py:1021-1023).
        // Expired `until` entries are swept from the driver's timer branch
        // (Transport.py:973-994).
        RpcRequest::GetBlackholedIdentities => build_blackholed_identities(core),
        RpcRequest::BlackholeIdentity {
            identity_hash,
            until,
            reason,
        } => blackhole_identity(core, identity_hash, *until, reason.clone()),
        RpcRequest::UnblackholeIdentity { identity_hash } => {
            unblackhole_identity(core, identity_hash)
        }
        RpcRequest::IsBlackholed { identity_hash } => is_blackholed(core, identity_hash),

        // destination_data cache lifecycle (Codeberg #84).
        //
        // Upstream Reticulum b5658c4 (2026-04-20, "Keep track of which
        // known destinations are actually in use, so irrelevant
        // destination data can be cleaned") added a known_destinations
        // GC scheme exposed via three RPC ops on the destination_data
        // dict key: "used", "retain", "unretain".  These map onto our
        // announce_cache: "retain" pins an entry so clean_announce_cache
        // never evicts it, "unretain" lifts the pin, "used" is a recency
        // touch that skips pinned entries — the same use-state semantics
        // as Python's known_destinations[dest][4].  A Python tool driving
        // lnsd sees the same booleans and the same survival-under-pressure
        // behaviour as against a Python rnsd.
        RpcRequest::DestinationDataUsed { destination_hash } => {
            destination_data_used(core, destination_hash)
        }
        RpcRequest::DestinationDataRetain { destination_hash } => {
            destination_data_retain(core, destination_hash)
        }
        RpcRequest::DestinationDataUnretain { destination_hash } => {
            destination_data_unretain(core, destination_hash)
        }

        // identity_data cache lifecycle (Codeberg #84).
        //
        // rnid retains an identity after a successful recall via
        // Reticulum._retain_identity, which issues
        // `{"identity_data": "retain", "identity_hash": <bytes>}` to the
        // shared instance (Reticulum.py:1316). Upstream rnsd dispatches it
        // to Identity._retain_identity, which retains every known
        // destination whose public key hashes to that identity. We mirror
        // that: retain/unretain pin/unpin all announce_cache destinations
        // matching the identity. The unretain arm is symmetric though
        // current upstream only emits retain.
        RpcRequest::IdentityDataRetain { identity_hash } => {
            identity_data_retain(core, identity_hash)
        }
        RpcRequest::IdentityDataUnretain { identity_hash } => {
            identity_data_unretain(core, identity_hash)
        }
    };

    serialize_response(&response, codec)
}

// Discovered interfaces (rnstatus -d/-D, Codeberg #32)
/// Build the `discovered_interfaces` response: a list of per-record dicts read
/// from the persisted registry under `<storage>/discovery/interfaces`.
///
/// Each dict mirrors Python `list_discovered_interfaces`: the announce fields
/// plus `status`/`status_code` derived from `last_heard`. `transport_id` /
/// `network_id` are hex strings; `stamp` / `discovery_hash` are bytes; optional
/// type-specific fields are present only when the record carries them. When no
/// storage path is configured the list is empty (no discovery registry).
fn build_discovered_interfaces(discovery_storage: Option<&std::path::Path>) -> Value {
    let Some(storage_root) = discovery_storage else {
        return pickle_list(vec![]);
    };
    let now = crate::discovery::now_unix_secs();
    let records = crate::discovery::list_discovered_interfaces(storage_root, now);

    let items = records
        .into_iter()
        .map(|r| {
            let status = r.status(now);
            let mut entries: Vec<(HashableValue, Value)> = vec![
                (pickle_str_key("type"), pickle_str(&r.interface_type)),
                (pickle_str_key("transport"), pickle_bool(r.transport)),
                (pickle_str_key("name"), pickle_str(&r.name)),
                (pickle_str_key("received"), pickle_float(r.received)),
                (pickle_str_key("stamp"), pickle_bytes(&r.stamp)),
                (pickle_str_key("value"), pickle_int(r.value as i64)),
                (pickle_str_key("transport_id"), pickle_str(&r.transport_id)),
                (pickle_str_key("network_id"), pickle_str(&r.network_id)),
                (pickle_str_key("hops"), pickle_int(r.hops as i64)),
                (pickle_str_key("latitude"), opt_float(r.latitude)),
                (pickle_str_key("longitude"), opt_float(r.longitude)),
                (pickle_str_key("height"), opt_float(r.height)),
                (
                    pickle_str_key("discovery_hash"),
                    pickle_bytes(&r.discovery_hash),
                ),
                (pickle_str_key("discovered"), pickle_float(r.discovered)),
                (pickle_str_key("last_heard"), pickle_float(r.last_heard)),
                (
                    pickle_str_key("heard_count"),
                    pickle_int(r.heard_count as i64),
                ),
                (pickle_str_key("status"), pickle_str(status.as_str())),
                (
                    pickle_str_key("status_code"),
                    pickle_int(status.code() as i64),
                ),
            ];
            // Type-specific fields present only when the record carries them,
            // matching Python (which sets keys conditionally per interface type).
            if let Some(v) = &r.ifac_netname {
                entries.push((pickle_str_key("ifac_netname"), pickle_str(v)));
            }
            if let Some(v) = &r.ifac_netkey {
                entries.push((pickle_str_key("ifac_netkey"), pickle_str(v)));
            }
            if let Some(v) = &r.reachable_on {
                entries.push((pickle_str_key("reachable_on"), pickle_str(v)));
            }
            if let Some(v) = r.port {
                entries.push((pickle_str_key("port"), pickle_int(v as i64)));
            }
            if let Some(v) = r.frequency {
                entries.push((pickle_str_key("frequency"), pickle_int(v as i64)));
            }
            if let Some(v) = r.bandwidth {
                entries.push((pickle_str_key("bandwidth"), pickle_int(v as i64)));
            }
            if let Some(v) = r.sf {
                entries.push((pickle_str_key("sf"), pickle_int(v as i64)));
            }
            if let Some(v) = r.cr {
                entries.push((pickle_str_key("cr"), pickle_int(v as i64)));
            }
            if let Some(v) = &r.config_entry {
                entries.push((pickle_str_key("config_entry"), pickle_str(v)));
            }
            pickle_dict(entries)
        })
        .collect();

    pickle_list(items)
}

/// A float pickle value or Python `None` for an absent optional.
fn opt_float(v: Option<f64>) -> Value {
    match v {
        Some(f) => pickle_float(f),
        None => pickle_none(),
    }
}

// Interface Stats (rnstatus)
/// Map a resolved announce-rate value to a pickle scalar: `Some(v)` -> int,
/// `None` -> Python `None` (Codeberg #67 Stage 2a).
fn ar_value(v: Option<u32>) -> Value {
    match v {
        Some(v) => pickle_int(v as i64),
        None => pickle_none(),
    }
}

/// An optional integer field: the value, or Python's `None` when the reference
/// interface has no such attribute at all.
fn opt_int(v: Option<i64>) -> Value {
    match v {
        Some(v) => pickle_int(v),
        None => pickle_none(),
    }
}

/// Build the RNode radio-stats key/value entries for one interface's
/// `interface_stats` dict (Codeberg #25).
///
/// Field names and units match Python `Reticulum.get_interface_stats`
/// (Reticulum.py:1371-1420) so rnstatus/lnstatus render the radio rows without
/// special-casing:
///   - `airtime_short`/`airtime_long`, `channel_load_short`/`channel_load_long`
///     -> float percent
///   - `noise_floor` -> int dBm, or `None`
///   - `cpu_temp` -> int Celsius (from `CMD_STAT_TEMP`), or `None`
///   - `battery_state` (string) / `battery_percent` (int) -> only once the
///     reported state leaves `Unknown` (Python emits these keys only when
///     `r_battery_state != 0x00`).
///   - `last_rssi` (int dBm) / `last_snr` (float dB) -> only once the device
///     has reported a received packet via `apply_radio_stat`
///     (interfaces/rnode.rs:758-766). These two are ours: Python does not
///     place them in the dict (its RSSI feeds per-packet reporting), so they
///     are emitted as additive keys and omitted while `None` — rnstatus
///     renders by key lookup (rnstatus.py:475-533) and ignores them.
fn radio_stat_fields(r: &crate::interfaces::RadioStats) -> Vec<(HashableValue, Value)> {
    let opt_int = |v: Option<i16>| match v {
        Some(x) => pickle_int(x as i64),
        None => pickle_none(),
    };
    let mut fields = vec![
        (
            pickle_str_key("airtime_short"),
            pickle_float(r.airtime_short),
        ),
        (pickle_str_key("airtime_long"), pickle_float(r.airtime_long)),
        (
            pickle_str_key("channel_load_short"),
            pickle_float(r.channel_load_short),
        ),
        (
            pickle_str_key("channel_load_long"),
            pickle_float(r.channel_load_long),
        ),
        (pickle_str_key("noise_floor"), opt_int(r.noise_floor)),
        (pickle_str_key("cpu_temp"), opt_int(r.cpu_temp)),
    ];
    if r.battery_state != leviculum_core::rnode::BatteryState::Unknown {
        fields.push((
            pickle_str_key("battery_state"),
            pickle_str(r.battery_state.as_str()),
        ));
        fields.push((
            pickle_str_key("battery_percent"),
            pickle_int(r.battery_percent as i64),
        ));
    }
    if let Some(rssi) = r.last_rssi {
        fields.push((pickle_str_key("last_rssi"), pickle_int(rssi as i64)));
    }
    if let Some(snr) = r.last_snr {
        fields.push((pickle_str_key("last_snr"), pickle_float(snr)));
    }
    fields
}

/// One row of the reported interface inventory.
///
/// Both row sources fill the same struct, so every row necessarily carries the
/// same key set: a listener reported out of the driver's inventory cannot
/// silently grow or lose a field relative to a routable interface reported out
/// of transport.
struct StatRow {
    /// Reporting order (see `InterfaceInventory::sort_key`).
    sort_key: (u8, usize),
    /// Python `str(interface)`.
    name: String,
    /// Python `interface.name`.
    short_name: String,
    /// Python `type(interface).__name__`.
    itype: String,
    rxb: u64,
    txb: u64,
    rxs: f64,
    txs: f64,
    /// Outgoing frames the interface's host-side send queue shed or
    /// abandoned (Codeberg #318). Reads 0 on a medium that holds no
    /// host-side queue. Doubles as the `txdrp` key, which upstream fills
    /// from `interface.tx_drops` (post-1.3.5; Reticulum 1.5.2,
    /// `get_interface_stats`).
    tx_queue_drops: u64,
    /// Payload bytes of those shed frames: the `txdrb` key (upstream
    /// `interface.tx_dropped_bytes`, same vintage).
    tx_dropped_bytes: u64,
    /// The `mtu` key (upstream `interface.HW_MTU`). `None` when the driver
    /// registered none — the reference base class also initialises
    /// `HW_MTU` (Interface.py:103) to `None`, and rnstatus renders it
    /// verbatim.
    mtu: Option<i64>,
    status: bool,
    mode: u8,
    bitrate: i64,
    clients: Option<i64>,
    peers: Option<i64>,
    incoming_announce_frequency: f64,
    outgoing_announce_frequency: f64,
    incoming_pr_frequency: f64,
    outgoing_pr_frequency: f64,
    announce_rate: (Option<u32>, Option<u32>, Option<u32>),
    burst_active: bool,
    burst_activated: u64,
    pr_burst_active: bool,
    pr_burst_activated: u64,
    held_announces: usize,
    ifac_size_bits: Option<i64>,
    /// Reported name of the listener this interface was spawned by, if any.
    parent_name: Option<String>,
    radio: Option<crate::interfaces::RadioStats>,
    /// Ceiling of the randomised delay this interface adds before it puts a
    /// frame on the air, in milliseconds; `None` for a medium that transmits
    /// as soon as it is asked (Codeberg #190).
    tx_jitter_max_ms: Option<u64>,
}

/// Serialise one row into the Python `interface_stats` per-interface dict.
fn row_fields(row: &StatRow, epoch_base: f64) -> Value {
    let mut fields = vec![
        (pickle_str_key("name"), pickle_str(&row.name)),
        (pickle_str_key("short_name"), pickle_str(&row.short_name)),
        (
            pickle_str_key("hash"),
            pickle_bytes(&compute_interface_hash(&row.name)),
        ),
        (pickle_str_key("type"), pickle_str(&row.itype)),
        (pickle_str_key("rxb"), pickle_int(row.rxb as i64)),
        (pickle_str_key("txb"), pickle_int(row.txb as i64)),
        (pickle_str_key("rxs"), pickle_float(row.rxs)),
        (pickle_str_key("txs"), pickle_float(row.txs)),
        // Codeberg #318: frames shed from the host-side send queue, the
        // counter behind RNODE_TX_QUEUE_DROP. No reference equivalent —
        // like `tx_jitter_max` below, an additive key the reference
        // reader tolerates: rnstatus looks every field up by name (text
        // path gated with `"key" in ifstat`, rnstatus.py:390-540) and
        // its --json path enumerates keys only to hex-encode bytes
        // values (rnstatus.py:345-357), so an unknown int key passes
        // through unread. Unconditional like rxb/txb — it reads from
        // the same per-interface counters struct, and 0 is the honest
        // value for a medium that cannot shed.
        (
            pickle_str_key("tx_queue_drops"),
            pickle_int(row.tx_queue_drops as i64),
        ),
        // The post-1.3.5 upstream key set. These keys exist nowhere in our
        // pinned 1.3.5 reference; the authoritative shape was measured on
        // Reticulum 1.5.2 (`get_interface_stats`, the version the nightly
        // containers run). Its rnstatus indexes several of them unguarded —
        // `txdrp` and, whenever `bitrate` is non-None (ours always is),
        // `mtu`, plus `txbuffered`, on the default path; every `--sort` key;
        // and the `arxc`/`atxc`/`prxc`/`ptxc` totals under `-a`/`-p` — so a
        // missing key is a KeyError crash, not a cosmetic gap.
        //
        // `txdrp`/`txdrb` carry our real shed-frame counters (the pair
        // behind RNODE_TX_QUEUE_DROP; upstream increments its
        // `tx_drops`/`tx_dropped_bytes` at its own send-buffer drop sites).
        // `mtu` carries the registered HW_MTU. The rest report the value a
        // 1.5.2 interface holds when the mechanism has never fired: we do
        // not run those mechanisms — no per-interface announce/path-request
        // byte or count ledger, no transmit buffer gauge, no violation or
        // filter counters, no switch gravity — so the never-fired constant
        // is the truthful report, and rnstatus degrades to the same output
        // it shows for an idle rnsd.
        (pickle_str_key("mtu"), opt_int(row.mtu)),
        (
            pickle_str_key("txdrp"),
            pickle_int(row.tx_queue_drops as i64),
        ),
        (
            pickle_str_key("txdrb"),
            pickle_int(row.tx_dropped_bytes as i64),
        ),
        (pickle_str_key("txstalled"), pickle_bool(false)),
        (pickle_str_key("txbuffered"), pickle_int(0)),
        (pickle_str_key("arxb"), pickle_int(0)),
        (pickle_str_key("atxb"), pickle_int(0)),
        (pickle_str_key("arxc"), pickle_int(0)),
        (pickle_str_key("atxc"), pickle_int(0)),
        (pickle_str_key("prxb"), pickle_int(0)),
        (pickle_str_key("ptxb"), pickle_int(0)),
        (pickle_str_key("prxc"), pickle_int(0)),
        (pickle_str_key("ptxc"), pickle_int(0)),
        (pickle_str_key("arxs"), pickle_float(0.0)),
        (pickle_str_key("atxs"), pickle_float(0.0)),
        (pickle_str_key("prxs"), pickle_float(0.0)),
        (pickle_str_key("ptxs"), pickle_float(0.0)),
        // Upstream's base class exposes `ic_burst_count`/`ic_pr_burst_count`
        // as properties returning None (1.5.2 Interface.py).
        (pickle_str_key("burst_count"), pickle_none()),
        (pickle_str_key("pr_burst_count"), pickle_none()),
        (pickle_str_key("gravity"), pickle_int(0)),
        (pickle_str_key("announces_to_internal"), pickle_none()),
        (pickle_str_key("protocol_violations"), pickle_int(0)),
        (pickle_str_key("ifac_violations"), pickle_int(0)),
        (pickle_str_key("packet_filter_hits"), pickle_int(0)),
        (pickle_str_key("autoconnect_source"), pickle_none()),
        // status: real `Interface::is_online()` (Codeberg #56). Source of
        // truth is the shared per-interface counters, whose online flag the
        // owning interface task flips at its connect boundaries (L-0020).
        // Missing entry → fall back to `true` (preserves the pre-fix
        // behavior for any caller-side mismatch).
        (pickle_str_key("status"), pickle_bool(row.status)),
        // mode: real Reticulum propagation mode (Codeberg #91), carried
        // per-interface by transport from the parsed config and reported
        // as the Python `Interface.MODE_*` value so rnstatus/lnstatus print
        // the right label (Utilities/rnstatus.py:421-427).
        (pickle_str_key("mode"), pickle_int(row.mode as i64)),
        (pickle_str_key("bitrate"), pickle_int(row.bitrate)),
        (pickle_str_key("clients"), opt_int(row.clients)),
        (pickle_str_key("peers"), opt_int(row.peers)),
        (
            pickle_str_key("incoming_announce_frequency"),
            pickle_float(row.incoming_announce_frequency),
        ),
        (
            pickle_str_key("outgoing_announce_frequency"),
            pickle_float(row.outgoing_announce_frequency),
        ),
        // Codeberg #67 Stage 2a: incoming/outgoing_pr_frequency are now real,
        // measured from per-interface path-request deques (Python
        // ip_freq_deque / op_freq_deque). They read 0.0 on an under-filled
        // deque, matching Interface.incoming_pr_frequency()/
        // outgoing_pr_frequency() (Interface.py:301-321).
        (
            pickle_str_key("incoming_pr_frequency"),
            pickle_float(row.incoming_pr_frequency),
        ),
        (
            pickle_str_key("outgoing_pr_frequency"),
            pickle_float(row.outgoing_pr_frequency),
        ),
        // Codeberg #67 Stage 2a: announce_rate_target/penalty/grace now carry
        // the real per-interface config (Reticulum.py:798-833). Unset keys
        // fall back to the Python interface defaults (target=3600 s,
        // penalty=0 s, grace=5) when transport is enabled, and stay None when
        // transport is disabled. rnstatus renders the `(t:.../p:.../g:...)`
        // suffix only when target is truthy (rnstatus.py:556-563).
        (
            pickle_str_key("announce_rate_target"),
            ar_value(row.announce_rate.0),
        ),
        (
            pickle_str_key("announce_rate_penalty"),
            ar_value(row.announce_rate.1),
        ),
        (
            pickle_str_key("announce_rate_grace"),
            ar_value(row.announce_rate.2),
        ),
        // burst_active/activated + pr_burst_active/activated: real ingress
        //   limiter burst state (Codeberg #87), read from the per-interface
        //   IngressBurstState (Python ic_burst_active/ic_burst_activated and
        //   ic_pr_burst_active/ic_pr_burst_activated, Interface.py:115-118).
        //   Idle interfaces read False / 0. rnstatus only reads *_activated
        //   when the matching *_active is truthy (rnstatus.py:565-573), so an
        //   idle interface renders no burst suffix.
        //   The core records activation on its monotonic clock; Python
        //   reports time.time() and rnstatus renders `now - activated` as
        //   the burst duration (rnstatus.py:566), so convert to epoch
        //   seconds here. Idle stays the int 0 (Python's initial value).
        (
            pickle_str_key("burst_active"),
            pickle_bool(row.burst_active),
        ),
        (
            pickle_str_key("burst_activated"),
            activation_to_epoch(epoch_base, row.burst_activated),
        ),
        (
            pickle_str_key("pr_burst_active"),
            pickle_bool(row.pr_burst_active),
        ),
        (
            pickle_str_key("pr_burst_activated"),
            activation_to_epoch(epoch_base, row.pr_burst_activated),
        ),
        (
            pickle_str_key("held_announces"),
            pickle_int(row.held_announces as i64),
        ),
        (pickle_str_key("announce_queue"), pickle_none()),
        (pickle_str_key("ifac_signature"), pickle_none()),
        (pickle_str_key("ifac_size"), opt_int(row.ifac_size_bits)),
        (pickle_str_key("ifac_netname"), pickle_none()),
    ];

    // Python emits the parent keys only for an interface that HAS a parent
    // (Reticulum.py:1342-1344), so a listener or a static interface carries
    // neither key. The hash is the parent's own `hash`, which is what makes
    // the link followable.
    if let Some(parent) = &row.parent_name {
        fields.push((pickle_str_key("parent_interface_name"), pickle_str(parent)));
        fields.push((
            pickle_str_key("parent_interface_hash"),
            pickle_bytes(&compute_interface_hash(parent)),
        ));
    }

    // Codeberg #25: RNode radio stats. Emitted only for radio interfaces
    // (Python gates each key on hasattr(interface, "r_*"), which is true
    // only for RNodeInterface).
    if let Some(r) = &row.radio {
        fields.extend(radio_stat_fields(r));
    }

    // Codeberg #190: `tx_jitter_max` — the ceiling of the randomised pre-TX
    // delay the interface draws against before a frame goes on the air.
    //
    // No reference equivalent: Python's RNodeInterface leaves medium access to
    // the RNode firmware's CSMA and holds no such attribute, so nothing in
    // `get_interface_stats` (Reticulum.py:1326-1470) reports it. Adding the key
    // is therefore an additive deviation, and a benign one: the reference
    // reader looks every field up by name (`ifstat["name"]`, rnstatus.py:391,
    // and `if "<key>" in ifstat` for the optional ones) and never enumerates
    // the dict — there is no `.keys()` or `.items()` over an interface entry
    // anywhere in rnstatus.py — so an unknown key is simply not read.
    //
    // Seconds as a float, because every other time-valued key in this dict is
    // seconds (announce_rate_target, burst_activated), and emitted only where
    // the concept applies, exactly as Python gates `airtime_short` and friends
    // on `hasattr`. A medium that transmits when asked carries no key at all.
    if let Some(ms) = row.tx_jitter_max_ms {
        fields.push((
            pickle_str_key("tx_jitter_max"),
            pickle_float(ms as f64 / 1000.0),
        ));
    }

    pickle_dict(fields)
}

/// Bitrate an interface reports when the config sets none: the per-medium
/// `BITRATE_GUESS` of the reference class (TCPInterface.py:452,
/// LocalInterface.py:431).
fn default_bitrate(itype: &str) -> i64 {
    if itype.starts_with("Local") {
        crate::interfaces::local::LOCAL_BITRATE
    } else {
        crate::interfaces::tcp::TCP_BITRATE_GUESS
    }
}

/// Build the `interface_stats` response dict matching Python's format.
/// `core` is mutable because frequency reads pop decayed samples, exactly
/// like Python's get_interface_stats (Python parity, Codeberg #67/#87).
///
/// The reported inventory is the union of two collections (Codeberg #177):
/// transport's routable interfaces, and the driver's listeners — the
/// shared-instance server and every configured server listener, which carry no
/// packets and therefore never enter transport. Python has one collection for
/// both (`RNS.Transport.interfaces`, Reticulum.py:1334), so reporting only the
/// routable half answered a monitoring query about a different world than the
/// one the daemon runs.
pub(crate) fn build_interface_stats(
    core: &mut StdNodeCore,
    start_time: std::time::Instant,
    iface_stats_map: &InterfaceStatsMap,
    inventory: &SharedInventory,
    auto_peer_count: usize,
) -> Value {
    let stats = core.interface_stats();
    let identity = core.identity();
    let transport_enabled = core.transport_config().enable_transport;
    let uptime = start_time.elapsed().as_secs_f64();
    let epoch_base = epoch_base_secs(start_time);
    let counters_map = iface_stats_map.lock_recover();
    let ifac_configs = core.clone_ifac_configs();
    let inv = inventory.lock_recover();

    let mut total_rxb: u64 = 0;
    let mut total_txb: u64 = 0;
    let mut total_rxs: f64 = 0.0;
    let mut total_txs: f64 = 0.0;

    // Aggregates a listener reports for the connections it spawned, keyed by
    // listener id: Python's parent counters (TCPInterface.py:306-308/327-329)
    // and `clients` (TCPInterface.py:496-497 / LocalInterface.py:463).
    #[derive(Default)]
    struct ChildAggregate {
        clients: i64,
        rxb: u64,
        txb: u64,
        rxs: f64,
        txs: f64,
        ia_freq: f64,
        oa_freq: f64,
        ip_freq: f64,
        op_freq: f64,
    }
    let mut aggregates: std::collections::BTreeMap<usize, ChildAggregate> =
        std::collections::BTreeMap::new();

    let mut rows: Vec<StatRow> = Vec::new();
    for entry in &stats {
        // Presentation identity: spawned interfaces carry the reference
        // display name their listener gave them; everything else falls back to
        // the driver's internal interface name.
        let identity_of = inv.identity(entry.id);
        let name = identity_of
            .map(|i| i.name.clone())
            .unwrap_or_else(|| entry.name.clone());
        let short = identity_of
            .map(|i| i.short_name.clone())
            .unwrap_or_else(|| short_name(&entry.name));
        let itype = identity_of
            .map(|i| i.type_name.to_string())
            .unwrap_or_else(|| interface_type(entry.kind, &entry.name));

        // Read byte counters and compute speeds from the shared counters
        let (rxb, txb, rxs, txs, tx_queue_drops, tx_dropped_bytes) = counters_map
            .get(&entry.id)
            .map(|c| {
                let (rxs, txs) = c.speeds();
                (
                    c.rx_bytes.load(Ordering::Relaxed),
                    c.tx_bytes.load(Ordering::Relaxed),
                    rxs,
                    txs,
                    c.tx_queue_drops.load(Ordering::Relaxed),
                    c.tx_dropped_bytes.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((0, 0, 0.0, 0.0, 0, 0));

        // Totals stay what they were: the traffic-bearing, non-local
        // interfaces. Local IPC clients and (below) listeners are excluded, so
        // adding their rows cannot double-count a byte.
        if !entry.is_local_client {
            total_rxb += rxb as u64;
            total_txb += txb as u64;
            total_rxs += rxs;
            total_txs += txs;
        }

        if let Some(parent) = identity_of.and_then(|i| i.parent) {
            let agg = aggregates.entry(parent).or_default();
            agg.clients += 1;
            agg.rxb += rxb as u64;
            agg.txb += txb as u64;
            agg.rxs += rxs;
            agg.txs += txs;
            agg.ia_freq += entry.incoming_announce_frequency;
            agg.oa_freq += entry.outgoing_announce_frequency;
            agg.ip_freq += entry.incoming_pr_frequency;
            agg.op_freq += entry.outgoing_pr_frequency;
        }

        // Codeberg #25: latest RNode radio stats (None for non-radio interfaces).
        let radio = counters_map.get(&entry.id).and_then(|c| c.radio_stats());

        // Bitrate reporting (Codeberg #93/#190). Python's precedence, in the
        // order it applies them: a configured `bitrate` overrides everything
        // (`if configured_bitrate: interface.bitrate = configured_bitrate`,
        // Reticulum.py:887); otherwise the interface's own rate for its medium,
        // which for a radio is the on-air bitrate it derives from its radio
        // settings (`RNodeInterface.updateBitrate`, RNodeInterface.py:693-696);
        // otherwise the per-medium BITRATE_GUESS. Reticulum.py:1421-1423 then
        // reports whichever survived as `bitrate`.
        //
        // The middle term used to be missing, so a radio row answered with the
        // TCP guess — a query about a different world than the one the daemon
        // runs, the shape of Codeberg #177.
        let bitrate = entry
            .configured_bitrate
            .or(entry.link_profile.map(|p| p.bitrate_bps))
            .map(|bps| bps as i64)
            .unwrap_or_else(|| default_bitrate(&itype));

        rows.push(StatRow {
            sort_key: inv.sort_key(entry.id),
            name,
            short_name: short,
            // Peers field: only meaningful for AutoInterface
            peers: (itype == "AutoInterface").then_some(auto_peer_count as i64),
            // A spawned connection reports no client count of its own; only a
            // listener does (filled from `aggregates` below).
            clients: None,
            itype,
            rxb: rxb as u64,
            txb: txb as u64,
            rxs,
            txs,
            tx_queue_drops: tx_queue_drops as u64,
            tx_dropped_bytes: tx_dropped_bytes as u64,
            mtu: entry.hw_mtu.map(|m| m as i64),
            // Real `Interface::is_online()` (Codeberg #56): the shared
            // counters carry the live state the interface task flips at
            // its connect boundaries (L-0020). Missing entry -> online,
            // preserving the pre-#56 fallback.
            status: counters_map
                .get(&entry.id)
                .map(|c| c.is_online())
                .unwrap_or(true),
            mode: entry.mode.as_u8(),
            bitrate,
            incoming_announce_frequency: entry.incoming_announce_frequency,
            outgoing_announce_frequency: entry.outgoing_announce_frequency,
            incoming_pr_frequency: entry.incoming_pr_frequency,
            outgoing_pr_frequency: entry.outgoing_pr_frequency,
            announce_rate: (
                entry.announce_rate_target,
                entry.announce_rate_penalty,
                entry.announce_rate_grace,
            ),
            burst_active: entry.burst_active,
            burst_activated: entry.burst_activated,
            pr_burst_active: entry.pr_burst_active,
            pr_burst_activated: entry.pr_burst_activated,
            held_announces: entry.held_announces,
            ifac_size_bits: ifac_configs
                .get(&entry.id)
                .map(|cfg| (cfg.ifac_size() * 8) as i64),
            parent_name: identity_of
                .and_then(|i| i.parent)
                .and_then(|p| inv.listeners().find(|(id, _)| *id == p))
                .map(|(_, l)| l.identity.name.clone()),
            radio,
            tx_jitter_max_ms: entry.link_profile.and_then(|p| p.tx_jitter_max_ms),
        });
    }

    // Listener rows. They carry no packets themselves, so every traffic value
    // is the aggregate of the connections they spawned, plus what already
    // departed. Burst/held state stays empty because the reference keeps it on
    // the spawned interface: a listener only collects frequency samples
    // (TCPInterface.py:634-644, LocalInterface.py:484-494).
    for (id, listener) in inv.listeners() {
        let agg = aggregates.remove(&id).unwrap_or_default();
        rows.push(StatRow {
            sort_key: inv.sort_key(id),
            name: listener.identity.name.clone(),
            short_name: listener.identity.short_name.clone(),
            itype: listener.identity.type_name.to_string(),
            rxb: listener.departed_rxb + agg.rxb,
            txb: listener.departed_txb + agg.txb,
            rxs: agg.rxs,
            txs: agg.txs,
            // A listener carries no packets, so it holds no send queue
            // that could shed one.
            tx_queue_drops: 0,
            tx_dropped_bytes: 0,
            mtu: Some(listener.hw_mtu),
            status: true,
            mode: listener.mode.as_u8(),
            bitrate: listener.bitrate,
            clients: Some(agg.clients),
            peers: None,
            incoming_announce_frequency: agg.ia_freq,
            outgoing_announce_frequency: agg.oa_freq,
            incoming_pr_frequency: agg.ip_freq,
            outgoing_pr_frequency: agg.op_freq,
            announce_rate: listener.announce_rate,
            burst_active: false,
            burst_activated: 0,
            pr_burst_active: false,
            pr_burst_activated: 0,
            held_announces: 0,
            ifac_size_bits: listener.ifac_size_bits,
            parent_name: None,
            radio: None,
            // A listener carries no packets, so it has no medium access of its
            // own to bound; the spawned connection is the row that would.
            tx_jitter_max_ms: None,
        });
    }

    rows.sort_by_key(|r| r.sort_key);
    let iface_list: Vec<Value> = rows.iter().map(|r| row_fields(r, epoch_base)).collect();

    let mut entries = vec![
        (pickle_str_key("interfaces"), pickle_list(iface_list)),
        (pickle_str_key("rxb"), pickle_int(total_rxb as i64)),
        (pickle_str_key("txb"), pickle_int(total_txb as i64)),
        (pickle_str_key("rxs"), pickle_float(total_rxs)),
        (pickle_str_key("txs"), pickle_float(total_txs)),
        (pickle_str_key("rss"), pickle_none()),
        // Top-level keys upstream 1.5.2 emits unconditionally (same
        // `get_interface_stats` producer as the per-interface set; all
        // post-1.3.5). Its `rnstatus --pps` reads rxpps/txpps and
        // `--queues` reads every rxq*/*pressure key unguarded; the
        // announce/path-request totals are presence-guarded but complete
        // the drop-in shape. As above, we keep no announce/PR traffic
        // ledger and no partitioned inbound queues, so the never-fired
        // constants are the truthful report.
        (pickle_str_key("arxb"), pickle_int(0)),
        (pickle_str_key("atxb"), pickle_int(0)),
        (pickle_str_key("arxs"), pickle_float(0.0)),
        (pickle_str_key("atxs"), pickle_float(0.0)),
        (pickle_str_key("arxf"), pickle_float(0.0)),
        (pickle_str_key("atxf"), pickle_float(0.0)),
        (pickle_str_key("prxb"), pickle_int(0)),
        (pickle_str_key("ptxb"), pickle_int(0)),
        (pickle_str_key("prxs"), pickle_float(0.0)),
        (pickle_str_key("ptxs"), pickle_float(0.0)),
        (pickle_str_key("prxf"), pickle_float(0.0)),
        (pickle_str_key("ptxf"), pickle_float(0.0)),
        // Upstream rounds its `rx_pps`/`tx_pps` to int before storing.
        (pickle_str_key("rxpps"), pickle_int(0)),
        (pickle_str_key("txpps"), pickle_int(0)),
        (pickle_str_key("rxqt"), pickle_int(0)),
        (pickle_str_key("rxqd"), pickle_int(0)),
        (pickle_str_key("rxqa"), pickle_int(0)),
        (pickle_str_key("rxqp"), pickle_int(0)),
        (pickle_str_key("rxqil"), pickle_int(0)),
        (pickle_str_key("rxqtd"), pickle_int(0)),
        (pickle_str_key("rxqdd"), pickle_int(0)),
        (pickle_str_key("rxqad"), pickle_int(0)),
        (pickle_str_key("rxqpd"), pickle_int(0)),
        (pickle_str_key("rxqild"), pickle_int(0)),
        (pickle_str_key("tqpressure"), pickle_float(0.0)),
        (pickle_str_key("dqpressure"), pickle_float(0.0)),
        (pickle_str_key("aqpressure"), pickle_float(0.0)),
        (pickle_str_key("pqpressure"), pickle_float(0.0)),
        (pickle_str_key("ilqpressure"), pickle_float(0.0)),
        (pickle_str_key("txq"), pickle_none()),
    ];

    if transport_enabled {
        entries.push((
            pickle_str_key("transport_id"),
            pickle_bytes(identity.hash()),
        ));
        entries.push((pickle_str_key("transport_uptime"), pickle_float(uptime)));
        let probe_value = match core.probe_dest_hash() {
            Some(hash) => pickle_bytes(hash.as_bytes()),
            None => pickle_none(),
        };
        entries.push((pickle_str_key("probe_responder"), probe_value));
        entries.push((pickle_str_key("network_id"), pickle_none()));
    }

    pickle_dict(entries)
}

// Path Table (rnpath -t)
/// Build the path table response. Timestamps are converted from monotonic core
/// milliseconds to approximate Unix epoch seconds using the start_time anchor.
fn build_path_table(
    core: &StdNodeCore,
    start_time: std::time::Instant,
    max_hops: Option<i64>,
) -> Value {
    let entries = core.path_table_entries();

    // Anchor: wall clock at start_time
    let epoch_base = epoch_base_secs(start_time);
    let now_mono_ms = core.now_ms();

    let mut list = Vec::new();
    for entry in &entries {
        // Hops are now incremented on receipt (matching Python semantics)
        let python_hops = entry.hops as i64;
        if let Some(max) = max_hops {
            if python_hops > max {
                continue;
            }
        }

        // Name lookup only; interface_stats() would pop frequency samples.
        let iface_name = core
            .interface_name(entry.interface_index)
            .unwrap_or("unknown");

        // Local receipt time, back-computed from expires - the lifetime THIS
        // path was given (which depends on the receiving interface's mode, so
        // the flat configured default is wrong for access-point and roaming
        // paths). See `PathTableExport::timestamp_ms`.
        let timestamp_secs = mono_ms_to_epoch(epoch_base, now_mono_ms, entry.timestamp_ms);
        let expires_secs = mono_ms_to_epoch(epoch_base, now_mono_ms, entry.expires_ms);

        let dict = pickle_dict(vec![
            (pickle_str_key("hash"), pickle_bytes(&entry.hash)),
            (pickle_str_key("timestamp"), pickle_float(timestamp_secs)),
            (
                pickle_str_key("via"),
                match &entry.next_hop {
                    // Relayed: next_hop is the relay's transport ID
                    Some(h) => pickle_bytes(h),
                    // Direct: Python uses the destination hash as received_from
                    // (Transport.py:1600), never None, rnpath crashes on None.
                    None => pickle_bytes(&entry.hash),
                },
            ),
            (pickle_str_key("hops"), pickle_int(python_hops)),
            (pickle_str_key("expires"), pickle_float(expires_secs)),
            (pickle_str_key("interface"), pickle_str(iface_name)),
        ]);
        list.push(dict);
    }
    pickle_list(list)
}

// Link Table (Leviculum-only `link_table` RPC — `lnstest diag` v2)
/// Build the `link_table` response — a list of per-link dicts.
///
/// One entry per local [`crate::link::Link`] regardless of state. Python
/// `rnsd` has no `link_table` precedent (it exposes `link_count` only); the
/// response shape is therefore a Leviculum extension, kept to
/// pickle-friendly scalars so Python clients can still deserialise it into
/// a list of dicts if they ever consume it.
fn build_link_table(core: &StdNodeCore) -> Value {
    let entries = core.link_table_entries();

    let mut list = Vec::new();
    for entry in &entries {
        // Name lookup only; interface_stats() would pop frequency samples.
        let iface_name = entry
            .interface_index
            .and_then(|idx| core.interface_name(idx))
            .unwrap_or("");
        let dict = pickle_dict(vec![
            (pickle_str_key("link_id"), pickle_bytes(&entry.link_id)),
            (pickle_str_key("state"), pickle_str(entry.state)),
            (
                pickle_str_key("destination_hash"),
                pickle_bytes(&entry.destination_hash),
            ),
            (
                pickle_str_key("age"),
                match entry.age_secs {
                    Some(s) => pickle_float(s as f64),
                    None => pickle_none(),
                },
            ),
            (pickle_str_key("interface"), pickle_str(iface_name)),
        ]);
        list.push(dict);
    }
    pickle_list(list)
}

// Transport tables (Codeberg #174 — `lnstatus -j --tables`)
//
// One snapshot of every table the transport maintains, taken under a single
// RPC call so the tables are mutually consistent. Python `rnsd` serves no such
// command and answers by closing the connection, which is what makes absence
// distinguishable from emptiness on the reading side: an absent
// `transport_tables` key means the daemon does not know the question, an
// empty list means it does and the table is empty.
//
// Reference check, per table:
//
//   * `path_table` — the ONE table Python serves over RPC
//     (`Reticulum.get_path_table`, Reticulum.py:1516-1538). Its six keys
//     (`hash`, `timestamp`, `via`, `hops`, `expires`, `interface`) and their
//     units (Unix seconds as floats) are taken verbatim, so a row here is a
//     row of that RPC's response plus `announce_emitted`.
//   * `reverse_table`, `link_table`, `announce_table`, `tunnels` — Python
//     holds all four but exposes none of them. The reference names them only
//     by list index (`IDX_RT_*`, `IDX_LT_*`, `IDX_AT_*`, `IDX_TT_*`,
//     Transport.py:3556-3586); the string keys here are ours, spelled after
//     those constants.
//   * `announce_cache` — our cache of the last announce per known
//     destination, whose retain/recency semantics mirror
//     `Identity.known_destinations[dest][4]` (Codeberg #84). No reference
//     RPC.
//   * `local_links` — the links this node TERMINATES; identical rows to the
//     pre-existing `link_table` RPC. Note the trap: that RPC's `link_table`
//     and this dump's `link_table` are different tables. Here the reference
//     name wins, because `Transport.link_table` is the reference's own name
//     for the relay table, and the local inventory (which the reference has
//     no table for at all) is the one that gets the qualified name.
//
// The additive keys are safe against a Python reader for the same reason
// `tx_jitter_max` is (Codeberg #190): every Python consumer of an RPC
// response reads it by name and none enumerates it, so an unknown key is
// simply never read.
fn build_transport_tables(core: &StdNodeCore, start_time: std::time::Instant) -> Value {
    let epoch_base = epoch_base_secs(start_time);
    let now_mono_ms = core.now_ms();
    let to_epoch = |mono_ms: u64| pickle_float(mono_ms_to_epoch(epoch_base, now_mono_ms, mono_ms));
    // Name lookup only; interface_stats() would pop frequency samples.
    let iface = |idx: usize| pickle_str(core.interface_name(idx).unwrap_or("unknown"));
    let opt_iface = |idx: Option<usize>| match idx {
        Some(i) => iface(i),
        None => pickle_none(),
    };

    let path_table = core
        .path_table_entries()
        .iter()
        .map(|e| {
            pickle_dict(vec![
                (pickle_str_key("hash"), pickle_bytes(&e.hash)),
                (pickle_str_key("timestamp"), to_epoch(e.timestamp_ms)),
                (
                    pickle_str_key("via"),
                    // Direct: Python uses the destination hash as received_from
                    // (Transport.py:1600), never None.
                    pickle_bytes(e.next_hop.as_ref().unwrap_or(&e.hash)),
                ),
                (pickle_str_key("hops"), pickle_int(e.hops as i64)),
                (pickle_str_key("expires"), to_epoch(e.expires_ms)),
                (pickle_str_key("interface"), iface(e.interface_index)),
                (
                    pickle_str_key("announce_emitted"),
                    pickle_int(e.announce_emitted_secs as i64),
                ),
            ])
        })
        .collect();

    let reverse_table = core
        .reverse_table_entries()
        .iter()
        .map(|e| {
            pickle_dict(vec![
                (pickle_str_key("hash"), pickle_bytes(&e.hash)),
                (
                    pickle_str_key("receiving_interface"),
                    iface(e.receiving_interface_index),
                ),
                (
                    pickle_str_key("outbound_interface"),
                    iface(e.outbound_interface_index),
                ),
                (pickle_str_key("timestamp"), to_epoch(e.timestamp_ms)),
            ])
        })
        .collect();

    let link_table = core
        .transport_link_table_entries()
        .iter()
        .map(|e| {
            pickle_dict(vec![
                (pickle_str_key("link_id"), pickle_bytes(&e.link_id)),
                (pickle_str_key("timestamp"), to_epoch(e.timestamp_ms)),
                (
                    pickle_str_key("next_hop_interface"),
                    iface(e.next_hop_interface_index),
                ),
                (
                    pickle_str_key("remaining_hops"),
                    pickle_int(e.remaining_hops as i64),
                ),
                (
                    pickle_str_key("receiving_interface"),
                    iface(e.received_interface_index),
                ),
                (pickle_str_key("hops"), pickle_int(e.hops as i64)),
                (
                    pickle_str_key("destination_hash"),
                    pickle_bytes(&e.destination_hash),
                ),
                (pickle_str_key("validated"), pickle_bool(e.validated)),
                (
                    pickle_str_key("proof_timeout"),
                    to_epoch(e.proof_timeout_ms),
                ),
            ])
        })
        .collect();

    let announce_table = core
        .announce_table_entries()
        .iter()
        .map(|e| {
            pickle_dict(vec![
                (pickle_str_key("hash"), pickle_bytes(&e.hash)),
                (pickle_str_key("timestamp"), to_epoch(e.timestamp_ms)),
                (
                    pickle_str_key("retransmit_timeout"),
                    e.retransmit_at_ms.map(to_epoch).unwrap_or_else(pickle_none),
                ),
                (pickle_str_key("retries"), pickle_int(e.retries as i64)),
                (
                    pickle_str_key("receiving_interface"),
                    iface(e.receiving_interface_index),
                ),
                (pickle_str_key("hops"), pickle_int(e.hops as i64)),
                (
                    pickle_str_key("packet_length"),
                    pickle_int(e.packet_length as i64),
                ),
                (
                    pickle_str_key("local_rebroadcasts"),
                    pickle_int(e.local_rebroadcasts as i64),
                ),
                (
                    pickle_str_key("block_rebroadcasts"),
                    pickle_bool(e.block_rebroadcasts),
                ),
                (
                    pickle_str_key("attached_interface"),
                    opt_iface(e.target_interface_index),
                ),
            ])
        })
        .collect();

    let announce_cache = core
        .announce_cache_entries()
        .iter()
        .map(|e| {
            pickle_dict(vec![
                (pickle_str_key("hash"), pickle_bytes(&e.hash)),
                (
                    pickle_str_key("packet_length"),
                    pickle_int(e.packet_length as i64),
                ),
                (pickle_str_key("retained"), pickle_bool(e.retained)),
                (
                    pickle_str_key("last_used"),
                    e.last_used_ms.map(to_epoch).unwrap_or_else(pickle_none),
                ),
            ])
        })
        .collect();

    let tunnels = core
        .tunnel_table_entries()
        .iter()
        .map(|t| {
            let paths = t
                .paths
                .iter()
                .map(|p| {
                    pickle_dict(vec![
                        (pickle_str_key("hash"), pickle_bytes(&p.hash)),
                        (pickle_str_key("hops"), pickle_int(p.hops as i64)),
                        (
                            pickle_str_key("via"),
                            pickle_bytes(p.next_hop.as_ref().unwrap_or(&p.hash)),
                        ),
                        (pickle_str_key("expires"), to_epoch(p.expires_ms)),
                        (pickle_str_key("timestamp"), to_epoch(p.timestamp_ms)),
                        (
                            pickle_str_key("announce_emitted"),
                            pickle_int(p.announce_emitted_secs as i64),
                        ),
                    ])
                })
                .collect();
            pickle_dict(vec![
                (pickle_str_key("tunnel_id"), pickle_bytes(&t.tunnel_id)),
                (pickle_str_key("interface"), opt_iface(t.interface_index)),
                (pickle_str_key("expires"), to_epoch(t.expires_ms)),
                (pickle_str_key("paths"), pickle_list(paths)),
            ])
        })
        .collect();

    pickle_dict(vec![
        (pickle_str_key("path_table"), pickle_list(path_table)),
        (pickle_str_key("reverse_table"), pickle_list(reverse_table)),
        (pickle_str_key("link_table"), pickle_list(link_table)),
        (
            pickle_str_key("announce_table"),
            pickle_list(announce_table),
        ),
        (
            pickle_str_key("announce_cache"),
            pickle_list(announce_cache),
        ),
        (pickle_str_key("tunnels"), pickle_list(tunnels)),
        (pickle_str_key("local_links"), build_link_table(core)),
    ])
}

// Identity listing (lnstatus --identities) — a Leviculum-only verb; see
// `RpcRequest::GetIdentityTable` for the unknown-daemon behaviour. Response is
// a list of dicts, one per identity learned from a received announce:
//
//   * `identity_hash` (16 bytes) — truncated hash of the announced public key,
//     the input every destination derivation starts from.
//   * `destination_hash` (16 bytes) — the destination the announce named.
//   * `name` (str or None) — the dotted destination name, only when the
//     announce's name hash matches an aspect this daemon registered itself;
//     None for every other foreign announce.
//   * `hops` (int or None), `interface` (str or None), `via` (16 bytes or
//     None, the next relay hop), `last_seen` (Unix seconds float or None) —
//     the live path toward the destination; all None when no path exists.
//
// Unlike the reference-shaped tables, `via` here is honestly None for a direct
// path instead of repeating the destination hash: this listing has no Python
// consumer to stay byte-compatible with, and the client renders direct as the
// bare interface name.
fn build_identity_table(core: &StdNodeCore, start_time: std::time::Instant) -> Value {
    let epoch_base = epoch_base_secs(start_time);
    let now_mono_ms = core.now_ms();
    let to_epoch = |mono_ms: u64| pickle_float(mono_ms_to_epoch(epoch_base, now_mono_ms, mono_ms));

    let rows = core
        .identity_table_entries()
        .iter()
        .map(|e| {
            pickle_dict(vec![
                (
                    pickle_str_key("identity_hash"),
                    pickle_bytes(&e.identity_hash),
                ),
                (
                    pickle_str_key("destination_hash"),
                    pickle_bytes(&e.destination_hash),
                ),
                (
                    pickle_str_key("name"),
                    match &e.name {
                        Some(n) => pickle_str(n),
                        None => pickle_none(),
                    },
                ),
                (
                    pickle_str_key("hops"),
                    match e.hops {
                        Some(h) => pickle_int(h as i64),
                        None => pickle_none(),
                    },
                ),
                (
                    pickle_str_key("interface"),
                    match e.interface_index {
                        // Name lookup only; interface_stats() would pop
                        // frequency samples.
                        Some(i) => pickle_str(core.interface_name(i).unwrap_or("unknown")),
                        None => pickle_none(),
                    },
                ),
                (
                    pickle_str_key("via"),
                    match &e.next_hop {
                        Some(h) => pickle_bytes(h),
                        None => pickle_none(),
                    },
                ),
                (
                    pickle_str_key("last_seen"),
                    e.last_seen_ms.map(to_epoch).unwrap_or_else(pickle_none),
                ),
            ])
        })
        .collect();
    pickle_list(rows)
}

// Rate Table (rnpath -r)
fn build_rate_table(core: &StdNodeCore, start_time: std::time::Instant) -> Value {
    let entries = core.rate_table_entries();
    let epoch_base = epoch_base_secs(start_time);
    let now_mono_ms = core.now_ms();

    let mut list = Vec::new();
    for entry in &entries {
        let last_secs = mono_ms_to_epoch(epoch_base, now_mono_ms, entry.last_ms);
        let blocked_until_secs = if entry.blocked_until_ms > 0 {
            pickle_float(mono_ms_to_epoch(
                epoch_base,
                now_mono_ms,
                entry.blocked_until_ms,
            ))
        } else {
            pickle_float(0.0)
        };

        let dict = pickle_dict(vec![
            (pickle_str_key("hash"), pickle_bytes(&entry.hash)),
            (pickle_str_key("last"), pickle_float(last_secs)),
            (
                pickle_str_key("rate_violations"),
                pickle_int(entry.rate_violations as i64),
            ),
            (pickle_str_key("blocked_until"), blocked_until_secs),
            (pickle_str_key("timestamps"), pickle_list(vec![])),
        ]);
        list.push(dict);
    }
    pickle_list(list)
}

/// Compute Python's `Transport.first_hop_timeout` in seconds.
///
/// Python (Transport.py:2700-2703):
/// ```text
/// latency = next_hop_per_byte_latency(dest)   # = 8 / bitrate  (s/byte)
/// if latency != None: return MTU * latency + DEFAULT_PER_HOP_TIMEOUT
/// else:               return DEFAULT_PER_HOP_TIMEOUT
/// ```
///
/// `bitrate_bps` is the next-hop interface bitrate for the destination, or
/// `None` when the path (or its interface bitrate) is unknown; in that case
/// Python returns the flat per-hop default. Note the reference formula scales
/// with the next-hop bitrate, not the hop count.
fn first_hop_timeout_secs(bitrate_bps: Option<u32>) -> f64 {
    let per_hop = DEFAULT_PER_HOP_TIMEOUT as f64;
    match bitrate_bps {
        Some(bitrate) if bitrate > 0 => {
            let per_byte_latency = 8.0 / bitrate as f64;
            MTU as f64 * per_byte_latency + per_hop
        }
        _ => per_hop,
    }
}

/// The bitrate of the slowest currently online interface, in bits per second,
/// or `None` when no online interface reports one.
///
/// Python keeps this as a cached class attribute, recomputed whenever the
/// interface list changes (Reticulum 1.5.2 `Transport.prioritize_interfaces`):
///
/// ```text
/// Transport.lowest_interface_bitrate = min(
///     interface.bitrate for interface in Transport.interfaces
///     if interface.online and interface.bitrate)
/// ```
///
/// Two filters, both reproduced here: `interface.online` drops interfaces the
/// driver has marked down, and the truthiness test on `interface.bitrate`
/// drops a `None`/`0` rate rather than letting it win the `min`. An empty
/// generator raises inside Python's `try`, which is how the attribute stays
/// `None` on a daemon with no online interface; we return `None` outright,
/// which is the same answer without the stale-value window Python's
/// keep-the-old-value-on-exception arm leaves open.
///
/// The bitrate of each interface is resolved with the SAME precedence
/// `build_interface_stats` reports in its `bitrate` key — configured overrides
/// the interface's own rate, which overrides the per-medium `BITRATE_GUESS` —
/// because a client that reads both must not see them disagree. The inventory
/// listeners are included for the same reason they appear as rows there
/// (Codeberg #177): Python holds listeners and routable interfaces in one
/// `Transport.interfaces` list, and its `min` sees all of them.
fn lowest_online_bitrate(
    core: &StdNodeCore,
    iface_stats_map: &InterfaceStatsMap,
    inventory: &SharedInventory,
) -> Option<i64> {
    let counters_map = iface_stats_map.lock_recover();
    let inv = inventory.lock_recover();

    let mut bitrates: Vec<i64> = Vec::new();
    for entry in core.interface_bitrate_entries() {
        // Missing entry -> online, matching the `status` key's fallback.
        if !counters_map
            .get(&entry.id)
            .map(|c| c.is_online())
            .unwrap_or(true)
        {
            continue;
        }
        let itype = inv
            .identity(entry.id)
            .map(|i| i.type_name.to_string())
            .unwrap_or_else(|| interface_type(entry.kind, &entry.name));
        bitrates.push(
            entry
                .configured_bitrate
                .or(entry.link_profile_bitrate)
                .map(|bps| bps as i64)
                .unwrap_or_else(|| default_bitrate(&itype)),
        );
    }
    // Listeners report `status: true` unconditionally in the stats rows, so
    // they are all online here too.
    bitrates.extend(inv.listeners().map(|(_, listener)| listener.bitrate));

    bitrates.into_iter().filter(|bps| *bps > 0).min()
}

/// Compute Python's `Transport.medium_path_timeout` in seconds.
///
/// Python (Reticulum 1.5.2 `Transport.medium_path_timeout`):
/// ```text
/// # A full round trip for an MTU on the slowest online interface
/// if not Transport.lowest_interface_bitrate: return 0
/// return 2*(RNS.Reticulum.MTU*8/max(Transport.lowest_interface_bitrate,
///                                   RNS.Reticulum.MINIMUM_BITRATE)) \
///        + RNS.Reticulum.DEFAULT_PER_HOP_TIMEOUT
/// ```
///
/// Constants, all from Reticulum 1.5.2 and all already pinned in
/// `leviculum_core::constants`: `MTU = 500` (Reticulum.py:93),
/// `MINIMUM_BITRATE = 5` (Reticulum.py:134), `DEFAULT_PER_HOP_TIMEOUT = 6`
/// (Reticulum.py:142).
///
/// `None` (and a non-positive rate, Python's falsy guard) yields 0 — the
/// documented "timeout is unknown" answer, not a fabricated window.
fn medium_path_timeout_secs(lowest_bitrate_bps: Option<i64>) -> f64 {
    match lowest_bitrate_bps {
        Some(bps) if bps > 0 => {
            let floored = bps.max(MINIMUM_BITRATE as i64) as f64;
            2.0 * (MTU as f64 * 8.0 / floored) + DEFAULT_PER_HOP_TIMEOUT as f64
        }
        _ => 0.0,
    }
}

// Path Lookups (rnpath)
fn get_next_hop(core: &StdNodeCore, destination_hash: &[u8]) -> Value {
    let hash = match try_into_hash(destination_hash) {
        Some(h) => h,
        None => return pickle_none(),
    };
    match core.get_path_clone(&hash) {
        Some(entry) => match &entry.next_hop {
            Some(h) => pickle_bytes(h),
            // Direct path: Python returns destination_hash (Transport.py:1600)
            None => pickle_bytes(&hash),
        },
        None => pickle_none(),
    }
}

fn get_next_hop_if_name(core: &StdNodeCore, destination_hash: &[u8]) -> Value {
    let hash = match try_into_hash(destination_hash) {
        Some(h) => h,
        None => return pickle_str("unknown"),
    };
    match core.get_path_clone(&hash) {
        Some(entry) => {
            // Name lookup only; interface_stats() would pop frequency samples.
            let iface_name = core
                .interface_name(entry.interface_index)
                .unwrap_or("unknown");
            pickle_str(iface_name)
        }
        None => pickle_str("unknown"),
    }
}

// Drop Operations
fn drop_path(core: &mut StdNodeCore, destination_hash: &[u8]) -> Value {
    let hash = match try_into_hash(destination_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    pickle_bool(core.remove_path(&hash))
}

fn drop_all_via(core: &mut StdNodeCore, via_hash: &[u8]) -> Value {
    let hash = match try_into_hash(via_hash) {
        Some(h) => h,
        None => return pickle_int(0),
    };
    pickle_int(core.drop_all_paths_via(&hash) as i64)
}

// Blackhole set (Codeberg #67)
/// Build the `blackholed_identities` response: a dict keyed by identity hash
/// (bytes) mapping to an entry dict `{"source", "until", "reason"}`, matching
/// Python's `RNS.Transport.blackholed_identities` (Transport.py:3420). The empty
/// case is an empty dict, exactly as the prior stub returned.
fn build_blackholed_identities(core: &StdNodeCore) -> Value {
    use serde_pickle::value::HashableValue;
    let entries = core
        .blackholed_identities()
        .iter()
        .map(|(hash, entry)| {
            let value = pickle_dict(vec![
                (pickle_str_key("source"), pickle_bytes(&entry.source)),
                (
                    pickle_str_key("until"),
                    entry.until.map(pickle_float).unwrap_or_else(pickle_none),
                ),
                (
                    pickle_str_key("reason"),
                    entry
                        .reason
                        .as_deref()
                        .map(pickle_str)
                        .unwrap_or_else(pickle_none),
                ),
            ]);
            (HashableValue::Bytes(hash.to_vec()), value)
        })
        .collect();
    Value::Dict(entries)
}

/// Insert into the blackhole set. Returns bool `true` on a fresh blackhole and
/// `None` when the identity was already present, mirroring Python's
/// `Transport.blackhole_identity` (Transport.py:3409); the `True`/`None`
/// returns are at Transport.py:3425,3427. An invalid hash
/// length yields `false`, matching the client-side length guard
/// (Reticulum.py:1723).
fn blackhole_identity(
    core: &mut StdNodeCore,
    identity_hash: &[u8],
    until: Option<f64>,
    reason: Option<String>,
) -> Value {
    let hash = match try_into_hash(identity_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    if core.blackhole_identity(hash, until, reason) {
        pickle_bool(true)
    } else {
        pickle_none()
    }
}

/// Remove from the blackhole set. Returns bool `true` when an entry was lifted
/// and `None` when the identity was not blackholed, mirroring Python's
/// `Transport.unblackhole_identity` (Transport.py:3434); the `True`/`None`
/// returns are at Transport.py:3446,3448.
fn unblackhole_identity(core: &mut StdNodeCore, identity_hash: &[u8]) -> Value {
    let hash = match try_into_hash(identity_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    if core.unblackhole_identity(&hash) {
        pickle_bool(true)
    } else {
        pickle_none()
    }
}

/// Membership check. Returns a bool, matching `identity_hash in
/// RNS.Transport.blackholed_identities` (Reticulum.py:1720).
fn is_blackholed(core: &StdNodeCore, identity_hash: &[u8]) -> Value {
    let hash = match try_into_hash(identity_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    pickle_bool(core.is_blackholed(&hash))
}

// Known-destination cache lifecycle (Codeberg #84)
/// Recency touch. Returns bool, mirroring Python
/// `Identity._used_destination_data`: true only when the destination is known
/// and not retained, false otherwise (including an invalid-length hash).
fn destination_data_used(core: &mut StdNodeCore, destination_hash: &[u8]) -> Value {
    let hash = match try_into_hash(destination_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    pickle_bool(core.used_destination_data(&hash))
}

/// Pin a known destination against cache eviction. Returns bool, mirroring
/// Python `Identity._retain_destination_data`.
fn destination_data_retain(core: &mut StdNodeCore, destination_hash: &[u8]) -> Value {
    let hash = match try_into_hash(destination_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    pickle_bool(core.retain_destination_data(&hash))
}

/// Lift a destination's retain pin. Returns bool, mirroring Python
/// `Identity._unretain_destination_data`.
fn destination_data_unretain(core: &mut StdNodeCore, destination_hash: &[u8]) -> Value {
    let hash = match try_into_hash(destination_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    pickle_bool(core.unretain_destination_data(&hash))
}

/// Retain every known destination for an identity. Returns bool, mirroring
/// Python `Identity._retain_identity` (true iff at least one was retained).
fn identity_data_retain(core: &mut StdNodeCore, identity_hash: &[u8]) -> Value {
    let hash = match try_into_hash(identity_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    pickle_bool(core.retain_identity_data(&hash))
}

/// Lift the retain pin on every known destination for an identity. Returns
/// bool (symmetric counterpart to [`identity_data_retain`]).
fn identity_data_unretain(core: &mut StdNodeCore, identity_hash: &[u8]) -> Value {
    let hash = match try_into_hash(identity_hash) {
        Some(h) => h,
        None => return pickle_bool(false),
    };
    pickle_bool(core.unretain_identity_data(&hash))
}

// Helpers
/// Interface hash: SHA-256 of the interface's reported name, which is exactly
/// Python `Interface.get_hash()` = `Identity.full_hash(str(interface))`
/// (Interface.py). Full 32 bytes, not truncated: the hash is how a script
/// keys an interface (and how `parent_interface_hash` points at its listener),
/// so it has to be the same value a Python `rnsd` reports for the same
/// interface (Codeberg #177).
fn compute_interface_hash(name: &str) -> [u8; 32] {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(name.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&hash);
    out
}

/// Extract short name from a full interface name.
/// E.g. "AutoInterface[Default Interface]" -> "Default Interface"
/// E.g. "tcp_client_0" -> "tcp_client_0"
fn short_name(name: &str) -> String {
    if let Some(start) = name.find('[') {
        if let Some(end) = name.find(']') {
            if start < end {
                return name[start + 1..end].to_string();
            }
        }
    }
    name.to_string()
}

/// Reported interface type.
///
/// The transport the interface was built over is authoritative: the name is a
/// peer/instance label the driver generates (`rnode_0`, `i2p_0_to_<peer>`,
/// `autoconnect/<addr>`), so classifying by it reports the wrong medium for
/// every interface whose label carries no transport prefix. The name is still
/// consulted for the TCP client/server split, which the kind does not carry,
/// and as the fallback for interfaces that register no kind.
fn interface_type(kind: InterfaceKind, name: &str) -> String {
    match kind {
        InterfaceKind::Tcp => {
            if name.starts_with("tcp_server") || name.starts_with("TCPServer") {
                "TCPServerInterface"
            } else {
                "TCPClientInterface"
            }
        }
        InterfaceKind::Udp => "UDPInterface",
        InterfaceKind::I2p => "I2PInterface",
        InterfaceKind::Serial => "SerialInterface",
        InterfaceKind::Rnode => "RNodeInterface",
        InterfaceKind::Kiss => "KISSInterface",
        // Python has no `LocalInterface` class: an accepted IPC connection is
        // a LocalClientInterface, and the shared-instance server (reported out
        // of the inventory, never through this fallback) a LocalServerInterface.
        InterfaceKind::Local => "LocalClientInterface",
        InterfaceKind::Pipe => "PipeInterface",
        InterfaceKind::Auto => "AutoInterface",
        // Not `ChannelInterface`: Python's `RNS.Channel` is an unrelated
        // link-layer concept, and byte-channel names are caller-supplied, so
        // the name heuristic must not get a say here.
        InterfaceKind::Channel => "ByteChannelInterface",
        // The Columba ble-reticulum protocol; the reference package's own
        // RNS interface class carries the same name.
        InterfaceKind::Ble => "BLEInterface",
        InterfaceKind::Unknown => return interface_type_from_name(name),
    }
    .to_string()
}

/// Fallback classification for interfaces that register no transport kind
/// (dynamically spawned interfaces that inherit `Unknown`, and out-of-tree
/// `Interface` implementations that do not override `kind()`).
fn interface_type_from_name(name: &str) -> String {
    if name.starts_with("AutoInterface") || name.starts_with("auto/") {
        "AutoInterface".to_string()
    } else if name.starts_with("tcp_client") || name.starts_with("TCPClient") {
        "TCPClientInterface".to_string()
    } else if name.starts_with("tcp_server") || name.starts_with("TCPServer") {
        "TCPServerInterface".to_string()
    } else if name.starts_with("udp") || name.starts_with("UDP") {
        "UDPInterface".to_string()
    } else if name.starts_with("local") || name.starts_with("Local") {
        "LocalClientInterface".to_string()
    } else {
        "Interface".to_string()
    }
}

/// Try to convert a byte slice to a 16-byte hash.
fn try_into_hash(bytes: &[u8]) -> Option<[u8; TRUNCATED_HASHBYTES]> {
    if bytes.len() >= TRUNCATED_HASHBYTES {
        let mut h = [0u8; TRUNCATED_HASHBYTES];
        h.copy_from_slice(&bytes[..TRUNCATED_HASHBYTES]);
        Some(h)
    } else {
        None
    }
}

/// Compute Unix epoch base from the monotonic start_time.
///
/// `start_time` is a `std::time::Instant` captured when the node was created.
/// `std::time::SystemTime::now() - start_time.elapsed()` gives the wall clock
/// at the moment `start_time` was created.
fn epoch_base_secs(start_time: std::time::Instant) -> f64 {
    let elapsed = start_time.elapsed();
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (now_epoch - elapsed).as_secs_f64()
}

/// Convert a core monotonic millisecond timestamp to Unix epoch seconds.
fn mono_ms_to_epoch(epoch_base: f64, _now_mono_ms: u64, mono_ms: u64) -> f64 {
    epoch_base + (mono_ms as f64 / 1000.0)
}

/// Burst activation timestamp for interface_stats: 0 (never activated) stays
/// the int 0 like Python's initial `ic_burst_activated`; a real activation is
/// converted from the core's monotonic seconds to epoch seconds, because
/// rnstatus renders `time.time() - burst_activated` as the burst duration
/// (rnstatus.py:566).
fn activation_to_epoch(epoch_base: f64, activated_mono_secs: u64) -> Value {
    if activated_mono_secs == 0 {
        pickle_int(0)
    } else {
        pickle_float(epoch_base + activated_mono_secs as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors Python `Transport.first_hop_timeout` (Transport.py:2700-2703):
    /// a known next-hop bitrate yields `MTU * 8 / bitrate + DEFAULT_PER_HOP_TIMEOUT`
    /// (not the flat 6.0 the stub returned); an unknown path yields the
    /// DEFAULT_PER_HOP_TIMEOUT fallback.
    #[test]
    fn test_first_hop_timeout_secs_matches_python() {
        // Unknown destination/bitrate -> Python fallback DEFAULT_PER_HOP_TIMEOUT.
        assert_eq!(first_hop_timeout_secs(None), DEFAULT_PER_HOP_TIMEOUT as f64);
        assert_eq!(
            first_hop_timeout_secs(Some(0)),
            DEFAULT_PER_HOP_TIMEOUT as f64
        );

        // Known bitrate: 500 * 8 / 9600 + 6 = 6.41666...
        let expected = MTU as f64 * 8.0 / 9600.0 + DEFAULT_PER_HOP_TIMEOUT as f64;
        let got = first_hop_timeout_secs(Some(9600));
        assert!((got - expected).abs() < 1e-9, "got {got}, want {expected}");
        assert!(got > 6.0, "known bitrate must exceed the flat 6.0 stub");

        // Slow LoRa link (1200 bps): 500 * 8 / 1200 + 6 = 9.33333...
        let expected_lora = MTU as f64 * 8.0 / 1200.0 + DEFAULT_PER_HOP_TIMEOUT as f64;
        assert!((first_hop_timeout_secs(Some(1200)) - expected_lora).abs() < 1e-9);
    }

    // --- The path-timing verb pair (Codeberg #329) ------------------------

    /// Build a core with a temp storage dir, named after the calling test so
    /// two tests never share a directory.
    fn timing_core(tag: &str) -> (StdNodeCore, tempfile::TempDir) {
        use crate::clock::SystemClock;
        use leviculum_core::node::NodeCoreBuilder;

        let tmp = tempfile::Builder::new()
            .prefix(&format!("rpc-{tag}-"))
            .tempdir()
            .expect("temp storage dir");
        let core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(tmp.path()).expect("storage"),
        );
        (core, tmp)
    }

    fn empty_stats_map_local() -> InterfaceStatsMap {
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()))
    }

    /// Insert fresh counters for `id` with the given online state, so a
    /// test can stage what an interface task would have recorded (L-0020).
    fn stage_online(map: &InterfaceStatsMap, id: usize, online: bool) {
        let counters = std::sync::Arc::new(crate::interfaces::InterfaceCounters::new());
        counters.set_online(online);
        map.lock_recover().insert(id, counters);
    }

    /// Mirrors Python `Transport.medium_path_timeout` (Reticulum 1.5.2) with
    /// the constants MTU=500 (Reticulum.py:93), MINIMUM_BITRATE=5
    /// (Reticulum.py:134) and DEFAULT_PER_HOP_TIMEOUT=6 (Reticulum.py:142),
    /// all three unchanged since the vendored 1.3.5.
    ///
    /// The zero arm is the one the batch exists for: it is what an lnsd
    /// without this verb handed every client, and it is NOT the same number as
    /// the formula's — the positive control below pins the difference.
    #[test]
    fn medium_path_timeout_is_the_upstream_formula() {
        // Unknown / non-positive bitrate -> Python's documented 0.
        assert_eq!(medium_path_timeout_secs(None), 0.0);
        assert_eq!(medium_path_timeout_secs(Some(0)), 0.0);
        assert_eq!(medium_path_timeout_secs(Some(-1)), 0.0);

        // 10 Mbps TCP: 2*(500*8/10_000_000) + 6 = 6.0008
        let tcp = medium_path_timeout_secs(Some(10_000_000));
        assert!(
            (tcp - 6.0008).abs() < 1e-9,
            "10 Mbps gave {tcp}, want 6.0008"
        );
        // Positive control: the formula value is distinguishable from the
        // no-verb answer, so a test asserting "not 0" is asserting something.
        assert!(tcp > 0.0);

        // 1200 bps LoRa: 2*(4000/1200) + 6 = 12.6666...
        let lora = medium_path_timeout_secs(Some(1200));
        let want_lora = 2.0 * (MTU as f64 * 8.0 / 1200.0) + DEFAULT_PER_HOP_TIMEOUT as f64;
        assert!((lora - want_lora).abs() < 1e-9, "1200 bps gave {lora}");
        assert!(lora > tcp, "a slower medium must widen the window");

        // The MINIMUM_BITRATE floor: anything under 5 bps is priced as 5 bps,
        // which caps the window at 2*(4000/5)+6 = 1606 s instead of growing
        // without bound.
        let floored =
            2.0 * (MTU as f64 * 8.0 / MINIMUM_BITRATE as f64) + DEFAULT_PER_HOP_TIMEOUT as f64;
        assert!((medium_path_timeout_secs(Some(1)) - floored).abs() < 1e-9);
        assert!((medium_path_timeout_secs(Some(4)) - floored).abs() < 1e-9);
        assert!(
            (medium_path_timeout_secs(Some(MINIMUM_BITRATE as i64)) - floored).abs() < 1e-9,
            "the floor must be reached, not merely approached, at MINIMUM_BITRATE"
        );
        // Positive control on the floor: one bit per second above it is a
        // strictly narrower window, so the clamp is a clamp and not a constant.
        assert!(medium_path_timeout_secs(Some(6)) < floored);
    }

    /// `min` over the ONLINE interfaces, with each interface's bitrate
    /// resolved exactly as `build_interface_stats` resolves its `bitrate` key
    /// (Reticulum 1.5.2 `Transport.prioritize_interfaces`).
    #[test]
    fn lowest_interface_bitrate_takes_the_min_over_online_interfaces() {
        use leviculum_core::transport::LinkProfile;

        let (mut core, _tmp) = timing_core("lowest-bitrate");
        let inventory = crate::interfaces::inventory::InterfaceInventory::shared();
        let online = empty_stats_map_local();

        // Positive control for the None arm: with no interface at all there
        // is nothing to take a min over, and Python's generator is empty.
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            None,
            "a daemon with no interfaces knows no bitrate"
        );

        // Fast TCP client: no configured bitrate, no profile -> BITRATE_GUESS.
        core.set_interface_name(0, "tcp_client_0".into());
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            Some(crate::interfaces::tcp::TCP_BITRATE_GUESS)
        );

        // A radio reporting its own on-air rate is now the slowest.
        core.set_interface_name(1, "RNodeInterface[/dev/ttyUSB0]".into());
        core.register_interface_link_profile(
            1,
            LinkProfile {
                bitrate_bps: 2734,
                tx_jitter_max_ms: Some(2926),
            },
        );
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            Some(2734),
            "the radio's own rate must win the min, not the TCP guess"
        );

        // Taking the radio offline hands the min back to TCP: the `online`
        // filter is real, not decorative.
        stage_online(&online, 1, false);
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            Some(crate::interfaces::tcp::TCP_BITRATE_GUESS),
            "an offline interface must not set the wait window"
        );
        stage_online(&online, 1, true);
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            Some(2734)
        );

        // A configured bitrate outranks the profile here for the same reason
        // it does in the stats row: a client reading both must see one number.
        core.register_interface_bitrate(1, 9600);
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            Some(9600)
        );
    }

    /// Listeners are part of the same one interface list Python takes its
    /// `min` over (Codeberg #177), and a zero/absent rate is dropped rather
    /// than winning the min (Python's `and interface.bitrate` truthiness
    /// filter).
    #[test]
    fn lowest_interface_bitrate_covers_listeners_and_drops_zero_rates() {
        use crate::interfaces::inventory::{InterfaceIdentity, ListenerRow};
        use leviculum_core::traits::InterfaceMode;

        let (mut core, _tmp) = timing_core("lowest-bitrate-listeners");
        let inventory = crate::interfaces::inventory::InterfaceInventory::shared();
        let online = empty_stats_map_local();

        let listener = |name: &str, type_name: &'static str, bitrate: i64| ListenerRow {
            identity: InterfaceIdentity {
                name: name.to_string(),
                short_name: name.to_string(),
                type_name,
                parent: None,
            },
            bitrate,
            hw_mtu: 262_144,
            mode: InterfaceMode::Full,
            announce_rate: (None, None, None),
            ifac_size_bits: None,
            departed_rxb: 0,
            departed_txb: 0,
            bound_addr: None,
        };

        // The shared-instance server alone: 1 Gbps, and it counts.
        inventory.lock_recover().add_listener(
            10,
            listener("Shared Instance", "LocalServerInterface", 1_000_000_000),
        );
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            Some(1_000_000_000),
            "a listener-only daemon still reports the rate it has"
        );

        // A TCP listener at 10 Mbps takes the min from it — the single-TCP
        // config the A/B parity check runs on.
        inventory.lock_recover().add_listener(
            11,
            listener("Parity TCP Server", "TCPServerInterface", 10_000_000),
        );
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            Some(10_000_000)
        );

        // An interface whose configured bitrate is 0 means "no cap", not
        // "0 bps": Python's truthiness filter drops it, and so must we —
        // otherwise the whole window collapses to the unknown-answer 0.
        core.set_interface_name(0, "tcp_client_0".into());
        core.register_interface_bitrate(0, 0);
        assert_eq!(
            lowest_online_bitrate(&core, &online, &inventory),
            Some(10_000_000),
            "a 0 bitrate must be dropped, not win the min"
        );
    }

    /// The verbs answer over the real dispatch path, in the codec a 1.5.2
    /// client speaks — the parse arm and the handler arm are separate places
    /// to get wrong.
    #[test]
    fn the_timing_verbs_dispatch_end_to_end() {
        use crate::interfaces::inventory::{InterfaceIdentity, ListenerRow};
        use leviculum_core::traits::InterfaceMode;

        let (mut core, _tmp) = timing_core("timing-dispatch");
        let inventory = crate::interfaces::inventory::InterfaceInventory::shared();
        let stats: InterfaceStatsMap =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
        inventory.lock_recover().add_listener(
            10,
            ListenerRow {
                identity: InterfaceIdentity {
                    name: "Parity TCP Server".into(),
                    short_name: "Parity TCP Server".into(),
                    type_name: "TCPServerInterface",
                    parent: None,
                },
                bitrate: 10_000_000,
                hw_mtu: 262_144,
                mode: InterfaceMode::Full,
                announce_rate: (None, None, None),
                ifac_size_bits: None,
                departed_rxb: 0,
                departed_txb: 0,
                bound_addr: None,
            },
        );

        let ask = |core: &mut StdNodeCore, verb: &str| -> Value {
            let request = pickle_dict(vec![(pickle_str_key("get"), pickle_str(verb))]);
            let encoded = encode_request_msgpack(&request).expect("encode request");
            let (parsed, codec) = parse_request(&encoded).expect("parse request");
            assert!(matches!(codec, Codec::Msgpack));
            let bytes = handle_request(
                &parsed,
                core,
                std::time::Instant::now(),
                &stats,
                &inventory,
                0,
                None,
                codec,
            )
            .expect("handle request");
            decode_response_msgpack(&bytes).expect("decode response")
        };

        assert_eq!(
            ask(&mut core, "lowest_interface_bitrate"),
            Value::I64(10_000_000)
        );
        match ask(&mut core, "medium_path_timeout") {
            Value::F64(secs) => assert!(
                (secs - 6.0008).abs() < 1e-9,
                "medium_path_timeout answered {secs}, want 6.0008"
            ),
            other => panic!("medium_path_timeout must answer a float, got {other:?}"),
        }
        assert_eq!(ask(&mut core, "active_link_count"), Value::I64(0));

        // Positive control on the parse arm: an unrelated verb still fails,
        // so the three arms above are matching their own names and not a
        // catch-all that would answer anything.
        let bogus = pickle_dict(vec![(
            pickle_str_key("get"),
            pickle_str("medium_path_timeouts"),
        )]);
        let encoded = encode_request_msgpack(&bogus).expect("encode");
        assert!(
            parse_request(&encoded).is_err(),
            "a near-miss verb name must not resolve"
        );

        // `profiling_results` stays declined: it must keep reaching the
        // unknown-command arm, which is what lets rnstatus --profiling skip
        // it (`rnstatus --profiling`) instead of rendering a fabrication.
        let profiling = pickle_dict(vec![(
            pickle_str_key("get"),
            pickle_str("profiling_results"),
        )]);
        let encoded = encode_request_msgpack(&profiling).expect("encode");
        assert!(
            parse_request(&encoded).is_err(),
            "profiling_results is declined and must stay unimplemented"
        );
    }

    #[test]
    fn test_short_name_with_brackets() {
        assert_eq!(
            short_name("AutoInterface[Default Interface]"),
            "Default Interface"
        );
    }

    #[test]
    fn test_short_name_without_brackets() {
        assert_eq!(short_name("tcp_client_0"), "tcp_client_0");
    }

    // The four name cases below pin the fallback an interface that registers no
    // kind still gets, so they pass `InterfaceKind::Unknown` deliberately.
    #[test]
    fn test_interface_type_auto() {
        assert_eq!(
            interface_type(InterfaceKind::Unknown, "AutoInterface[foo]"),
            "AutoInterface"
        );
    }

    #[test]
    fn test_interface_type_auto_peer() {
        assert_eq!(
            interface_type(InterfaceKind::Unknown, "auto/eth0/abcd1234"),
            "AutoInterface"
        );
    }

    #[test]
    fn test_interface_type_tcp_client() {
        assert_eq!(
            interface_type(InterfaceKind::Unknown, "tcp_client_0"),
            "TCPClientInterface"
        );
    }

    #[test]
    fn test_interface_type_unknown() {
        assert_eq!(
            interface_type(InterfaceKind::Unknown, "custom_iface"),
            "Interface"
        );
    }

    // The kind is authoritative where the name label carries no transport: the
    // driver's own generated names for RNode, I2P, serial and discovery-
    // autoconnected TCP interfaces all fall through the name heuristic.
    #[test]
    fn test_interface_type_prefers_kind_over_name_label() {
        for (kind, name, expected) in [
            (InterfaceKind::Rnode, "rnode_0", "RNodeInterface"),
            (InterfaceKind::I2p, "i2p_0_to_peer", "I2PInterface"),
            (InterfaceKind::Serial, "serial_0", "SerialInterface"),
            (InterfaceKind::Kiss, "kiss_0", "KISSInterface"),
            (InterfaceKind::Pipe, "pipe_0", "PipeInterface"),
            (
                InterfaceKind::Tcp,
                "autoconnect/10.0.0.1:4242",
                "TCPClientInterface",
            ),
            (
                InterfaceKind::Tcp,
                "tcp_server/10.0.0.1",
                "TCPServerInterface",
            ),
            (InterfaceKind::Udp, "udp_0", "UDPInterface"),
            (InterfaceKind::Local, "Local[lnsd]", "LocalClientInterface"),
            (InterfaceKind::Auto, "auto/eth0/abcd", "AutoInterface"),
            // Byte-channel names are caller-supplied, so a name the heuristic
            // would misread must not override the registered kind.
            (
                InterfaceKind::Channel,
                "tcp_client_app",
                "ByteChannelInterface",
            ),
        ] {
            assert_eq!(
                interface_type(kind, name),
                expected,
                "kind={kind} name={name}"
            );
        }
    }

    #[test]
    fn test_interface_hash_deterministic() {
        let h1 = compute_interface_hash("test");
        let h2 = compute_interface_hash("test");
        assert_eq!(h1, h2);
        assert_ne!(h1, [0u8; 32]);
        // Full-length Identity.full_hash, byte-comparable with the reference.
        assert_eq!(h1.len(), 32);
    }

    #[test]
    fn test_try_into_hash() {
        assert!(try_into_hash(&[0xAB; 16]).is_some());
        assert!(try_into_hash(&[0xAB; 20]).is_some());
        assert!(try_into_hash(&[0xAB; 15]).is_none());
        assert!(try_into_hash(&[]).is_none());
    }

    // identity_data lifecycle dispatch end-to-end: handle_request must produce a
    // bool response (never drop the connection) for both the pickle and msgpack
    // codecs. For an identity with no known destinations (nothing cached), the
    // real handler reports Bool(false), mirroring Python `_retain_identity`
    // returning retained=False. This is the wire contract rnid relies on when it
    // issues identity_data:retain after a recall (it swallows the boolean).
    #[test]
    fn identity_data_handlers_return_bool_false_when_unknown() {
        use crate::clock::SystemClock;
        use crate::interfaces::InterfaceStatsMap;
        use crate::rpc::pickle::{decode_response_msgpack, Codec, RpcRequest};
        use leviculum_core::node::NodeCoreBuilder;
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-identity-data-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::new()));
        let start = std::time::Instant::now();

        let decode = |bytes: &[u8], codec: Codec| -> Value {
            match codec {
                Codec::Pickle => serde_pickle::value_from_slice(bytes, Default::default()).unwrap(),
                Codec::Msgpack => decode_response_msgpack(bytes).unwrap(),
            }
        };

        for codec in [Codec::Pickle, Codec::Msgpack] {
            for req in [
                RpcRequest::IdentityDataRetain {
                    identity_hash: vec![0x11u8; 16],
                },
                RpcRequest::IdentityDataUnretain {
                    identity_hash: vec![0x22u8; 16],
                },
            ] {
                let bytes = handle_request(
                    &req,
                    &mut core,
                    start,
                    &stats,
                    &crate::interfaces::inventory::InterfaceInventory::shared(),
                    0,
                    None,
                    codec,
                )
                .unwrap();
                let value = decode(&bytes, codec);
                assert!(
                    matches!(value, Value::Bool(false)),
                    "{:?} via {:?} must return bool false for an unknown identity, got {:?}",
                    req,
                    codec,
                    value
                );
            }
        }
    }

    // destination_data cache-lifecycle RPC round-trip (Codeberg #84): each op
    // dispatches end-to-end and returns the real bool reflecting the known
    // destination's use-state, over both codecs.
    #[test]
    fn destination_data_rpc_round_trip() {
        use crate::clock::SystemClock;
        use crate::interfaces::InterfaceStatsMap;
        use crate::rpc::pickle::{decode_response_msgpack, Codec, RpcRequest};
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::traits::Storage;
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-dest-data-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::new()));
        let start = std::time::Instant::now();

        let decode = |bytes: &[u8], codec: Codec| -> Value {
            match codec {
                Codec::Pickle => serde_pickle::value_from_slice(bytes, Default::default()).unwrap(),
                Codec::Msgpack => decode_response_msgpack(bytes).unwrap(),
            }
        };
        let dispatch = |core: &mut StdNodeCore, req: &RpcRequest, codec: Codec| -> Value {
            let bytes = handle_request(
                req,
                core,
                start,
                &stats,
                &crate::interfaces::inventory::InterfaceInventory::shared(),
                0,
                None,
                codec,
            )
            .unwrap();
            decode(&bytes, codec)
        };

        let dest = vec![0x42u8; 16];
        let dest_arr: [u8; 16] = dest.clone().try_into().unwrap();

        for codec in [Codec::Pickle, Codec::Msgpack] {
            let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
                rand_core::OsRng,
                SystemClock::new(),
                crate::storage::Storage::new(&tmp).unwrap(),
            );

            // Unknown destination: every op reports false.
            for op in [
                RpcRequest::DestinationDataUsed {
                    destination_hash: dest.clone(),
                },
                RpcRequest::DestinationDataRetain {
                    destination_hash: dest.clone(),
                },
                RpcRequest::DestinationDataUnretain {
                    destination_hash: dest.clone(),
                },
            ] {
                assert!(
                    matches!(dispatch(&mut core, &op, codec), Value::Bool(false)),
                    "{:?} on an unknown destination must be false ({:?})",
                    op,
                    codec
                );
            }

            // Make it known (cache a placeholder announce blob).
            core.storage_mut()
                .set_announce_cache(dest_arr, vec![0xAB; 32]);

            // used → true (recency touch), retain → true (pin), then used → false
            // because a retained entry is skipped, unretain → true (lifts pin),
            // used → true again.
            assert!(matches!(
                dispatch(
                    &mut core,
                    &RpcRequest::DestinationDataUsed {
                        destination_hash: dest.clone()
                    },
                    codec
                ),
                Value::Bool(true)
            ));
            assert!(matches!(
                dispatch(
                    &mut core,
                    &RpcRequest::DestinationDataRetain {
                        destination_hash: dest.clone()
                    },
                    codec
                ),
                Value::Bool(true)
            ));
            assert!(
                matches!(
                    dispatch(
                        &mut core,
                        &RpcRequest::DestinationDataUsed {
                            destination_hash: dest.clone()
                        },
                        codec
                    ),
                    Value::Bool(false)
                ),
                "used on a retained destination is false ({:?})",
                codec
            );
            assert!(matches!(
                dispatch(
                    &mut core,
                    &RpcRequest::DestinationDataUnretain {
                        destination_hash: dest.clone()
                    },
                    codec
                ),
                Value::Bool(true)
            ));
            assert!(matches!(
                dispatch(
                    &mut core,
                    &RpcRequest::DestinationDataUsed {
                        destination_hash: dest.clone()
                    },
                    codec
                ),
                Value::Bool(true)
            ));
        }
    }

    // Codeberg #25: the radio-stats field builder emits the Python field
    // names/units and gates battery_state/battery_percent on a known state.
    #[test]
    fn radio_stat_fields_names_units_and_gating() {
        use crate::interfaces::RadioStats;
        use leviculum_core::rnode::BatteryState;

        let get = |fields: &[(HashableValue, Value)], key: &str| -> Option<Value> {
            fields
                .iter()
                .find(|(k, _)| *k == HashableValue::String(key.into()))
                .map(|(_, v)| v.clone())
        };

        // Unknown battery + no reports: airtime/channel-load default to 0.0,
        // noise_floor/cpu_temp are None, battery_* keys are omitted.
        let f = radio_stat_fields(&RadioStats::default());
        assert_eq!(get(&f, "airtime_short"), Some(Value::F64(0.0)));
        assert_eq!(get(&f, "airtime_long"), Some(Value::F64(0.0)));
        assert_eq!(get(&f, "channel_load_short"), Some(Value::F64(0.0)));
        assert_eq!(get(&f, "channel_load_long"), Some(Value::F64(0.0)));
        assert_eq!(get(&f, "noise_floor"), Some(Value::None));
        assert_eq!(get(&f, "cpu_temp"), Some(Value::None));
        assert!(get(&f, "battery_state").is_none());
        assert!(get(&f, "battery_percent").is_none());
        assert!(get(&f, "last_rssi").is_none());
        assert!(get(&f, "last_snr").is_none());

        // Populated values, charging battery.
        let r = RadioStats {
            airtime_short: 3.0,
            airtime_long: 10.0,
            channel_load_short: 2.0,
            channel_load_long: 6.0,
            noise_floor: Some(-57),
            cpu_temp: Some(25),
            battery_state: BatteryState::Charging,
            battery_percent: 85,
            last_rssi: Some(-57),
            last_snr: Some(10.0),
        };
        let f = radio_stat_fields(&r);
        assert_eq!(get(&f, "airtime_short"), Some(Value::F64(3.0)));
        assert_eq!(get(&f, "airtime_long"), Some(Value::F64(10.0)));
        assert_eq!(get(&f, "channel_load_short"), Some(Value::F64(2.0)));
        assert_eq!(get(&f, "channel_load_long"), Some(Value::F64(6.0)));
        assert_eq!(get(&f, "noise_floor"), Some(Value::I64(-57)));
        assert_eq!(get(&f, "cpu_temp"), Some(Value::I64(25)));
        assert_eq!(
            get(&f, "battery_state"),
            Some(Value::String("charging".into()))
        );
        assert_eq!(get(&f, "battery_percent"), Some(Value::I64(85)));
        // RSSI/SNR are additive keys of ours, present once reported (Python
        // does not place them in the dict; rnstatus ignores unknown keys).
        assert_eq!(get(&f, "last_rssi"), Some(Value::I64(-57)));
        assert_eq!(get(&f, "last_snr"), Some(Value::F64(10.0)));
        assert!(get(&f, "r_stat_rssi").is_none());
    }

    // Codeberg #25: end-to-end through build_interface_stats — a radio
    // interface's response dict carries the radio rows with the right
    // fields/units.
    #[test]
    fn build_interface_stats_emits_radio_rows_for_rnode() {
        use crate::clock::SystemClock;
        use crate::interfaces::{InterfaceCounters, InterfaceStatsMap};
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::rnode::BatteryState;
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-radio-stats-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        core.set_interface_name(0, "RNodeInterface[/dev/ttyUSB0]".into());

        let counters = Arc::new(InterfaceCounters::new());
        counters.enable_radio_stats();
        counters.update_radio(|r| {
            r.airtime_short = 3.0;
            r.noise_floor = Some(-57);
            r.cpu_temp = Some(25);
            r.battery_state = BatteryState::Charging;
            r.battery_percent = 85;
            r.last_rssi = Some(-42);
            r.last_snr = Some(9.5);
        });
        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::from([(0usize, counters)])));

        let value = build_interface_stats(
            &mut core,
            std::time::Instant::now(),
            &stats,
            &crate::interfaces::inventory::InterfaceInventory::shared(),
            0,
        );

        let Value::Dict(top) = value else {
            panic!("interface_stats must be a dict")
        };
        let ifaces = top
            .get(&HashableValue::String("interfaces".into()))
            .expect("interfaces key");
        let Value::List(list) = ifaces else {
            panic!("interfaces must be a list")
        };
        assert_eq!(list.len(), 1, "the one registered interface must appear");
        let Value::Dict(iface) = &list[0] else {
            panic!("interface entry must be a dict")
        };
        let get = |k: &str| iface.get(&HashableValue::String(k.into())).cloned();
        assert_eq!(get("airtime_short"), Some(Value::F64(3.0)));
        assert_eq!(get("noise_floor"), Some(Value::I64(-57)));
        assert_eq!(get("cpu_temp"), Some(Value::I64(25)));
        assert_eq!(get("battery_state"), Some(Value::String("charging".into())));
        assert_eq!(get("battery_percent"), Some(Value::I64(85)));
        assert_eq!(get("last_rssi"), Some(Value::I64(-42)));
        assert_eq!(get("last_snr"), Some(Value::F64(9.5)));
    }

    /// Codeberg #190: what a client tool two hops from the radio has to be
    /// able to read off the shared-instance payload.
    ///
    /// Three rows, one per case the reader must handle:
    ///
    /// - a radio with a pre-TX contention bound reports its own on-air bitrate
    ///   (Python `RNodeInterface.updateBitrate`, RNodeInterface.py:693-696) and
    ///   the ceiling, in seconds like every other time-valued key here;
    /// - a medium with airtime but no contention bound reports the bitrate and
    ///   no ceiling key at all, exactly as Python gates its optional keys on
    ///   `hasattr`;
    /// - a medium with neither keeps the BITRATE_GUESS and carries no ceiling.
    #[test]
    fn build_interface_stats_reports_the_link_profile_of_each_medium() {
        use crate::clock::SystemClock;
        use crate::interfaces::InterfaceStatsMap;
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::transport::LinkProfile;
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-link-profile-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        core.set_interface_name(0, "RNodeInterface[/dev/ttyUSB0]".into());
        core.register_interface_link_profile(
            0,
            LinkProfile {
                bitrate_bps: 2734,
                tx_jitter_max_ms: Some(2926),
            },
        );
        core.set_interface_name(1, "SerialInterface[/dev/ttyUSB1]".into());
        core.register_interface_link_profile(
            1,
            LinkProfile {
                bitrate_bps: 115_200,
                tx_jitter_max_ms: None,
            },
        );
        core.set_interface_name(2, "tcp_client_0".into());

        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::new()));
        let value = build_interface_stats(
            &mut core,
            std::time::Instant::now(),
            &stats,
            &crate::interfaces::inventory::InterfaceInventory::shared(),
            0,
        );

        let Value::Dict(top) = value else {
            panic!("interface_stats must be a dict")
        };
        let Value::List(list) = top
            .get(&HashableValue::String("interfaces".into()))
            .expect("interfaces key")
        else {
            panic!("interfaces must be a list")
        };
        assert_eq!(list.len(), 3);
        let field = |row: &Value, k: &str| -> Option<Value> {
            let Value::Dict(d) = row else {
                panic!("interface entry must be a dict")
            };
            d.get(&HashableValue::String(k.into())).cloned()
        };
        let row_named = |name: &str| -> Value {
            list.iter()
                .find(|r| field(r, "name") == Some(Value::String(name.into())))
                .unwrap_or_else(|| panic!("no row named {name}"))
                .clone()
        };

        let radio = row_named("RNodeInterface[/dev/ttyUSB0]");
        assert_eq!(
            field(&radio, "bitrate"),
            Some(Value::I64(2734)),
            "a radio must report its own on-air bitrate, not the TCP guess"
        );
        assert_eq!(
            field(&radio, "tx_jitter_max"),
            Some(Value::F64(2.926)),
            "the jitter ceiling is reported in seconds"
        );

        let serial = row_named("SerialInterface[/dev/ttyUSB1]");
        assert_eq!(field(&serial, "bitrate"), Some(Value::I64(115_200)));
        assert!(
            field(&serial, "tx_jitter_max").is_none(),
            "a medium that transmits when asked carries no ceiling key"
        );

        let tcp = row_named("tcp_client_0");
        assert_eq!(
            field(&tcp, "bitrate"),
            Some(Value::I64(crate::interfaces::tcp::TCP_BITRATE_GUESS)),
            "no profile keeps the per-medium BITRATE_GUESS"
        );
        assert!(field(&tcp, "tx_jitter_max").is_none());
    }

    /// A configured `bitrate` still overrides the interface's own rate, which
    /// is the order Python applies them in
    /// (`if configured_bitrate: interface.bitrate = configured_bitrate`,
    /// Reticulum.py:887). Codeberg #190 added the middle term below it, not
    /// above it.
    #[test]
    fn a_configured_bitrate_still_outranks_the_interfaces_own_rate() {
        use crate::clock::SystemClock;
        use crate::interfaces::InterfaceStatsMap;
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::transport::LinkProfile;
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-bitrate-order-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        core.set_interface_name(0, "RNodeInterface[/dev/ttyUSB0]".into());
        core.register_interface_link_profile(
            0,
            LinkProfile {
                bitrate_bps: 2734,
                tx_jitter_max_ms: Some(2926),
            },
        );
        core.register_interface_bitrate(0, 9600);

        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::new()));
        let value = build_interface_stats(
            &mut core,
            std::time::Instant::now(),
            &stats,
            &crate::interfaces::inventory::InterfaceInventory::shared(),
            0,
        );
        let Value::Dict(top) = value else {
            panic!("dict")
        };
        let Value::List(list) = top
            .get(&HashableValue::String("interfaces".into()))
            .expect("interfaces key")
        else {
            panic!("list")
        };
        let Value::Dict(row) = &list[0] else {
            panic!("dict")
        };
        assert_eq!(
            row.get(&HashableValue::String("bitrate".into())),
            Some(&Value::I64(9600))
        );
        assert_eq!(
            row.get(&HashableValue::String("tx_jitter_max".into())),
            Some(&Value::F64(2.926)),
            "overriding the bitrate must not hide the contention bound"
        );
    }

    /// Codeberg #177: a listener carries no packets and therefore has no
    /// transport entry, but it must still be reported — under the reference
    /// name, aggregating its children, and pointed at by them.
    #[test]
    fn build_interface_stats_reports_listeners_and_their_children() {
        use crate::clock::SystemClock;
        use crate::interfaces::inventory::{InterfaceIdentity, InterfaceInventory, ListenerRow};
        use crate::interfaces::{InterfaceCounters, InterfaceStatsMap};
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::traits::InterfaceMode;
        use std::collections::BTreeMap;
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-listener-rows-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        // Only the accepted connection is a routable interface; the listener
        // that spawned it never enters transport.
        core.set_interface_name(7, "tcp_server/127.0.0.1:40000".into());
        core.set_interface_kind(7, InterfaceKind::Tcp);

        let counters = Arc::new(InterfaceCounters::new());
        counters.rx_bytes.store(500, Ordering::Relaxed);
        counters.tx_bytes.store(90, Ordering::Relaxed);
        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::from([(7usize, counters)])));

        let listener_name = "TCPServerInterface[Srv/127.0.0.1:4242]";
        let inventory = InterfaceInventory::shared();
        {
            let mut inv = inventory.lock_recover();
            inv.add_listener(
                0,
                ListenerRow {
                    identity: InterfaceIdentity {
                        name: listener_name.into(),
                        short_name: "Srv".into(),
                        type_name: "TCPServerInterface",
                        parent: None,
                    },
                    bitrate: 10_000_000,
                    hw_mtu: 262_144,
                    mode: InterfaceMode::default(),
                    announce_rate: (Some(3600), Some(0), Some(5)),
                    ifac_size_bits: None,
                    // 100 bytes already carried by a client that left.
                    departed_rxb: 100,
                    departed_txb: 10,
                    bound_addr: Some("127.0.0.1:4242".parse().unwrap()),
                },
            );
            inv.add_spawned(
                7,
                InterfaceIdentity {
                    name: "TCPInterface[Client on Srv/127.0.0.1:40000]".into(),
                    short_name: "Client on Srv".into(),
                    type_name: "TCPClientInterface",
                    parent: Some(0),
                },
            );
        }

        let value =
            build_interface_stats(&mut core, std::time::Instant::now(), &stats, &inventory, 0);

        let Value::Dict(top) = value else {
            panic!("interface_stats must be a dict")
        };
        let Some(Value::List(list)) = top
            .get(&HashableValue::String("interfaces".into()))
            .cloned()
        else {
            panic!("interfaces must be a list")
        };
        assert_eq!(list.len(), 2, "listener + spawned connection");
        let row = |i: usize| -> BTreeMap<HashableValue, Value> {
            match &list[i] {
                Value::Dict(d) => d.clone(),
                other => panic!("interface entry must be a dict, got {other:?}"),
            }
        };
        let field = |d: &BTreeMap<HashableValue, Value>, k: &str| -> Option<Value> {
            d.get(&HashableValue::String(k.into())).cloned()
        };

        // Listener first (lower id), then its child.
        let listener = row(0);
        let child = row(1);
        assert_eq!(
            field(&listener, "name"),
            Some(Value::String(listener_name.into()))
        );
        assert_eq!(
            field(&listener, "type"),
            Some(Value::String("TCPServerInterface".into()))
        );
        assert_eq!(field(&listener, "clients"), Some(Value::I64(1)));
        // Live child plus the bytes banked from the departed one.
        assert_eq!(field(&listener, "rxb"), Some(Value::I64(600)));
        assert_eq!(field(&listener, "txb"), Some(Value::I64(100)));
        assert_eq!(
            field(&listener, "parent_interface_name"),
            None,
            "a listener has no parent"
        );

        assert_eq!(
            field(&child, "name"),
            Some(Value::String(
                "TCPInterface[Client on Srv/127.0.0.1:40000]".into()
            ))
        );
        assert_eq!(
            field(&child, "short_name"),
            Some(Value::String("Client on Srv".into()))
        );
        assert_eq!(
            field(&child, "type"),
            Some(Value::String("TCPClientInterface".into()))
        );
        assert_eq!(field(&child, "clients"), Some(Value::None));
        assert_eq!(
            field(&child, "parent_interface_name"),
            Some(Value::String(listener_name.into()))
        );
        assert_eq!(
            field(&child, "parent_interface_hash"),
            Some(Value::Bytes(compute_interface_hash(listener_name).to_vec())),
            "the parent link must resolve to the listener's own hash"
        );
        assert_eq!(
            field(&listener, "hash"),
            field(&child, "parent_interface_hash")
        );

        // Totals count the packet-carrying interface once: the listener's
        // aggregate must not be added on top.
        assert_eq!(
            top.get(&HashableValue::String("rxb".into())),
            Some(&Value::I64(500))
        );
        assert_eq!(
            top.get(&HashableValue::String("txb".into())),
            Some(&Value::I64(90))
        );
    }

    /// Codeberg #329: the keys current rnstatus indexes WITHOUT a presence
    /// guard are mandatory in every interface row, on every row kind.
    ///
    /// The rule this pins, so the next upstream key addition is a decision and
    /// not an outage: **a key rnstatus reads unguarded is mandatory; a key it
    /// reads behind `"k" in ifstat` is optional.** Omitting a guarded key
    /// degrades one line of output; omitting an unguarded one is a `KeyError`
    /// traceback and a non-zero exit — which is exactly how #329 presented,
    /// with two regression cells red on an image rebuild alone.
    ///
    /// The lists below are the audit of RNS 1.5.2 (`RNS/Utilities/rnstatus.py`,
    /// the release the periculum-test image resolves from PyPI). Line numbers
    /// are that file. `build_interface_stats_reports_listeners_and_their_children`
    /// above covers the same two row kinds for value correctness; this one
    /// covers key PRESENCE and the guard-implies-key relations, which is a
    /// different failure mode.
    #[test]
    fn every_interface_row_carries_the_keys_rnstatus_reads_unguarded() {
        use crate::clock::SystemClock;
        use crate::interfaces::inventory::{InterfaceIdentity, InterfaceInventory, ListenerRow};
        use crate::interfaces::{InterfaceCounters, InterfaceStatsMap};
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::traits::InterfaceMode;
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        // Read unconditionally for every interface rnstatus prints, on the
        // default path with no flags at all.
        const UNGUARDED_DEFAULT_PATH: &[&str] = &[
            "name",       // :405, :479
            "status",     // :432
            "mode",       // :437-442
            "clients",    // :446
            "txdrp",      // :495  <- the #329 crash site
            "txdrb",      // :496  (inside the txdrp branch)
            "rxb",        // :605
            "txb",        // :606
            "txbuffered", // :700  <- the next crash after txdrp
            "txstalled",  // :701  (inside the txbuffered branch)
        ];

        // Read unconditionally once a flag is given. Not reachable by a bare
        // `rnstatus`, but every one of them is a crash for an operator who
        // types the flag, so they are mandatory too.
        const UNGUARDED_UNDER_FLAGS: &[&str] = &[
            "bitrate", // --sort rate :379
            "mtu",     // :499, reached because our bitrate is never None
            "rxs", "txs", // --sort rxs/txs :387-390
            "arxc", "atxc", // -A totals :687-688; --sort arxc/atxc
            "prxc", "ptxc", // -P totals :679-680; --sort prxc/ptxc
            "arxs", "atxs", // -A flow ratios :622-623
            "prxs", "ptxs", // -P flow ratios :647-648
            "arxb", "atxb", "prxb", "ptxb", // --sort atx/arx/ptx/prx :391-398
        ];

        // Guarded reads that pull a SECOND key in unguarded once the guard
        // passes. Serving the guard key without its partner is the same crash
        // one level down, so these are all-or-nothing pairs.
        const IMPLIED_PAIRS: &[(&str, &str)] = &[
            // :609 guards on incoming_*, :610/:618 then index outgoing_*.
            ("incoming_announce_frequency", "outgoing_announce_frequency"),
            // :631 guards on incoming_*, :632/:643 then index outgoing_*.
            ("incoming_pr_frequency", "outgoing_pr_frequency"),
            // :668 guards on both together, :669 indexes both.
            ("protocol_violations", "ifac_violations"),
            ("ifac_violations", "protocol_violations"),
            // :592/:599 guard on *_active, :595/:602 index *_activated.
            ("burst_active", "burst_activated"),
            ("pr_burst_active", "pr_burst_activated"),
            // :561 guards on ifac_signature, :563 indexes ifac_size.
            ("ifac_signature", "ifac_size"),
        ];

        let tmp = std::env::temp_dir().join(format!("rpc-mandatory-keys-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        core.set_interface_name(7, "tcp_server/127.0.0.1:40000".into());
        core.set_interface_kind(7, InterfaceKind::Tcp);

        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::from([(
            7usize,
            Arc::new(InterfaceCounters::new()),
        )])));

        // Both row kinds: a listener (built from the inventory) and a spawned
        // connection (built from transport). They share `row_fields`, and this
        // asserts that sharing actually holds at the reply boundary.
        let inventory = InterfaceInventory::shared();
        {
            let mut inv = inventory.lock_recover();
            inv.add_listener(
                0,
                ListenerRow {
                    identity: InterfaceIdentity {
                        name: "TCPServerInterface[Srv/127.0.0.1:4242]".into(),
                        short_name: "Srv".into(),
                        type_name: "TCPServerInterface",
                        parent: None,
                    },
                    bitrate: 10_000_000,
                    hw_mtu: 262_144,
                    mode: InterfaceMode::default(),
                    announce_rate: (Some(3600), Some(0), Some(5)),
                    ifac_size_bits: None,
                    departed_rxb: 0,
                    departed_txb: 0,
                    bound_addr: Some("127.0.0.1:4242".parse().unwrap()),
                },
            );
            inv.add_spawned(
                7,
                InterfaceIdentity {
                    name: "TCPInterface[Client on Srv/127.0.0.1:40000]".into(),
                    short_name: "Client on Srv".into(),
                    type_name: "TCPClientInterface",
                    parent: Some(0),
                },
            );
        }

        let value =
            build_interface_stats(&mut core, std::time::Instant::now(), &stats, &inventory, 0);
        let Value::Dict(top) = value else {
            panic!("interface_stats must be a dict")
        };
        let Some(Value::List(list)) = top
            .get(&HashableValue::String("interfaces".into()))
            .cloned()
        else {
            panic!("interfaces must be a list")
        };
        assert_eq!(list.len(), 2, "listener + spawned connection");

        for entry in &list {
            let Value::Dict(row) = entry else {
                panic!("interface entry must be a dict, got {entry:?}")
            };
            let has = |k: &str| row.contains_key(&HashableValue::String(k.into()));
            let name = match row.get(&HashableValue::String("name".into())) {
                Some(Value::String(s)) => s.clone(),
                _ => "<unnamed>".into(),
            };

            for key in UNGUARDED_DEFAULT_PATH.iter().chain(UNGUARDED_UNDER_FLAGS) {
                assert!(
                    has(key),
                    "{name}: rnstatus indexes ifstat[{key:?}] with no presence guard, \
                     so omitting it is a KeyError crash, not a missing line"
                );
            }
            for (guard, implied) in IMPLIED_PAIRS {
                if has(guard) {
                    assert!(
                        has(implied),
                        "{name}: rnstatus enters its {guard:?} branch and then indexes \
                         {implied:?} unguarded — serve both keys or neither"
                    );
                }
            }
            // :621/:646 guard on the four speed keys together and then index
            // arxs/atxs (announce) and prxs/ptxs (path request) unguarded.
            if ["prxs", "rxs", "ptxs", "txs"].iter().all(|k| has(k)) {
                for key in ["arxs", "atxs", "prxs", "ptxs"] {
                    assert!(
                        has(key),
                        "{name}: the prxs/rxs/ptxs/txs guard passes, so rnstatus indexes \
                         {key:?} unguarded in the -A/-P flow ratios"
                    );
                }
            }
        }

        // Top level: read unguarded on the default path, under --pps, and
        // under --queues (:718-745, :784-800).
        for key in [
            "rxb",
            "txb",
            "rxs",
            "txs",
            "rxpps",
            "txpps",
            "rxqt",
            "rxqd",
            "rxqa",
            "rxqp",
            "rxqil",
            "rxqtd",
            "rxqdd",
            "rxqad",
            "rxqpd",
            "rxqild",
            "tqpressure",
            "dqpressure",
            "aqpressure",
            "pqpressure",
            "ilqpressure",
        ] {
            assert!(
                top.contains_key(&HashableValue::String(key.into())),
                "rnstatus indexes stats[{key:?}] with no presence guard"
            );
        }
    }

    // Codeberg #140: the reported interface type must come from the transport
    // the interface was built over, not from its name. The driver names a
    // configured RNode interface `rnode_<idx>` (driver/mod.rs:1607) — a peer/
    // instance label that matches none of `interface_type`'s name prefixes, so
    // a LoRa radio is reported as the generic "Interface". That is not only a
    // wrong medium column in rnstatus: `itype` also picks the bitrate guess
    // (handlers.rs:341-345), so the radio is advertised at 10 Mbps.
    #[test]
    fn interface_type_comes_from_the_transport_not_the_name() {
        use crate::clock::SystemClock;
        use crate::interfaces::{InterfaceCounters, InterfaceStatsMap};
        use leviculum_core::node::NodeCoreBuilder;
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-iface-kind-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        // Exactly what the driver registers for `[[RNode LoRa Interface]]`.
        core.set_interface_name(0, "rnode_0".into());
        core.set_interface_kind(0, InterfaceKind::Rnode);

        let counters = Arc::new(InterfaceCounters::new());
        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::from([(0usize, counters)])));

        let value = build_interface_stats(
            &mut core,
            std::time::Instant::now(),
            &stats,
            &crate::interfaces::inventory::InterfaceInventory::shared(),
            0,
        );

        let Value::Dict(top) = value else {
            panic!("interface_stats must be a dict")
        };
        let Value::List(list) = top
            .get(&HashableValue::String("interfaces".into()))
            .expect("interfaces key")
        else {
            panic!("interfaces must be a list")
        };
        let Value::Dict(iface) = &list[0] else {
            panic!("interface entry must be a dict")
        };
        let get = |k: &str| iface.get(&HashableValue::String(k.into())).cloned();
        assert_eq!(
            get("type"),
            Some(Value::String("RNodeInterface".into())),
            "a radio interface must be reported as an RNode, not classified by its name label"
        );
    }

    // Codeberg #174: the transport_tables dump. Two things are pinned here —
    // that every table the transport maintains is present as its own key even
    // when empty (so a reader can tell "table is empty" from "daemon cannot
    // answer", which is what `merge_transport_tables` relies on), and that a
    // populated row carries the reference key names with the reference units.
    #[test]
    fn build_transport_tables_names_every_table_and_uses_reference_keys() {
        use crate::clock::SystemClock;
        use leviculum_core::constants::RANDOM_HASHBYTES;
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::storage_types::{AnnounceEntry, LinkEntry, PathEntry, ReverseEntry};
        use leviculum_core::traits::Storage as _;
        use std::collections::BTreeMap;

        let tmp = std::env::temp_dir().join(format!("rpc-tt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );
        core.set_interface_name(0, "TCPInterface[peer]".into());
        core.set_interface_name(1, "RNodeInterface[/dev/ttyUSB0]".into());

        let now = core.now_ms();
        let lifetime_ms = core.transport_config().path_expiry_secs * 1000;
        let dest = [0x11u8; 16];
        let via = [0x22u8; 16];
        // A random blob whose trailing 5 bytes are the emission timebase
        // (announce.rs::emission_from_random_hash / Transport.py:3191-3195).
        // Recomposed here independently of the writer: 0x0102030405 big-endian.
        let mut blob = [0u8; RANDOM_HASHBYTES];
        blob[5..].copy_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05]);
        let expected_emitted: i64 = 0x01_0203_0405;

        core.storage_mut().set_path(
            dest,
            PathEntry {
                hops: 3,
                expires_ms: now + lifetime_ms,
                interface_index: 0,
                random_blobs: vec![blob],
                next_hop: Some(via),
                via_peer: None,
            },
        );
        core.storage_mut().set_reverse(
            [0x33u8; 16],
            ReverseEntry {
                timestamp_ms: now,
                receiving_interface_index: 0,
                outbound_interface_index: 1,
            },
        );
        core.storage_mut().set_link_entry(
            [0x44u8; 16],
            LinkEntry {
                timestamp_ms: now,
                next_hop_interface_index: 1,
                remaining_hops: 2,
                received_interface_index: 0,
                hops: 1,
                validated: true,
                proof_timeout_ms: now + 5_000,
                destination_hash: dest,
                peer_signing_key: None,
            },
        );
        core.storage_mut().set_announce(
            dest,
            AnnounceEntry {
                timestamp_ms: now,
                hops: 3,
                retries: 1,
                retransmit_at_ms: Some(now + 1_000),
                raw_packet: vec![0xAB; 77],
                receiving_interface_index: 0,
                target_interface: Some(1),
                local_rebroadcasts: 2,
                block_rebroadcasts: true,
            },
        );
        core.storage_mut().set_announce_cache(dest, vec![0xCD; 42]);

        let value = build_transport_tables(&core, std::time::Instant::now());
        let Value::Dict(top) = &value else {
            panic!("transport_tables must be a dict")
        };
        let table = |k: &str| match top.get(&HashableValue::String(k.into())) {
            Some(Value::List(rows)) => rows.clone(),
            other => panic!("{k} must be a list, got {other:?}"),
        };

        // Every table is named, including the ones that are empty here. An
        // absent key must never be the way a reader learns a table is empty.
        for key in [
            "path_table",
            "reverse_table",
            "link_table",
            "announce_table",
            "announce_cache",
            "tunnels",
            "local_links",
        ] {
            assert!(
                top.contains_key(&HashableValue::String(key.into())),
                "transport_tables must always carry the {key} key, got {:?}",
                top.keys().collect::<Vec<_>>()
            );
        }
        assert!(
            table("tunnels").is_empty() && table("local_links").is_empty(),
            "no tunnel and no local link were seeded, so both tables are present and empty"
        );

        let row = |rows: &[Value], what: &str| -> BTreeMap<HashableValue, Value> {
            match rows.first() {
                Some(Value::Dict(d)) => d.clone(),
                other => panic!("{what} must hold one dict row, got {other:?}"),
            }
        };
        let field = |d: &BTreeMap<HashableValue, Value>, k: &str| -> Value {
            d.get(&HashableValue::String(k.into()))
                .unwrap_or_else(|| panic!("missing key {k}"))
                .clone()
        };

        // path_table: Python's own six RPC keys (Reticulum.py:1528-1535) plus
        // the additive announce_emitted.
        let p = row(&table("path_table"), "path_table");
        assert_eq!(field(&p, "hash"), Value::Bytes(dest.to_vec()));
        assert_eq!(field(&p, "via"), Value::Bytes(via.to_vec()));
        assert_eq!(field(&p, "hops"), Value::I64(3));
        assert_eq!(
            field(&p, "interface"),
            Value::String("TCPInterface[peer]".into())
        );
        assert_eq!(
            field(&p, "announce_emitted"),
            Value::I64(expected_emitted),
            "announce_emitted is what the PEER stamped, read out of the random blob"
        );
        // timestamp is OUR receipt time, back-computed from expires minus the
        // lifetime the path was given: expires - lifetime == now, so the two
        // epoch values must differ by exactly the lifetime.
        let (Value::F64(ts), Value::F64(exp)) = (field(&p, "timestamp"), field(&p, "expires"))
        else {
            panic!("timestamp and expires are Unix seconds as floats, like Python's time.time()")
        };
        assert!(
            (exp - ts - lifetime_ms as f64 / 1000.0).abs() < 0.001,
            "expires - timestamp must be the path lifetime ({lifetime_ms} ms), got {}",
            exp - ts
        );

        // reverse_table: field names after IDX_RT_* (Transport.py:3556-3558),
        // interfaces reported by name, not index.
        let r = row(&table("reverse_table"), "reverse_table");
        assert_eq!(field(&r, "hash"), Value::Bytes(vec![0x33u8; 16]));
        assert_eq!(
            field(&r, "receiving_interface"),
            Value::String("TCPInterface[peer]".into())
        );
        assert_eq!(
            field(&r, "outbound_interface"),
            Value::String("RNodeInterface[/dev/ttyUSB0]".into())
        );

        // link_table: the RELAYED links, field names after IDX_LT_*
        // (Transport.py:3572-3580). The hops/remaining_hops split is the pair
        // Codeberg #38 turned on.
        let l = row(&table("link_table"), "link_table");
        assert_eq!(field(&l, "link_id"), Value::Bytes(vec![0x44u8; 16]));
        assert_eq!(field(&l, "hops"), Value::I64(1));
        assert_eq!(field(&l, "remaining_hops"), Value::I64(2));
        assert_eq!(field(&l, "validated"), Value::Bool(true));
        assert_eq!(field(&l, "destination_hash"), Value::Bytes(dest.to_vec()));

        // announce_table: field names after IDX_AT_* (Transport.py:3561-3569);
        // the stored packet is reported as a length, not as bytes.
        let a = row(&table("announce_table"), "announce_table");
        assert_eq!(field(&a, "retries"), Value::I64(1));
        assert_eq!(field(&a, "packet_length"), Value::I64(77));
        assert_eq!(field(&a, "local_rebroadcasts"), Value::I64(2));
        assert_eq!(field(&a, "block_rebroadcasts"), Value::Bool(true));
        assert_eq!(
            field(&a, "attached_interface"),
            Value::String("RNodeInterface[/dev/ttyUSB0]".into())
        );

        // announce_cache: the known-destination cache, with the retain state
        // Codeberg #84 mirrors from known_destinations[dest][4].
        let c = row(&table("announce_cache"), "announce_cache");
        assert_eq!(field(&c, "hash"), Value::Bytes(dest.to_vec()));
        assert_eq!(field(&c, "packet_length"), Value::I64(42));
        assert_eq!(field(&c, "retained"), Value::Bool(false));
        core.storage_mut().retain_known_dest(&dest);
        let c = row(
            &match build_transport_tables(&core, std::time::Instant::now()) {
                Value::Dict(d) => match d.get(&HashableValue::String("announce_cache".into())) {
                    Some(Value::List(rows)) => rows.clone(),
                    other => panic!("announce_cache must be a list, got {other:?}"),
                },
                other => panic!("expected dict, got {other:?}"),
            },
            "announce_cache",
        );
        assert_eq!(
            field(&c, "retained"),
            Value::Bool(true),
            "retaining a destination must be visible in the dump"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // The `rnstatus -l` pair. Both verbs answer from the TRANSPORT link table
    // (`len(Transport.link_table)` and its validated subset, Reticulum 1.5.2
    // Transport.py:3211-3216), because that is the number the operator line
    // "N entries in link table (M active)" names. Before this test the pair
    // answered from the LOCAL links instead — a different table, a different
    // number under the same rendered line depending on the daemon.
    //
    // The seeded core terminates no link of its own, so an implementation
    // still reading the local-link count would answer 0/0 here: the 2/1 below
    // cannot be produced by the code this replaces.
    #[test]
    fn link_count_pair_answers_from_the_transport_link_table() {
        use crate::clock::SystemClock;
        use crate::rpc::pickle::{decode_response_msgpack, Codec, RpcRequest};
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::storage_types::LinkEntry;
        use leviculum_core::traits::Storage as _;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-lcpair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );

        let stats: InterfaceStatsMap = Arc::new(Mutex::new(Default::default()));
        let inventory = crate::interfaces::inventory::InterfaceInventory::shared();
        let ask = |core: &mut StdNodeCore, verb: RpcRequest| -> Value {
            let bytes = handle_request(
                &verb,
                core,
                std::time::Instant::now(),
                &stats,
                &inventory,
                0,
                None,
                Codec::Msgpack,
            )
            .expect("handle request");
            decode_response_msgpack(&bytes).expect("decode response")
        };

        // Idle: an empty relay table is 0/0, not an error and not a None.
        assert_eq!(ask(&mut core, RpcRequest::GetLinkCount), Value::I64(0));
        assert_eq!(
            ask(&mut core, RpcRequest::GetActiveLinkCount),
            Value::I64(0)
        );

        let now = core.now_ms();
        let entry = |validated: bool| LinkEntry {
            timestamp_ms: now,
            next_hop_interface_index: 1,
            remaining_hops: 2,
            received_interface_index: 0,
            hops: 1,
            validated,
            proof_timeout_ms: now + 5_000,
            destination_hash: [0x11u8; 16],
            peer_signing_key: None,
        };
        // The unvalidated id's eighth byte is non-zero and the validated id's
        // is zero, so upstream's key-indexing accident (Transport.py:3215-3216)
        // would answer 1 for exactly the wrong entry. Ours reads `validated`.
        let mut validated_id = [0x44u8; TRUNCATED_HASHBYTES];
        validated_id[7] = 0x00;
        let unvalidated_id = [0x55u8; TRUNCATED_HASHBYTES];
        core.storage_mut().set_link_entry(validated_id, entry(true));
        core.storage_mut()
            .set_link_entry(unvalidated_id, entry(false));

        assert_eq!(
            ask(&mut core, RpcRequest::GetLinkCount),
            Value::I64(2),
            "link_count is the size of the relay table, validated or not"
        );
        assert_eq!(
            ask(&mut core, RpcRequest::GetActiveLinkCount),
            Value::I64(1),
            "active_link_count is the validated subset — by the `validated` \
             flag, not by a byte of the link id"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Codeberg #174: the dump survives both codecs. `transport_tables` is a
    // nested dict-of-lists-of-dicts, deeper than any pre-existing response, so
    // the msgpack transcode is worth pinning rather than assuming.
    #[test]
    fn transport_tables_round_trips_through_both_codecs() {
        use crate::clock::SystemClock;
        use crate::rpc::pickle::{decode_response_msgpack, Codec, RpcRequest};
        use leviculum_core::node::NodeCoreBuilder;
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-tt-codec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::new()));
        let mut core: StdNodeCore = NodeCoreBuilder::new().enable_transport(true).build(
            rand_core::OsRng,
            SystemClock::new(),
            crate::storage::Storage::new(&tmp).unwrap(),
        );

        for codec in [Codec::Pickle, Codec::Msgpack] {
            let bytes = handle_request(
                &RpcRequest::GetTransportTables,
                &mut core,
                std::time::Instant::now(),
                &stats,
                &crate::interfaces::inventory::InterfaceInventory::shared(),
                0,
                None,
                codec,
            )
            .expect("transport_tables must serialize");
            let decoded = match codec {
                Codec::Pickle => {
                    serde_pickle::value_from_slice(&bytes, Default::default()).unwrap()
                }
                Codec::Msgpack => decode_response_msgpack(&bytes).unwrap(),
            };
            let Value::Dict(d) = decoded else {
                panic!("{codec:?}: transport_tables must decode to a dict")
            };
            for key in [
                "path_table",
                "reverse_table",
                "link_table",
                "announce_table",
                "announce_cache",
                "tunnels",
                "local_links",
            ] {
                assert!(
                    matches!(
                        d.get(&HashableValue::String(key.into())),
                        Some(Value::List(_))
                    ),
                    "{codec:?}: {key} must survive as a list"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // The identities listing (lnstatus --identities): a peer identity learned
    // from a received announce comes back with its identity hash, the
    // announced destination, the path columns — and the name ONLY when the
    // announce's name hash matches an aspect this daemon registered itself
    // (here: rnstransport.probe, via respond_to_probes). The foreign-named
    // announce of the SAME peer identity is the control: same identity hash,
    // name None. Both codecs.
    #[test]
    fn identity_table_lists_learned_identities_with_registered_names_only() {
        use crate::clock::SystemClock;
        use crate::interfaces::InterfaceStatsMap;
        use crate::rpc::pickle::{decode_response_msgpack, Codec, RpcRequest};
        use leviculum_core::constants::MTU;
        use leviculum_core::node::NodeCoreBuilder;
        use leviculum_core::transport::InterfaceId;
        use leviculum_core::{Destination, DestinationType, Direction, Identity};
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        let tmp = std::env::temp_dir().join(format!("rpc-identity-table-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut core: StdNodeCore = NodeCoreBuilder::new()
            .enable_transport(true)
            .respond_to_probes(true)
            .build(
                rand_core::OsRng,
                SystemClock::new(),
                crate::storage::Storage::new(&tmp).unwrap(),
            );
        core.set_interface_name(0, "tcp0".into());

        // One peer identity, two announced destinations: the probe aspect this
        // node itself registered, and a foreign app name it has never seen.
        let peer = Identity::generate(&mut rand_core::OsRng);
        let peer_hash = *peer.hash();
        let now_ms = core.now_ms();
        let announce_raw = |dest: &mut Destination| -> Vec<u8> {
            let ann = dest
                .announce(None, &mut rand_core::OsRng, now_ms, now_ms / 1000)
                .unwrap();
            let mut buf = [0u8; MTU];
            let len = ann.pack(&mut buf).unwrap();
            buf[..len].to_vec()
        };
        let mut probe_dest = Destination::new(
            Some(peer.clone()),
            Direction::In,
            DestinationType::Single,
            "rnstransport",
            &["probe"],
        )
        .unwrap();
        let mut foreign_dest = Destination::new(
            Some(peer),
            Direction::In,
            DestinationType::Single,
            "someforeignapp",
            &["delivery"],
        )
        .unwrap();
        let probe_hash = *probe_dest.hash();
        let foreign_hash = *foreign_dest.hash();
        let _ = core.handle_packet(InterfaceId(0), &announce_raw(&mut probe_dest));
        let _ = core.handle_packet(InterfaceId(0), &announce_raw(&mut foreign_dest));

        let stats: InterfaceStatsMap = Arc::new(Mutex::new(BTreeMap::new()));
        for codec in [Codec::Pickle, Codec::Msgpack] {
            let bytes = handle_request(
                &RpcRequest::GetIdentityTable,
                &mut core,
                std::time::Instant::now(),
                &stats,
                &crate::interfaces::inventory::InterfaceInventory::shared(),
                0,
                None,
                codec,
            )
            .expect("identities must serialize");
            let decoded = match codec {
                Codec::Pickle => {
                    serde_pickle::value_from_slice(&bytes, Default::default()).unwrap()
                }
                Codec::Msgpack => decode_response_msgpack(&bytes).unwrap(),
            };
            let Value::List(rows) = decoded else {
                panic!("{codec:?}: identities must decode to a list")
            };
            assert_eq!(rows.len(), 2, "{codec:?}: both announces listed");

            let field = |row: &Value, key: &str| -> Value {
                let Value::Dict(d) = row else {
                    panic!("{codec:?}: row must be a dict")
                };
                d.get(&HashableValue::String(key.into()))
                    .unwrap_or_else(|| panic!("{codec:?}: row missing key {key}"))
                    .clone()
            };
            let row_for = |dest: &[u8; 16]| -> &Value {
                rows.iter()
                    .find(|r| matches!(field(r, "destination_hash"), Value::Bytes(b) if b == dest))
                    .unwrap_or_else(|| panic!("{codec:?}: no row for the announced destination"))
            };

            let probe_row = row_for(probe_hash.as_bytes());
            assert_eq!(
                field(probe_row, "identity_hash"),
                Value::Bytes(peer_hash.to_vec()),
                "{codec:?}: identity hash is the truncated hash of the announced key"
            );
            assert_eq!(
                field(probe_row, "name"),
                Value::String("rnstransport.probe".into()),
                "{codec:?}: an aspect this node registered itself is named"
            );
            assert_eq!(field(probe_row, "hops"), Value::I64(1));
            assert_eq!(field(probe_row, "interface"), Value::String("tcp0".into()));
            assert_eq!(
                field(probe_row, "via"),
                Value::None,
                "{codec:?}: direct path, no relay hop"
            );
            assert!(
                matches!(field(probe_row, "last_seen"), Value::F64(t) if t > 0.0),
                "{codec:?}: last_seen carries the path timestamp"
            );

            let foreign_row = row_for(foreign_hash.as_bytes());
            assert_eq!(
                field(foreign_row, "identity_hash"),
                Value::Bytes(peer_hash.to_vec()),
                "{codec:?}: same peer identity behind the foreign name"
            );
            assert_eq!(
                field(foreign_row, "name"),
                Value::None,
                "{codec:?}: a name this node never registered stays unknown, not guessed"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
