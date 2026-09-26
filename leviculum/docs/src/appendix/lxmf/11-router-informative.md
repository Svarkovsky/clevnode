# Router internals (informative)

Everything in this section is **informative**. It documents how the reference
router behaves so an implementation can match observable timing and limits where
useful, but an implementation MAY diverge from any of it without breaking wire or
semantic compatibility. The normative obligations are in the preceding sections.

## Delivery scheduling (`LXMRouter.py:30-83`)

| Constant | Value | Meaning |
|----------|-------|---------|
| `MAX_DELIVERY_ATTEMPTS` | 5 | retries before a message fails |
| `PROCESSING_INTERVAL` | 4 s | jobloop tick |
| `DELIVERY_RETRY_WAIT` | 10 s | wait between delivery attempts |
| `PATH_REQUEST_WAIT` | 7 s | wait after a path request |
| `MAX_PATHLESS_TRIES` | 1 | sends attempted before forcing a path request |
| `LINK_MAX_INACTIVITY` | 600 s | idle link teardown |
| `P_LINK_MAX_INACTIVITY` | 180 s | idle propagation link teardown |

## Expiry and limits

| Constant | Value | Meaning |
|----------|-------|---------|
| `MESSAGE_EXPIRY` | 30 days | propagation store retention |
| `STAMP_COST_EXPIRY` | 45 days | cached outbound stamp cost retention |
| `PROPAGATION_LIMIT` | 256 KB | per-transfer propagation limit |
| `SYNC_LIMIT` | 256*40 KB | per-sync cumulative limit |
| `DELIVERY_LIMIT` | 1000 KB | direct delivery resource limit |
| `PN_STAMP_THROTTLE` | 180 s | propagation stamp throttle window |

## Peering (`LXMRouter.py:43-60`)

`MAX_PEERS` = 20, `AUTOPEER` = true, `AUTOPEER_MAXDEPTH` = 4 hops,
`ROTATION_HEADROOM_PCT` = 10, `ROTATION_AR_MAX` = 0.5, `PEERING_COST` = 18
(max 26), `PROPAGATION_COST` = 16 (min 13, flex 3). Peer selection and rotation
policy are internal.

## Jobloop cadence (`LXMRouter.py:853-860`)

A single jobloop dispatches staggered jobs: outbound processing (1 s), deferred
stamp generation (1 s), link cleanup (1 s), transient-cache cleanup (60 s),
message-store cleanup (120 s), peer sync (6 s), peer rotation (`56 *` peer-ingest
interval). An implementation may use any scheduler.

## Persistence

The Python reference persists to a writable storage path: the message store
(one file per propagation message, named
`transient_id_timestamp_stampvalue`), `peers`, `available_tickets`,
`outbound_stamp_costs`, `locally_delivered_transient_ids`,
`locally_processed_transient_ids`, and `node_stats`. The on-disk encoding
(mostly msgpack) and file naming are implementation choices; only the messages
and announces these structures drive onto the wire are normative.

`leviculum-lxmf` checkpoints only its client/router state through
`LxmfStorage`: outbound direct/opportunistic messages, delivered and processed
IDs (including downloaded transient IDs), stamp costs, tickets, ignored
destinations, and the next job deadline. It has no propagation message store,
peer records, node statistics, or node-hosting persistence.

## Python peer sync state machine (not implemented; `LXMPeer.py:17-22`)

A propagation peer progresses `IDLE -> LINK_ESTABLISHING -> LINK_READY ->
REQUEST_SENT -> RESPONSE_RECEIVED -> RESOURCE_TRANSFERRING -> IDLE`, with
`STRATEGY_LAZY`/`STRATEGY_PERSISTENT` (default persistent) controlling whether a
peer keeps syncing while unhandled messages remain. The states are local; the
wire payloads they produce (`/offer`, the sync Resource) are documented for
Python-reference completeness ([Propagation](10-propagation.md)); they are not
part of the Rust client implementation.
