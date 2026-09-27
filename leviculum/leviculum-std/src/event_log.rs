//! Structured event-log sink.
//!
//! This is a PRODUCTION component: `lnsd` installs it via
//! [`install_global_subscriber`] and routes events to an append-only
//! file when `LEVICULUM_EVENT_LOG=<path>` is set.  The same sink also
//! backs the mvr / integration test capture harness in
//! [`crate::test_support::event_log`], which layers per-test buffer
//! isolation on top of the process-global layer here.
//!
//! # Format
//!
//! Each emitted event renders to a single line in canonical form:
//!
//! ```text
//! EVENT_NAME node=<n> key1=val1 key2=val2 ... t=<rel-ms>
//! ```
//!
//! - `EVENT_NAME` = string value of the `event` field passed to
//!   `tracing::debug!(event = "FOO", ...)`.
//! - `node=` is reserved as the first field, sourced from
//!   `LEVICULUM_EVENT_NODE` (default `local`).
//! - All other fields are alphabetically sorted.
//! - `t=` is always last; relative milliseconds since the layer was
//!   registered (process-global init time).
//!
//! Records that do not carry an `event = "..."` field are silently
//! ignored — the legacy printf-style `tracing::debug!("[FOO] ...")`
//! sites stay compatible.
//!
//! # Architecture: process-global layer + active-handles list
//!
//! A single [`EventLogLayer`] is registered once in the process (by
//! [`install_global_subscriber`] in production, or by
//! [`crate::test_support::tracing_setup::init_tracing_with_event_log`]
//! under test).  All threads, including `tokio::test(multi_thread)`
//! workers, route events through it.
//!
//! Per-test buffer isolation is built on top of the global layer: an
//! [`EventLogHandle`] (created via the helpers in
//! [`crate::test_support::event_log`]) registers an `ActiveHandle` in
//! the layer's shared list.  `on_event` iterates the active list and
//! pushes the formatted line to every active buffer.  When the handle
//! drops, it removes itself from the list.
//!
//! Concurrency consequence: every active buffer receives every event,
//! regardless of which test emitted it.  Tests that assert on buffer
//! contents must filter by event name to avoid cross-test pollution.
//!
//! # Validation
//!
//! Two violation classes, both non-blocking — original event lines
//! are never suppressed.
//!
//! ## Schema validation (per-handle)
//!
//! [`EVENT_CATALOG`] declares required keys per event name.  A name
//! may appear under several entries when its emitters use per-reason
//! shapes (Codeberg #320); a record passes if any declared shape is
//! fully present.  Per consumed event, the layer iterates each active
//! handle's catalogue (production + the handle's `extra_schemas`).
//! If the event is catalogued and no declared shape is satisfied, a
//! synthetic line naming the nearest shape's missing keys is
//! appended to that handle's buffer:
//!
//! ```text
//! EVENT_SCHEMA_VIOLATION event=PKT_RX missing=[hops,len] caller=transport.rs:1074 t=<rel-ms>
//! ```
//!
//! ## Field-value validation (per-event, per-handle)
//!
//! Token-based parsers (the `jl`/`jldiff` tools) split lines on
//! whitespace.  Field values containing whitespace, `=`, or
//! non-printable characters break that contract.  The visitor detects
//! them and the layer emits one synthetic line per offending field,
//! into every active buffer:
//!
//! ```text
//! EVENT_FIELD_VIOLATION event=PKT_RX field=note value_problem=whitespace caller=transport.rs:1074 t=<rel-ms>
//! ```
//!
//! `value_problem` ∈ {`whitespace`, `equals`, `non_printable`}.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use crate::sync_ext::MutexRecover;
use std::time::Instant;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{filter::LevelFilter, fmt, Registry};

const NODE_ENV_VAR: &str = "LEVICULUM_EVENT_NODE";
const LOG_FILE_ENV_VAR: &str = "LEVICULUM_EVENT_LOG";

/// Schema for one structured event.  Declares the keys that MUST be
/// present on every emission of this event name.
pub struct EventSchema {
    pub name: &'static str,
    pub required_keys: &'static [&'static str],
}

/// Production catalogue — one entry per converted call site in
/// `leviculum-core/src/transport.rs`.  Adding entries without a
/// live emitter is the "stale catalogue" failure mode (Variant 3
/// can't detect it), so every entry here MUST have a corresponding
/// `tracing::debug!(event = "FOO", ...)` in production code.
pub const EVENT_CATALOG: &[EventSchema] = &[
    // Per-packet journey contract (Periculum phase 6): PKT_RX / PKT_TX /
    // PKT_FORWARD / PKT_DROP / DEDUP_DROP live on the dedicated
    // `leviculum_core::pkt` tracing target and carry `ph`, the first
    // 16 hex chars of the dedup packet hash, as the cross-node journey
    // correlator. This layer sees every record regardless of target.
    EventSchema {
        name: "PKT_RX",
        required_keys: &["iface", "type", "dst", "hops", "len", "ph"],
    },
    EventSchema {
        name: "PKT_TX",
        required_keys: &["iface", "hops", "len", "ph"],
    },
    EventSchema {
        name: "ANN_RX",
        required_keys: &["dst", "hops", "iface", "path_response"],
    },
    EventSchema {
        name: "PATH_ADD",
        required_keys: &[
            "dst",
            "hops",
            "iface",
            "next_hop",
            "ok",
            "source",
            "table_len",
        ],
    },
    EventSchema {
        name: "PKT_LOCAL",
        required_keys: &["dst", "iface", "matched"],
    },
    // Codeberg #365: the routing decision for an ORIGINATED packet, one
    // line per `send_to_destination`, so a log distinguishes "sent to a
    // live carrier" from "withheld because the path's interface is
    // offline" — the firmware's `[TELEMETRY] send` line is the same
    // statement on the debug port (docs/src/structured-event-logs.md).
    EventSchema {
        name: "OUTBOUND_ROUTE",
        required_keys: &["dst", "iface", "next_hop", "online"],
    },
    EventSchema {
        name: "OUTBOUND_WITHHELD",
        required_keys: &["dst", "iface", "next_hop", "reason"],
    },
    EventSchema {
        name: "PKT_DROP",
        required_keys: &["dst", "hops", "iface_in", "ph", "reason", "type"],
    },
    // Second PKT_DROP shape, for the one drop on the OUTBOUND path
    // (`dispatch_actions`, Codeberg #344). The action never reached an
    // interface, so there is no `iface_in`; it was never a parsed `Packet`
    // at this layer, so there is no `dst` or `type`. What it does carry is
    // the interface it was ADDRESSED to, which is the whole diagnosis.
    EventSchema {
        name: "PKT_DROP",
        required_keys: &["hops", "iface_out", "len", "ph", "reason"],
    },
    EventSchema {
        name: "PKT_FORWARD",
        required_keys: &[
            "dst",
            "hops",
            "iface_in",
            "iface_out",
            "next_hop",
            "ph",
            "type",
        ],
    },
    // Codeberg #66 observability: duplicate-hash drop in
    // process_incoming. Deliberately its own event name (not
    // PKT_DROP reason=duplicate): PKT_DROP's schema is
    // forwarding-specific (iface_in/iface_out pairing), and the
    // #66 failure class must be greppable by name alone.
    EventSchema {
        name: "DEDUP_DROP",
        required_keys: &["dst", "iface", "ph", "type", "context"],
    },
    // Codeberg #50 Bug-A forensic instrumentation.  Emitted by
    // periculum's `src/runner.rs::silence_unused_lnode` at function
    // entry and at every exit branch; lets jl/jldiff diff between
    // RNode-only and T114-involved scenarios for any future hang.
    EventSchema {
        name: "SILENCE_LNODE_ENTER",
        required_keys: &["usb_serial", "port_path"],
    },
    EventSchema {
        name: "SILENCE_LNODE_EXIT",
        required_keys: &["usb_serial", "port_path", "result"],
    },
    // Stage-6 catalogue expansion (Codeberg #39 piece 5 follow-up).
    // Promotes the remaining `[TAG] k=v ...` printf-style sites to
    // structured events.  See `docs/src/structured-event-logs.md`
    // for the convention.
    EventSchema {
        name: "EMB_EVICT",
        required_keys: &["cap", "len_before", "map"],
    },
    EventSchema {
        name: "EMB_INSERT_FAIL",
        required_keys: &["cap", "len_after_evict", "map"],
    },
    EventSchema {
        name: "IDENTITY",
        required_keys: &["node"],
    },
    EventSchema {
        name: "PATH_LOOKUP",
        // `hops` and `iface` are present only on the `found=true`
        // branch; only `dst` and `found` are required across both
        // call sites.
        required_keys: &["dst", "found"],
    },
    EventSchema {
        name: "PATH_TABLE",
        required_keys: &["size"],
    },
    EventSchema {
        name: "PATH_TABLE_ENTRY",
        required_keys: &["dst", "expires_in_ms", "hops", "iface", "next_hop"],
    },
    EventSchema {
        name: "PROOF_GEN",
        required_keys: &["for_pkt", "to_dst"],
    },
    EventSchema {
        name: "PROOF_SEND",
        required_keys: &["iface", "pkt"],
    },
    EventSchema {
        name: "REVERSE_ADD",
        required_keys: &["in_iface", "out_iface", "pkt_hash"],
    },
    // Drop observability for the NodeEvent application channels (Codeberg
    // #71). Emitted by `EventSink` in leviculum-std/src/driver/mod.rs.
    // EVENT_CHANNEL_FULL now fires only for the lossless control plane (a
    // dropped event that will be surfaced via ControlPlaneOverflow); data
    // drops are silent backpressure. dropped_event_type carries
    // NodeEvent::variant_name() so saturation is greppable per-event-type.
    EventSchema {
        name: "EVENT_CHANNEL_FULL",
        required_keys: &["queue_capacity", "dropped_event_type"],
    },
    EventSchema {
        name: "EVENT_CHANNEL_CLOSED",
        required_keys: &["dropped_event_type"],
    },
    // Control-plane overflow marker delivery: how many control events were
    // dropped since the last marker. Pairs with NodeEvent::ControlPlaneOverflow.
    EventSchema {
        name: "CONTROL_PLANE_OVERFLOW",
        required_keys: &["dropped_count"],
    },
    // OBS-1: announce rebroadcast made observable. ANN_TX fires when the node
    // actually (re)transmits a stored announce on an interface (pairs with
    // ANN_RX); ANN_TX_SUPPRESSED fires when an airtime cap held the rebroadcast
    // back, so a suppressed announce is also visible without claiming a TX.
    EventSchema {
        name: "ANN_TX",
        required_keys: &["dst", "hops", "iface"],
    },
    EventSchema {
        name: "ANN_TX_SUPPRESSED",
        required_keys: &["dst", "hops", "iface", "suppressed", "reason"],
    },
    // OBS-3 (Codeberg #114): endpoint observability. A node acting as the
    // ENDPOINT of a link (accepting an inbound link, delivering locally,
    // generating the establishment proof, answering a remote-management
    // request) previously emitted no structured events -- only the RELAY path
    // (PKT_FORWARD, relay-side LINK_ENTRY_SET) was instrumented. LINK_LOCAL is
    // the endpoint counterpart to LINK_ENTRY_SET: it fires when we accept an
    // inbound link for one of our own destinations (never for a relayed link).
    // The establishment proof reuses PROOF_GEN/PROOF_SEND, and local delivery
    // reuses PKT_LOCAL (all already catalogued above).
    EventSchema {
        name: "LINK_LOCAL",
        required_keys: &["dst", "iface", "link"],
    },
    // OBS-3: the remote-management (and any request/response) responder path.
    // REQUEST_RX fires when an authorized request is dispatched to a handler
    // (pairs with the RequestReceived NodeEvent); RESPONSE_TX fires when the
    // responder sends the single-packet reply back over the link.
    EventSchema {
        name: "REQUEST_RX",
        required_keys: &["link", "path_hash", "request_id"],
    },
    EventSchema {
        name: "RESPONSE_TX",
        required_keys: &["len", "link", "request_id"],
    },
    // OBS-2: periodic per-reason drop summary at the PATH_TABLE cadence (~10s).
    // Surfaces the always-on drop counters (including the high-volume overheard
    // path) without per-packet flooding.
    EventSchema {
        name: "PKT_DROP_SUMMARY",
        required_keys: &[
            "overheard_transport_id",
            "invalid_announce",
            "plain_group_multihop",
            "no_path",
            "ifac",
            "duplicate",
            "announce_over_max_hops",
            "announce_replay",
            "announce_rate_limited",
            "ingress_burst_announce",
            "lrproof_invalid",
            "link_repeat_echo",
            "forward_max_hops",
            "blackholed_announce",
            "single_decrypt_fail",
            "group_decrypt_fail",
            "unknown_context",
            "no_such_interface",
            "total",
        ],
    },
    // RNode CMD_READY flow control under the firmware duty lock.
    // Emitted by `interfaces/rnode.rs::rnode_io_task`. GATED fires once the
    // gate has held queued frames past one CHTM cadence and repeats at a
    // bounded rate; RELEASED closes the pair when the gate reopens;
    // QUEUE_DROP names frames the host-side queue loses, and `reason` says
    // how:
    //   reason=queue_full  — the bounded queue shed its oldest frame;
    //                        `len` = that frame's payload bytes, `depth` =
    //                        what stays queued. One event per frame.
    //   any other reason   — a return path of the io task abandoned its
    //                        task-local queue on disconnect (serial_eof,
    //                        device_reset, error_*, …); `len` = frames
    //                        abandoned, `depth` = 0. One event per return
    //                        path, so a reconnect cannot emit 64 lines at
    //                        once.
    // The Columba BLE interface (`interfaces::ble`). BLE_SCAN_DECISION
    // carries the exact fields the firmware's line of the same name does
    // (leviculum-nrf ble/columba.rs), so a merged rig timeline correlates
    // the two sides of one decision; the link lifecycle events carry the
    // same `peer=<hex8>` the firmware logs. Emitted once per
    // (address, decision) change, not per advertising PDU.
    EventSchema {
        name: "BLE_SCAN_DECISION",
        required_keys: &["addr", "caps", "caps_record", "initiate", "rule"],
    },
    EventSchema {
        name: "BLE_LINK_UP",
        required_keys: &["iface", "peer", "addr", "role", "mtu"],
    },
    EventSchema {
        name: "BLE_LINK_DOWN",
        required_keys: &["iface", "peer", "role", "reason"],
    },
    EventSchema {
        name: "BLE_LINK_DUP",
        required_keys: &["peer", "addr", "action"],
    },
    EventSchema {
        name: "BLE_LINK_SELF",
        required_keys: &["addr", "action"],
    },
    EventSchema {
        name: "BLE_TX_FANOUT_DROP",
        required_keys: &["iface", "peer", "len", "depth"],
    },
    // What the core's #376 delivery hint made of one outbound packet.
    // BLE_TX_ROUTE names the one peer the packet was addressed to and
    // the role of the link it went on; BLE_TX_FLOOD is the broadcast
    // case with the number of live links it reached; BLE_TX_ROUTE_MISS
    // is a routed packet dropped because the addressed peer holds no
    // live link here. Exactly one of the three per outbound packet, so
    // a capture accounts for every packet the interface was handed.
    EventSchema {
        name: "BLE_TX_ROUTE",
        required_keys: &["iface", "peer", "conn", "len"],
    },
    EventSchema {
        name: "BLE_TX_FLOOD",
        required_keys: &["iface", "links", "len"],
    },
    EventSchema {
        name: "BLE_TX_ROUTE_MISS",
        required_keys: &["iface", "peer", "len"],
    },
    // A link's reassembly was discarded before completion (#373): a
    // torn or interleaved fragment stream from the peer cost `lost`
    // whole Reticulum packets, `total` is the link's running count.
    // Same name as the firmware's line (slot-keyed there), so a merged
    // bench timeline carries both receivers.
    EventSchema {
        name: "BLE_RX_ABANDON",
        required_keys: &["iface", "peer", "lost", "total"],
    },
    EventSchema {
        name: "RNODE_TX_GATED",
        required_keys: &["iface", "held_ms", "depth"],
    },
    EventSchema {
        name: "RNODE_TX_RELEASED",
        required_keys: &["iface", "held_ms", "depth"],
    },
    // RNODE_TX_QUEUE_DROP carries two shapes keyed by `reason`
    // (Codeberg #320): `queue_full` sheds one frame and reports its
    // payload size as `len` bytes, while the abandon reasons (a dying
    // io task, #316) report how many whole frames were lost as
    // `frames`. Both entries share the name; a record passes if either
    // shape is fully present (see the any-shape rule in `on_event`).
    EventSchema {
        name: "RNODE_TX_QUEUE_DROP",
        required_keys: &["iface", "len", "depth", "reason"],
    },
    EventSchema {
        name: "RNODE_TX_QUEUE_DROP",
        required_keys: &["iface", "frames", "depth", "reason"],
    },
    // `lnmsg`, the LXMF messenger. Its emitting sites are in `lnmsg/src/events.rs`
    // rather than in this workspace member: the catalogue is one global list by
    // design (the layer looks a name up here whatever crate raised it), and a
    // second per-crate catalogue would mean two places to keep a name's required
    // keys. Every entry below has a live emitter, as this file's "How to add an
    // event" rule requires.
    EventSchema {
        name: "LNMSG_SENDER",
        required_keys: &["from", "source"],
    },
    EventSchema {
        name: "LNMSG_ATTACHED",
        required_keys: &["instance", "address"],
    },
    EventSchema {
        name: "LNMSG_RESOLVED",
        required_keys: &["dst", "waited_ms"],
    },
    EventSchema {
        name: "LNMSG_ENQUEUED",
        required_keys: &["id", "dst", "bytes", "via"],
    },
    EventSchema {
        name: "LNMSG_STATE",
        required_keys: &["id", "state"],
    },
    EventSchema {
        name: "LNMSG_DONE",
        required_keys: &["id", "outcome", "code"],
    },
];

/// Where the buffer is dumped on a panicking drop.
///
/// `pub(crate)` so the test-capture helpers in
/// [`crate::test_support::event_log`] can select stderr vs file when
/// building a handle via [`new_handle`].
pub(crate) enum DumpTarget {
    Stderr,
    File(PathBuf),
}

/// Per-handle bookkeeping kept in the layer's active list.
struct ActiveHandle {
    buffer: Arc<Mutex<Vec<String>>>,
    extra_schemas: &'static [EventSchema],
}

/// Handle returned from the test-capture helpers in
/// [`crate::test_support::event_log`].  While alive, every event the
/// global layer sees is appended to `buffer`.  On drop, the handle
/// removes itself from the layer's active list and — if the thread is
/// panicking — dumps the buffer to the configured target.
pub struct EventLogHandle {
    buffer: Arc<Mutex<Vec<String>>>,
    dump_target: DumpTarget,
    /// Reference to the layer's shared active-handles list, used by
    /// `Drop` to remove this handle's entry.
    active: Arc<Mutex<Vec<ActiveHandle>>>,
}

impl EventLogHandle {
    /// Snapshot the current buffer.  Useful for assertions in
    /// non-panicking tests.  Other parallel tests may have
    /// contributed lines — filter by event name.
    pub fn dump(&self) -> Vec<String> {
        self.buffer.lock_recover().clone()
    }
}

impl Drop for EventLogHandle {
    fn drop(&mut self) {
        // Remove our active entry first so subsequent events don't
        // race against a partly-torn-down handle.
        if let Ok(mut active) = self.active.lock() {
            active.retain(|h| !Arc::ptr_eq(&h.buffer, &self.buffer));
        }

        if !std::thread::panicking() {
            return;
        }

        let buffer = self.buffer.lock_recover();
        let body = buffer.join("\n");
        let dump = format!(
            "=== EVENT LOG DUMP (test panicked, {} lines) ===\n{}\n=== END EVENT LOG DUMP ===\n",
            buffer.len(),
            body,
        );
        match &self.dump_target {
            DumpTarget::Stderr => eprintln!("{dump}"),
            DumpTarget::File(p) => {
                // Best-effort — failure to write the dump must not
                // shadow the original panic.
                let _ = std::fs::write(p, dump);
            }
        }
    }
}

/// Register a fresh capture handle in the global layer's active list
/// and return it.  The caller is responsible for having installed the
/// global subscriber first (production via [`install_global_subscriber`],
/// tests via `tracing_setup::init_tracing_with_event_log`).
///
/// `pub(crate)`: the public test-capture entry points
/// (`init_event_log`, …) live in [`crate::test_support::event_log`] and
/// delegate here after ensuring the subscriber is installed.
pub(crate) fn new_handle(
    dump_target: DumpTarget,
    extra_schemas: &'static [EventSchema],
) -> EventLogHandle {
    let buffer: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let active = active_list();
    {
        let mut list = active.lock_recover();
        list.push(ActiveHandle {
            buffer: Arc::clone(&buffer),
            extra_schemas,
        });
    }
    EventLogHandle {
        buffer,
        dump_target,
        active: Arc::clone(active),
    }
}

/// Build the layer used by the global subscriber installers.  One
/// global layer per process; the active-handles list it owns is shared
/// with every [`EventLogHandle`] via `active_list`.
pub fn layer() -> EventLogLayer {
    EventLogLayer {
        active: Arc::clone(active_list()),
        init_time: Instant::now(),
    }
}

/// Global active-handles list.  Lazily allocated on first access so
/// the order of subscriber install and handle registration doesn't
/// matter.
fn active_list() -> &'static Arc<Mutex<Vec<ActiveHandle>>> {
    static ACTIVE: OnceLock<Arc<Mutex<Vec<ActiveHandle>>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
}

/// Process-wide node identifier.  Read from `LEVICULUM_EVENT_NODE`
/// at first access; defaults to `local`.  Cached via OnceLock so
/// every emitted event gets a consistent prefix even if the env
/// changes mid-run.
fn node_name() -> &'static str {
    static NODE: OnceLock<String> = OnceLock::new();
    NODE.get_or_init(|| std::env::var(NODE_ENV_VAR).unwrap_or_else(|_| "local".to_string()))
}

/// Process-wide append-only event log file.  Returns `Some` only when
/// `LEVICULUM_EVENT_LOG=<path>` is set in the environment at first
/// access AND the file opens successfully.  Cached via OnceLock so
/// the env-var lookup + file-open happens exactly once per process.
///
/// An open failure is reported ONCE, on stderr, then cached as `None`
/// (L-0022). stderr rather than `tracing` is deliberate twice over:
/// this init runs inside the layer's own `on_event` (see the caller),
/// where emitting a tracing event would re-enter the dispatcher while
/// the OnceLock is mid-init; and a broken `LEVICULUM_EVENT_LOG` must
/// be visible even in a process that installs no subscriber. No retry:
/// the path is fixed by the environment for the process lifetime, its
/// failure modes (permissions, missing directory) do not self-heal,
/// and a retry would put a failing `open(2)` on every event emission
/// inside the tracing hot path.
fn event_log_file() -> Option<&'static Mutex<File>> {
    static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        std::env::var(LOG_FILE_ENV_VAR).ok().and_then(|p| {
            match OpenOptions::new().create(true).append(true).open(&p) {
                Ok(f) => Some(Mutex::new(f)),
                Err(e) => {
                    eprintln!(
                        "{LOG_FILE_ENV_VAR}={p}: cannot open event log: {e} — \
                         event logging disabled for this process"
                    );
                    None
                }
            }
        })
    })
    .as_ref()
}

/// Read every input file as text, parse the trailing `t=<n>` token of
/// each non-empty line, and return all lines sorted by parsed `n`
/// (stable on tie).  Lines that fail `t=` parsing sort to the end
/// with a synthetic timestamp of `u128::MAX`, preserving their
/// relative order.
pub fn merge_event_logs(paths: &[PathBuf]) -> Vec<String> {
    let mut lines: Vec<(u128, usize, String)> = Vec::new();
    let mut tie_breaker: usize = 0;
    for path in paths {
        let Ok(file) = File::open(path) else { continue };
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let t = parse_t(&line).unwrap_or(u128::MAX);
            lines.push((t, tie_breaker, line));
            tie_breaker += 1;
        }
    }
    lines.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    lines.into_iter().map(|(_, _, l)| l).collect()
}

fn parse_t(line: &str) -> Option<u128> {
    line.split_whitespace()
        .rev()
        .find_map(|tok| tok.strip_prefix("t="))
        .and_then(|n| n.parse::<u128>().ok())
}

/// Install a global tracing subscriber for production daemons (`lnsd`,
/// the helper bin, etc.) that combines a standard fmt layer with the
/// event-log layer when `LEVICULUM_EVENT_LOG` is set.  Unset → only
/// fmt installed; runtime overhead matches the previous standalone
/// `tracing_subscriber::fmt().init()` call.
///
/// `default_filter` is the env-filter directive used when `RUST_LOG`
/// is unset (e.g. `"info"`, `"debug"`, …).
pub fn install_global_subscriber(default_filter: &str) {
    let level_filter = match default_filter.to_lowercase().as_str() {
        "error" => LevelFilter::ERROR,
        "warn" => LevelFilter::WARN,
        "debug" => LevelFilter::DEBUG,
        "trace" => LevelFilter::TRACE,
        "off" => LevelFilter::OFF,
        _ => LevelFilter::INFO,
    };
    let fmt_layer = fmt::layer().compact().with_filter(level_filter);
    if std::env::var(LOG_FILE_ENV_VAR).is_ok() {
        let _ = Registry::default().with(fmt_layer).with(layer()).try_init();
    } else {
        let _ = Registry::default().with(fmt_layer).try_init();
    }
}

/// The layer registered into the global subscriber chain.  Driven by
/// the active-handles list above.
pub struct EventLogLayer {
    active: Arc<Mutex<Vec<ActiveHandle>>>,
    init_time: Instant,
}

impl EventLogLayer {
    fn caller(&self, event: &Event<'_>) -> String {
        let meta = event.metadata();
        match (meta.file(), meta.line()) {
            (Some(f), Some(l)) => {
                let basename = f.rsplit('/').next().unwrap_or(f);
                format!("{basename}:{l}")
            }
            _ => "?".to_string(),
        }
    }
}

impl<S: Subscriber> Layer<S> for EventLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);

        let Some(event_name) = visitor.event_name else {
            return;
        };

        let t_ms = self.init_time.elapsed().as_millis();

        // Build the canonical line.  node= reserved as the first
        // field, sourced from LEVICULUM_EVENT_NODE (default "local").
        // Other fields alphabetical; t= last.
        let mut line = String::with_capacity(64);
        line.push_str(&event_name);
        line.push(' ');
        line.push_str("node=");
        line.push_str(node_name());
        for (k, v) in &visitor.fields {
            // `node` from a tracing call would conflict with the
            // reserved prefix — env-var wins, user-supplied skipped.
            if k == "node" {
                continue;
            }
            line.push(' ');
            line.push_str(k);
            line.push('=');
            line.push_str(v);
        }
        line.push_str(&format!(" t={t_ms}"));

        let caller = self.caller(event);

        // Build the field-violation lines once; they have no per-
        // handle component, so all consumers (file + every active
        // buffer) receive the same text.
        let field_violation_lines: Vec<String> = visitor
            .field_violations
            .iter()
            .map(|(field, problem)| {
                format!(
                    "EVENT_FIELD_VIOLATION event={} field={} value_problem={} caller={} t={}",
                    event_name, field, problem, caller, t_ms,
                )
            })
            .collect();

        // Process-wide append-only file (when LEVICULUM_EVENT_LOG is
        // set).  Production daemons + helper bin write here.  Schema
        // violations are per-handle so they don't appear in the file.
        if let Some(file) = event_log_file() {
            if let Ok(mut f) = file.lock() {
                let _ = writeln!(f, "{line}");
                for v in &field_violation_lines {
                    let _ = writeln!(f, "{v}");
                }
                let _ = f.flush();
            }
        }

        // Distribute to every active handle.  Per-handle:
        //   1. push the canonical line
        //   2. push one EVENT_FIELD_VIOLATION per offending field
        //   3. push one EVENT_SCHEMA_VIOLATION if the handle's
        //      catalogue (production + extra_schemas) declares this
        //      event with required keys absent from the record.
        let active = self.active.lock_recover();
        for handle in active.iter() {
            let mut buf = handle.buffer.lock_recover();
            buf.push(line.clone());
            if buf.len() > 1000 { buf.remove(0); }

            for v in &field_violation_lines {
                buf.push(v.clone());
                if buf.len() > 1000 { buf.remove(0); }
            }

            // A name may be catalogued under several shapes (e.g. the
            // per-reason field split of RNODE_TX_QUEUE_DROP, Codeberg
            // #320): the record passes if ANY declared shape is fully
            // present. The violation reports the nearest shape — the
            // one with the fewest missing keys — which for the common
            // single-shape event is just that shape's missing list.
            let mut satisfied = false;
            let mut nearest_missing: Option<Vec<&str>> = None;
            for s in EVENT_CATALOG
                .iter()
                .chain(handle.extra_schemas.iter())
                .filter(|s| s.name == event_name)
            {
                let missing: Vec<&str> = s
                    .required_keys
                    .iter()
                    .filter(|k| !visitor.fields.contains_key(**k))
                    .copied()
                    .collect();
                if missing.is_empty() {
                    satisfied = true;
                    break;
                }
                match &nearest_missing {
                    Some(prev) if prev.len() <= missing.len() => {}
                    _ => nearest_missing = Some(missing),
                }
            }
            if !satisfied {
                if let Some(missing) = nearest_missing {
                    let v = format!(
                        "EVENT_SCHEMA_VIOLATION event={} missing=[{}] caller={} t={}",
                        event_name,
                        missing.join(","),
                        caller,
                        t_ms,
                    );
                    buf.push(v);
                    if buf.len() > 1000 { buf.remove(0); }
                }
            }
        }
    }
}

/// Fields whose value is a human/discovery-provided *name* rather than a
/// structured token.  Interface names legitimately carry whitespace
/// (auto-connect names a discovered node's interface after it, e.g.
/// `autoconnect/Dark Doodad 23`), so a space in one of these is expected
/// input, not a source-bug symptom.  Their values are still coerced into a
/// tokenizable scalar for the line, but the advisory
/// `EVENT_FIELD_VIOLATION` is suppressed so the legitimate case does not
/// flood the log or poison the self-alarm metric (Codeberg #113).  Genuinely
/// structured fields keep the detector (still catches freetext-leak bugs
/// like BUG-1).
fn is_name_field(field: &str) -> bool {
    matches!(
        field,
        "iface" | "iface_in" | "iface_out" | "in_iface" | "out_iface"
    )
}

/// Detect non-scalar values that would break a whitespace/`=`-token
/// parser.  Returns the kind of problem, or `None` if safe.
fn field_value_problem(value: &str) -> Option<&'static str> {
    for c in value.chars() {
        if c.is_ascii_whitespace() {
            return Some("whitespace");
        }
        if c == '=' {
            return Some("equals");
        }
        if !c.is_ascii_graphic() {
            return Some("non_printable");
        }
    }
    None
}

/// Coerce a field value into a whitespace-free scalar so the canonical
/// line is ALWAYS tokenizable by the documented whitespace `key=val`
/// parser, no matter what an emission site passed.  Internal whitespace,
/// embedded `=`, and non-graphic bytes all become `_`.
///
/// This is the by-construction safety net behind the advisory
/// `EVENT_FIELD_VIOLATION`: the violation still fires (so the source bug
/// gets surfaced and fixed), but the emitted line is parseable regardless
/// (no stray bare tokens, no key collision from an embedded `=`).
fn sanitize_scalar(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_whitespace() || c == '=' || !c.is_ascii_graphic() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Reduce a `Debug` rendering to a scalar token: unwrap a single
/// `Some(...)` wrapper and strip the surrounding string-quote pair that
/// `Debug` adds to string-like values, so a value like `Some("373e…")`
/// renders as the bare `373e…` instead of leaking Rust Debug syntax into
/// the line.  `None` and other enum variants pass through unchanged
/// (they are already whitespace-free scalars; `None` is NOT collapsed to
/// empty because legitimate enum variants are also named `None`, e.g.
/// `PacketContext::None`).  Any residual whitespace/`=` is handled by
/// [`sanitize_scalar`] at record time.
fn normalize_debug(raw: &str) -> String {
    let s = raw.trim();
    let s = s
        .strip_prefix("Some(")
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(s);
    s.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

#[derive(Default)]
struct EventVisitor {
    event_name: Option<String>,
    fields: BTreeMap<String, String>,
    field_violations: Vec<(String, &'static str)>,
}

impl EventVisitor {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "event" {
            self.event_name = Some(value);
            return;
        }
        // BUG-3: a non-scalar value is reported AND sanitized, so the
        // canonical line stays well-formed by construction even when an
        // emission site passes whitespace or an embedded `=`.
        //
        // #113: name-type fields (interface names) legitimately carry
        // whitespace, so they are coerced SILENTLY -- sanitized for the
        // line, but no violation, to keep the false-positive flood out of
        // the log.  Structured fields keep the detector.
        let value = match field_value_problem(&value) {
            Some(problem) => {
                if !is_name_field(field.name()) {
                    self.field_violations
                        .push((field.name().to_string(), problem));
                }
                sanitize_scalar(&value)
            }
            None => value,
        };
        self.fields.insert(field.name().to_string(), value);
    }
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, value.to_string());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // BUG-2: emit a bare scalar (e.g. `373e…`), not Rust Debug
        // wrapper syntax (`Some("373e…")`). `trim_matches('"')` used to
        // strip the quotes that kept a Debug string parseable, leaking
        // its spaces into the line; `normalize_debug` unwraps the
        // wrapper instead.
        self.record(field, normalize_debug(&format!("{value:?}")));
    }
}
