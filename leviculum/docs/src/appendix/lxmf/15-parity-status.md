# Python LXMF 1.1.0 and RNS 1.4.0 parity status

This page records implementation parity separately from the wire-format
coverage ledger. The active Python LXMF reference is release 1.1.0 at commit
`795fdaa2b0777c13033787d933d1afc94a2377cb`. Its package metadata requires
RNS 1.4.0 or newer. Leviculum's pinned Reticulum fixture is still the
RNS 1.3.5-based commit `d5e62d4e15c5fe2e170f7bd9e120551671f21a27`,
so passing the LXMF vectors does not by itself constitute an RNS 1.4.0 parity
claim.

The 1.1.0 vector regeneration changed only the recorded LXMF version and
commit. The existing message, stamp, ticket, delivery-announce, propagation
announce, upload, and mailbox wire bytes remained identical.

## LXMF 1.1.0 client parity

| Python 1.1.0 behaviour | Leviculum status | Notes |
|---|---|---|
| Propagation announces accept numeric transfer and sync limits | Implemented | `leviculum-lxmf` accepts unsigned integers and finite, non-negative, integral MessagePack floats, then normalises both to `u64`. This matches Python's `int(...)` conversion, including the 1.1.0 announce observed with `1024.0`. Fractional, negative, non-finite, and out-of-range values remain rejected. |
| `propagation_transfer_size` records a `/get` response Resource size | Implemented | Incoming request progress carries `transfer_size` through `PropagationTransportEvent`; the mailbox runtime retains it, and WASM returns it as `transferSize` from `lxmfPropagationStatus()`. It resets with a new or cancelled sync and remains available with the completed result. |
| `inbound_count()` and `inbound_resources()` expose active incoming delivery Resources | Implemented, read-only | `LxmfNode` tracks accepted/started Resources by Resource hash and removes them on completion, failure, or Link close. Rust exposes count and iterator APIs. WASM exposes `lxmfInboundResources()` snapshots containing `linkId`, `resourceHash`, `transferSize`, `dataSize`, and `progress`. |
| `cancel_inbound(resource_hash)` and `cancel_all_inbound()` | Blocked on core | Tracking is deliberately read-only. Clean cancellation needs a core API that locates an already accepted incoming Resource, transitions it exactly once, emits/sends the Reticulum Resource cancellation (`RESOURCE_RCL`), and produces the normal terminal event. Merely dropping the LXMF tracking entry would leave the Reticulum transfer active. |
| Thread locks and atomic Python file replacement | Equivalent by architecture | The Rust router is single-owner mutable state. Its bounded checkpoint is handed to the embedding storage transactionally, so Python's thread/file implementation details are not copied literally. |
| Configurable `lxmd` inbound delivery stamp cost, with daemon default 12 | Policy-supported | `leviculum-lxmf` accepts a configured inbound cost and advertises/enforces it. It does not implement the `lxmd` daemon or inherit its CLI/config default. An embedding application may deliberately choose a different default. |

## Python propagation-node and peer functionality

`leviculum-lxmf` remains a propagation **client**, not a propagation-node
server. The following Python 1.1.0 changes therefore remain intentionally
unimplemented:

- corrected minimum accepted `/offer` stamp cost
  (`max(0, cost - flexibility)` instead of `min(...)`);
- completing peer synchronisation without sending an empty offer;
- accepted-offer Link accounting and the new offer state values;
- sequential propagation-stamp validation, optional treatment of static peers,
  and the maximum concurrent inbound-sync limit;
- propagation-node hosting, peer selection, transit storage, `/offer`, stats,
  rotation, and synchronisation generally.

These are not client interoperability blockers. They become requirements if
Leviculum adds propagation-node hosting. At that point they belong in a
separate server/peer state machine with bounded queues and tests, not in
`leviculum-core`.

The existing client also deliberately limits a semantic Link request or
response Resource to one efficient Resource segment. Python-compatible
reassembly of split request/response Resources above 1,048,575 bytes remains a
layer-level gap. It is independent of the LXMF 1.1.0 wire changes above.

## Required `leviculum-core` work for the RNS 1.4.0 baseline

The items below are the core changes identified by the RNS 1.4.0 compatibility
audit that affect LXMF or the security and lifecycle of primitives it uses.
They must be completed before claiming the LXMF 1.1.0-required RNS baseline.
This list is scoped to Leviculum's implemented packet, announce, Link, request,
and Resource surfaces; a release claim still requires advancing the vendored
RNS reference and running a complete differential audit.

### 1. Raise the discovery stamp default to 16

`leviculum-core/src/discovery/stamp.rs` still defines
`DEFAULT_STAMP_VALUE = 14`; the RNS 1.4.0 baseline uses 16.

Required work:

- change the default and its documentation;
- regenerate discovery announce vectors and update tests that assume cost 14;
- verify acceptance at the threshold and rejection below it;
- verify encrypted and plaintext discovery announces;
- confirm any application-configured overrides remain explicit.

Receiving a higher-cost RNS 1.4.0 discovery announce already works. The
interoperability problem is generation: a local default-cost-14 discovery
announce may be rejected by a peer enforcing the new default.

### 2. Bound and serialise discovery announce validation

RNS 1.4.0 adds bounded valid/invalid validation caches and serialises expensive
discovery validation. Leviculum validates correctly but does not yet reproduce
those resource controls.

Required work:

- add bounded caches keyed by the validation input/hash, with explicit
  capacities and deterministic eviction;
- cache both successful and failed validation without caching partially parsed
  or unauthenticated state;
- ensure only one expensive validation for the same input can be in flight;
- keep the `no_std` core single-threaded and expose cooperative work if the
  validation cannot be completed within the caller's budget;
- add flood, duplicate, eviction, malformed-input, and persistence-boundary
  tests.

This is primarily CPU and memory denial-of-service hardening, not a wire-format
change.

### 3. Make Link identification one-time

The current LINKIDENTIFY handler replaces the Link's remote identity and emits
`LinkIdentified` every time a valid identify packet is received. RNS 1.4.0 only
sets the remote identity while it is unknown.

Required work:

- ignore or explicitly reject subsequent LINKIDENTIFY attempts after the first
  accepted identity;
- never replace an established remote identity;
- emit the identification event exactly once;
- preserve the existing blackhole check before accepting the first identity;
- add tests for duplicate-same-identity and conflicting-identity attempts.

This is a state-integrity and application-trust boundary, so it should be fixed
before exposing identified-Link metadata as durable identity evidence.

### 4. Add active incoming Resource cancellation

The incoming Resource object has a private `cancel()` transition and core
already understands the `RESOURCE_RCL` context, but `NodeCore` cannot cancel an
already accepted inbound Resource by hash.

Required work:

- add a public `NodeCore` operation scoped by Link ID and Resource hash;
- locate only an active receiver-side Resource and make cancellation
  idempotent;
- send the correct Resource cancellation packet to the peer;
- remove the active receiver state and emit one
  `ResourceFailed { error: Cancelled, is_sender: false }`;
- define behaviour for unknown, completed, sender-side, and wrong-Link hashes;
- add loss, duplicate-cancel, Link-close, and simultaneous full-duplex Resource
  tests.

After this exists, `leviculum-lxmf` can implement Python-compatible
`cancel_inbound()` and `cancel_all_inbound()` without leaking transport state.

### 5. Align Link keepalive activity timing

Leviculum currently schedules proactive initiator keepalives primarily from its
last keepalive timestamp. RNS 1.4.0 bases the idle interval on general outbound
Link activity and keepalive echo activity. The current behaviour is
interoperable but can transmit unnecessary keepalives.

Required work:

- track last outbound Link activity independently of the keepalive counter;
- reset the idle deadline for ordinary outbound Link traffic and appropriate
  keepalive echoes;
- retain the RTT-derived interval, stale detection, and configured override;
- test sustained one-way traffic, idle initiator/responder pairs, delayed
  echoes, and stale recovery.

This is lower priority than the stamp, identification, validation-cache, and
Resource-cancellation items because it does not change message correctness.

## Completion criteria

RNS 1.4.0 parity should only be marked complete after all required core items
are implemented, the Reticulum submodule is advanced to an immutable 1.4.0
reference, affected golden vectors are regenerated, and differential tests
cover malformed, duplicated, delayed, and concurrent inputs. Until then,
Leviculum should describe itself as LXMF 1.1.0 wire-compatible on its
implemented client surfaces, with the RNS core baseline still in progress.
