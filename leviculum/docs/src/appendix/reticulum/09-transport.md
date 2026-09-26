# Transport

Transport routes packets across the mesh: it discovers paths, forwards packets
toward known next hops, and propagates announces. The **wire surfaces** (path
request and path response packets) are normative and proven by
`[VEC-PATH-REQUEST]`. The routing **internals** (tables, retransmission timing,
deduplication, TTLs) are informative.

## Path request

To discover a path, a node sends a path request: a DATA packet to the PLAIN
destination `rnstransport.path.request` (`Transport.request_path`,
`Transport.py:2786-2787`). The payload is (`Transport.py:2783-2784`):

```
target_destination_hash(16) || [transport_identity_hash(16)] || request_tag
```

The `transport_identity_hash` is included only when transport is enabled on the
requesting node; the `request_tag` is a random hash that deduplicates the request
(`Transport.py:2780`). `[VEC-PATH-REQUEST]` is a **frozen-injection** vector with
transport disabled: payload `target(16) || tag(16)`, in a PLAIN HEADER_1 packet
(flags `08`). An implementation MUST address the request to this PLAIN
destination and use this payload order so existing nodes answer it.

## Path response

A node that holds a path answers by re-emitting the destination's cached announce
with the packet context set to `PATH_RESPONSE` (0x0B) instead of `NONE`
(`Transport.py:595-596`). The announce data is byte-identical to an ordinary
announce (see [Announce](05-announce.md)); only the context differs, and the
rebroadcast is marked so it is not propagated further.

## Routing internals (informative)

The following are reference behaviour an implementation MAY diverge from:

- **Tables.** Transport maintains path, announce, reverse, link, and tunnel
  tables, each a list indexed by the `IDX_*` constants (`Transport.py:3547-3586`).
  A path entry holds timestamp, next hop, hops, expiry, the announce random
  blobs, the receiving interface, and the cached packet hash.
- **Announce propagation.** Announces are rebroadcast with a hop limit
  `PATHFINDER_M = 128`, up to `LOCAL_REBROADCASTS_MAX = 2` local rebroadcasts,
  after a grace `PATHFINDER_G = 5 s` plus random jitter `PATHFINDER_RW = 0.5 s`
  (`Transport.py:63-77`).
- **Path TTLs.** Default `PATHFINDER_E = 7 days`; access-point paths `AP_PATH_TIME
  = 1 day`; roaming paths `ROAMING_PATH_TIME = 6 hours` (`Transport.py:71-73`).
- **Deduplication.** A rolling table of recent packet hashes suppresses loops.
- **Path request pacing.** `PATH_REQUEST_TIMEOUT = 15 s`, `PATH_REQUEST_MI = 20 s`
  minimum interval, and per-interface announce caps (`Transport.py:79-83`).

These values and structures are documented for fidelity; only the path request
and path response packets above are normative.
