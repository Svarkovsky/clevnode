# Storage Trait Split Analysis

Deep analysis of every Storage trait method: callers, frequency,
embedded relevance, and proposed sub-trait groupings.

## Method Inventory: 71 methods across 15 data categories

### Group 1: Packet Dedup (3 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 1 | `has_packet_hash` | Transport (`process_incoming`) | **hot** -- every inbound packet | ESSENTIAL |
| 2 | `add_packet_hash` | Transport (9 sites: send/receive/proof/data) | **hot** -- every packet | ESSENTIAL |
| 3 | `remove_packet_hash` | **DEAD CODE** -- 0 production calls | never | dead |

**Embedded impl**: Fixed-size ring buffer, e.g. `[[u8; 32]; 512]` with
write cursor. Has-check is linear scan (512 x 32B = 16KB). Cannot be
no-op -- without dedup, packets loop forever.

---

### Group 2: Path Table (12 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 4 | `get_path` | Transport (19 sites) | **hot** | ESSENTIAL |
| 5 | `set_path` | Transport (4 sites) | frequent | ESSENTIAL |
| 6 | `remove_path` | NodeCore (1), Transport (3), RPC (1) | sometimes | ESSENTIAL |
| 7 | `path_count` | NodeCore (1), Transport (1), Driver (1) | rarely | nice-to-have |
| 8 | `expire_paths` | Transport (`clean_path_states`) | periodic | ESSENTIAL |
| 9 | `earliest_path_expiry` | Transport (`next_deadline`) | periodic | ESSENTIAL |
| 10 | `has_path` | NodeCore (3), Transport (4), Driver (1) | frequent | ESSENTIAL |
| 11 | `path_entries` | Transport (2: `path_table_entries`, `drop_all_paths_via`) | rarely | nice-to-have |
| 12 | `get_path_state` | Transport (1: `path_is_unresponsive`) | sometimes | nice-to-have |
| 13 | `set_path_state` | Transport (3: `mark_path_unresponsive`/`responsive`) | sometimes | nice-to-have |
| 14 | `clean_stale_path_metadata` | Transport (`clean_path_states`) | periodic | nice-to-have |
| 15 | `remove_paths_for_interface` | NodeCore (1), Transport (1) | rarely | ESSENTIAL |

**Embedded impl**: Fixed-size array, e.g. `[Option<PathEntry>; 32]`
with LRU eviction on set. PathEntry is ~50 bytes, total ~1.6KB. Cannot
be no-op -- node can't route without paths.

Path state (methods 12-14) is separable -- unresponsive tracking is a
quality-of-life feature. An embedded node could skip it and just remove
stale paths via expiry.

---

### Group 3: Announce Processing (11 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 16 | `get_announce` | Transport (5 sites) | sometimes | ESSENTIAL |
| 17 | `get_announce_mut` | NodeCore (1), Transport (2) | sometimes | ESSENTIAL |
| 18 | `set_announce` | Transport (4 sites) | sometimes | ESSENTIAL |
| 19 | `remove_announce` | Transport (2: `check_announce_rebroadcasts`) | sometimes | ESSENTIAL |
| 20 | `announce_keys` | Transport (2: `next_deadline`, `check_announce_rebroadcasts`) | periodic | ESSENTIAL |
| 21 | `get_announce_cache` | NodeCore (1), Transport (3) | sometimes | ESSENTIAL |
| 22 | `set_announce_cache` | NodeCore (3), Transport (1) | sometimes | ESSENTIAL |
| 23 | `clean_announce_cache` | Transport (1: `clean_path_states`) | periodic | nice-to-have |
| 24 | `get_announce_rate` | Transport (1: `check_announce_rate`) | sometimes | OPTIONAL |
| 25 | `set_announce_rate` | Transport (3: `check_announce_rate`) | sometimes | OPTIONAL |
| 26 | `announce_rate_entries` | Transport (1: `rate_table_entries`) | rarely | OPTIONAL |

**Embedded impl**: AnnounceEntry array, e.g. `[Option<AnnounceEntry>;
16]` (~2KB). Announce cache stores raw bytes -- variable size, harder
for embedded (up to ~500 bytes each, so 16 x 500 = ~8KB). Cannot be
no-op for core announces (16-22). Rate limiting (24-26) CAN be no-op --
node just doesn't rate-limit.

---

### Group 4: Path Requests (3 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 27 | `get_path_request_time` | Transport (2: `send_path_request` rate limiting) | sometimes | ESSENTIAL |
| 28 | `set_path_request_time` | Transport (1) | sometimes | ESSENTIAL |
| 29 | `check_path_request_tag` | Transport (1: `handle_path_request` dedup) | sometimes | ESSENTIAL |

**Embedded impl**: Small fixed array, e.g. `[([u8; 16], u64); 16]` for
request times (~384B), ring buffer for tags. Cannot be no-op -- without
request dedup, path request storms occur.

---

### Group 5: Receipts (5 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 30 | `get_receipt` | Transport (4: `get_receipt`, `mark_delivered`, proof handling) | sometimes | OPTIONAL |
| 31 | `set_receipt` | Transport (3: `create_receipt`, `create_receipt_with_timeout`, `mark_delivered`) | sometimes | OPTIONAL |
| 32 | `remove_receipt` | **0 production calls** (only via `expire_receipts`) | never | dead (direct) |
| 33 | `expire_receipts` | Transport (1: `check_receipt_timeouts`) | periodic | OPTIONAL |
| 34 | `earliest_receipt_deadline` | Transport (1: `next_deadline`) | periodic | OPTIONAL |

**Embedded impl**: Fixed array, e.g. `[Option<PacketReceipt>; 8]`
(~1KB). CAN be no-op -- node works without delivery proofs. Links still
establish; resources still transfer. You just don't get explicit delivery
confirmation for single packets.

---

### Group 6: Known Identities (2 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 35 | `get_identity` | NodeCore (1: `send_single_packet`), Driver (1) | sometimes | ESSENTIAL |
| 36 | `set_identity` | NodeCore (1: `remember_identity`) | sometimes | ESSENTIAL |

**Embedded impl**: Fixed array, e.g. `[([u8; 16], Identity); 16]`.
Identity is ~128 bytes, total ~2.3KB. Borderline essential -- without it,
node can't encrypt to a destination whose announce it already saw but
isn't currently cached in the announce table. Could be no-op if the node
only talks to destinations it just heard announce.

---

### Group 7: Transport Relay (14 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 37 | `get_link_entry` | Transport (3: `forward_link_routed`, `process_data`, `is_for_local_client_link`) | sometimes | RELAY ONLY |
| 38 | `get_link_entry_mut` | Transport (1: mark link validated on proof) | rarely | RELAY ONLY |
| 39 | `set_link_entry` | Transport (1: `handle_link_request` -- insert bidirectional route) | rarely | RELAY ONLY |
| 40 | `remove_link_entry` | **0 production calls** (only via expire/cleanup) | never | dead (direct) |
| 41 | `has_link_entry` | Transport (3: dedup exemptions, `is_link_routed` check) | sometimes | RELAY ONLY |
| 42 | `expire_link_entries` | Transport (1: `clean_link_table`) | periodic | RELAY ONLY |
| 43 | `earliest_link_deadline` | Transport (1: `next_deadline`) | periodic | RELAY ONLY |
| 44 | `remove_link_entries_for_interface` | NodeCore (1), Transport (1) | rarely | RELAY ONLY |
| 45 | `get_reverse` | Transport (1: proof routing) | sometimes | RELAY ONLY |
| 46 | `set_reverse` | Transport (3: `forward_packet`, link-routed data, proof handling) | sometimes | RELAY ONLY |
| 47 | `remove_reverse` | Transport (1: proof routing) | sometimes | RELAY ONLY |
| 48 | `has_reverse` | **0 production calls** (default impl, test only) | never | dead |
| 49 | `expire_reverses` | Transport (1: `clean_reverse_table`) | periodic | RELAY ONLY |
| 50 | `remove_reverse_entries_for_interface` | NodeCore (1), Transport (1) | rarely | RELAY ONLY |

**Embedded impl**: CAN be full no-op for leaf nodes
(`enable_transport=false`). A leaf node never relays, never builds
link/reverse tables. If an embedded node IS a relay, needs fixed arrays:
`[Option<LinkEntry>; 16]` (~1KB), `[Option<ReverseEntry>; 32]` (~2KB).

---

### Group 8: Discovery Path Requests (5 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 51 | `set_discovery_path_request` | Transport (1: `handle_path_request`) | sometimes | RELAY ONLY |
| 52 | `get_discovery_path_request` | Transport (3: handle/retry/send_discovery) | sometimes | RELAY ONLY |
| 53 | `remove_discovery_path_request` | Transport (2: `send_discovery_path_response`) | sometimes | RELAY ONLY |
| 54 | `expire_discovery_path_requests` | Transport (1: `clean_path_states`) | periodic | RELAY ONLY |
| 55 | `discovery_path_request_dest_hashes` | Transport (2: `next_deadline`, retry) | periodic | RELAY ONLY |

**Embedded impl**: CAN be full no-op for leaf nodes. Only transport
nodes forward path requests on behalf of others. Leaf nodes send their
own path requests via `send_path_request` (Group 4), not this mechanism.

---

### Group 9: Ratchets (7 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 56 | `get_known_ratchet` | Transport (1: `set_local_client` ratchet replay) | rarely | OPTIONAL |
| 57 | `remember_known_ratchet` | Transport (1: `handle_announce`), NodeCore (2: `announce_destination`, `check_mgmt_announces`) | rarely | OPTIONAL |
| 58 | `has_known_ratchet` | **0 production calls** | never | dead |
| 59 | `known_ratchet_count` | **0 production calls** (test only) | never | dead |
| 60 | `expire_known_ratchets` | Transport (1: `clean_path_states`) | periodic | OPTIONAL |
| 61 | `store_dest_ratchet_keys` | NodeCore (2: `announce_destination`, `check_mgmt_announces`) | rarely | OPTIONAL |
| 62 | `load_dest_ratchet_keys` | NodeCore (1: `register_destination`) | rarely | OPTIONAL |

**Embedded impl**: CAN be full no-op. Node works without forward
secrecy -- announces are still validated, links still established, data
still encrypted. Ratchets add key rotation for post-compromise security.
On a RAM-constrained device, this is the first thing to skip.

---

### Group 10: Shared Instance (7 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 63 | `add_local_client_dest` | Transport (1: `handle_announce` for local client) | rarely | SHARED ONLY |
| 64 | `remove_local_client_dests` | Transport (1: `set_local_client` cleanup) | rarely | SHARED ONLY |
| 65 | `has_local_client_dest` | **0 production calls** (test only) | never | dead |
| 66 | `set_local_client_known_dest` | Transport (1: `handle_announce`) | rarely | SHARED ONLY |
| 67 | `has_local_client_known_dest` | **0 production calls** (test only) | never | dead |
| 68 | `local_client_known_dest_hashes` | Transport (2: `set_local_client`, `clean_path_states`) | rarely | SHARED ONLY |
| 69 | `expire_local_client_known_dests` | Transport (1: `clean_path_states`) | periodic | SHARED ONLY |

**Embedded impl**: Full no-op. Embedded nodes don't share instances.
Zero correctness impact. Shared instance is a desktop/server feature
(multiple programs sharing one daemon via Unix sockets). An embedded node
IS the daemon.

---

### Group 11: Persistence & Diagnostics (2 methods)

| # | Method | Caller | Frequency | Embedded? |
|---|--------|--------|-----------|-----------|
| 70 | `flush` | Driver (2: `save_persistent_state`, `auto_interface`) | rarely | OPTIONAL |
| 71 | `diagnostic_dump` | Driver (1) | rarely | OPTIONAL |

Already have empty default implementations. No action needed.

---

## Dead Code Summary

7 methods with zero production callers:

| Method | Notes |
|--------|-------|
| `remove_packet_hash` | Defined but never called anywhere |
| `has_reverse` | Only the default impl delegates to `get_reverse`; no external callers |
| `remove_link_entry` | Only called indirectly via `expire_link_entries` |
| `remove_receipt` | Only called indirectly via `expire_receipts` |
| `has_known_ratchet` | Test-only |
| `known_ratchet_count` | Test-only |
| `has_local_client_known_dest` | Test-only (one NodeCore test) |

`has_local_client_dest` is also test-only in Transport tests.

---

## Proposed Sub-Trait Split

### Tier 1: `CoreStorage` -- 28 methods

Every node needs these. Without them the protocol doesn't function.

```
Packet dedup:    has_packet_hash, add_packet_hash          (2)
Path table:      get_path, set_path, remove_path,
                 path_count, expire_paths,
                 earliest_path_expiry, has_path,
                 path_entries, remove_paths_for_interface   (9)
Path state:      get_path_state, set_path_state,
                 clean_stale_path_metadata                  (3)
Announces:       get_announce, get_announce_mut,
                 set_announce, remove_announce,
                 announce_keys, get_announce_cache,
                 set_announce_cache, clean_announce_cache    (8)
Path requests:   get_path_request_time,
                 set_path_request_time,
                 check_path_request_tag                      (3)
Identities:      get_identity, set_identity                  (2)
Persistence:     flush                                       (1)
```

**Embedded minimum: ~30KB RAM total**

| Collection | Layout | Size |
|------------|--------|------|
| Packet ring | `[[u8; 32]; 512]` | 16KB |
| Path table | `[Option<PathEntry>; 32]` | ~2KB |
| Announce table | `[Option<AnnounceEntry>; 16]` | ~1KB |
| Announce cache | `[Option<([u8;16], Vec<u8>)>; 16]` | ~8KB (variable, biggest concern) |
| Path requests | `[([u8;16], u64); 16]` | ~0.5KB |
| Path states | `[Option<([u8;16], PathState)>; 32]` | ~1KB |
| Identities | `[Option<([u8;16], Identity)>; 16]` | ~2KB |

**Cannot be no-op.**

### Tier 2: `ReceiptStorage` -- 5 methods

```
get_receipt, set_receipt, remove_receipt,
expire_receipts, earliest_receipt_deadline
```

**Embedded**: `[Option<([u8;16], PacketReceipt)>; 8]` ~1KB.
**Can be no-op.** Node works; single-packet delivery proofs are lost.
Links, resources, and channels all function -- they have their own proof
mechanisms.

**Why separate from CoreStorage**: An nRF52840 sensor that only sends
data and doesn't care about delivery confirmation saves 1KB RAM and 5
method implementations.

### Tier 3: `TransportRelayStorage` -- 19 methods

```
Link table:      get_link_entry, get_link_entry_mut,
                 set_link_entry, remove_link_entry,
                 has_link_entry, expire_link_entries,
                 earliest_link_deadline,
                 remove_link_entries_for_interface           (8)
Reverse table:   get_reverse, set_reverse, remove_reverse,
                 has_reverse, expire_reverses,
                 remove_reverse_entries_for_interface         (6)
Discovery:       set_discovery_path_request,
                 get_discovery_path_request,
                 remove_discovery_path_request,
                 expire_discovery_path_requests,
                 discovery_path_request_dest_hashes           (5)
```

**Embedded**: Full no-op for leaf nodes. Only matters if
`enable_transport=true`.
**Can be no-op.** Leaf node can't relay, but communicates fine as an
endpoint.

**Why one trait instead of three**: Link table, reverse table, and
discovery requests are always used together -- they're all
transport-mode infrastructure. A node is either a relay or it isn't.
There's no use case for "relay with link table but no reverse table."

### Tier 4: `RatchetStorage` -- 7 methods

```
get_known_ratchet, remember_known_ratchet,
has_known_ratchet, known_ratchet_count,
expire_known_ratchets,
store_dest_ratchet_keys, load_dest_ratchet_keys
```

**Embedded**: `[Option<([u8;16], [u8;32], u64)>; 8]` ~0.5KB if
implemented.
**Can be full no-op.** Forward secrecy is a security enhancement.
Without ratchets, announce encryption still works via the destination's
static keys. An embedded sensor node may not need post-compromise key
rotation.

**Why separate**: Security feature with storage cost. Embedded devices
with extreme RAM constraints can skip it. Also the only group that spans
both Transport and NodeCore callers in a way that's cleanly separable.

### Tier 5: `SharedInstanceStorage` -- 7 methods

```
add_local_client_dest, remove_local_client_dests,
has_local_client_dest,
set_local_client_known_dest, has_local_client_known_dest,
local_client_known_dest_hashes,
expire_local_client_known_dests
```

**Embedded**: Full no-op. Zero correctness impact. Shared instance is a
desktop/server feature (multiple programs sharing one daemon via Unix
sockets). An embedded node IS the daemon.

**Why separate**: Entirely irrelevant to embedded. Also the most likely
candidate for removal from the trait hierarchy entirely -- it could be a
compile-time feature flag instead.

### Tier 6: `AnnounceRateStorage` -- 3 methods

```
get_announce_rate, set_announce_rate, announce_rate_entries
```

**Embedded**: `[Option<([u8;16], AnnounceRateEntry)>; 16]` ~0.5KB if
implemented.
**Can be no-op.** Without rate limiting, node processes all announces.
On a small network (typical for embedded LoRa), announce volume is low
enough that rate limiting is unnecessary.

**Why separate from CoreStorage**: Rate limiting is operator policy, not
protocol correctness. A network of 5 LoRa nodes doesn't need it.

---

## Composition Design

```rust
// Tier 1 -- every node
trait CoreStorage { /* 28 methods */ }

// Tier 2-6 -- optional capabilities
trait ReceiptStorage { /* 5 methods */ }
trait TransportRelayStorage { /* 19 methods */ }
trait RatchetStorage { /* 7 methods */ }
trait SharedInstanceStorage { /* 7 methods */ }
trait AnnounceRateStorage { /* 3 methods */ }

// Backward-compatible supertrait -- existing code unchanged
trait Storage: CoreStorage + ReceiptStorage
    + TransportRelayStorage + RatchetStorage
    + SharedInstanceStorage + AnnounceRateStorage {}

// Blanket impl
impl<T> Storage for T where T: CoreStorage + ReceiptStorage
    + TransportRelayStorage + RatchetStorage
    + SharedInstanceStorage + AnnounceRateStorage {}
```

`Transport<C, S>` and `NodeCore<R, C, S>` keep `S: Storage` -- **zero
changes to existing code.** MemoryStorage and FileStorage implement all
sub-traits and get `Storage` for free.

For embedded:

```rust
struct EmbeddedStorage {
    // Only CoreStorage collections
    // ~30KB RAM
}
impl CoreStorage for EmbeddedStorage { /* real impls */ }
impl ReceiptStorage for EmbeddedStorage { /* no-ops */ }
impl TransportRelayStorage for EmbeddedStorage { /* no-ops */ }
impl RatchetStorage for EmbeddedStorage { /* no-ops */ }
impl SharedInstanceStorage for EmbeddedStorage { /* no-ops */ }
impl AnnounceRateStorage for EmbeddedStorage { /* no-ops */ }
// Gets Storage automatically via blanket impl
```

---

## Trade-offs & Uncertainties

### Confident assessments

- Groups 5 (SharedInstance) and 3 (TransportRelay) are cleanly
  separable -- no leaf-node code path touches them in production.
- Group 4 (Ratchets) is cleanly separable -- all call sites have
  graceful None/no-op fallback.
- The 7 dead-code methods should be removed regardless of whether the
  trait is split.

### Uncertainties

**1. CoreStorage is still 28 methods.** That's a lot for a "minimal"
trait. I considered splitting Path and Announce into separate sub-traits,
but they're called from the same Transport methods (`handle_announce`
touches both path table and announce table in the same function).
Splitting would require `where S: PathStorage + AnnounceStorage` bounds
scattered across Transport methods -- high friction for zero embedded
benefit since both are essential.

**2. Announce cache is the RAM wildcard.** Each cached announce is up to
~500 bytes of raw wire data. 16 entries = 8KB. On an nRF52840 with 256KB
RAM this is manageable, but on a smaller MCU it could dominate. The
cache is needed for path responses and link requests from remote nodes.
An endpoint that only initiates (never responds to path requests) could
skip it -- but that's a very narrow use case.

**3. Whether the split is worth the complexity.** Right now, `NoStorage`
already serves as the "skip everything" option, and `MemoryStorage` with
capacity limits would cover the "real embedded" case. The sub-trait
split adds type-system guarantees but also adds 6 trait definitions, 6
impl blocks per storage type, and ongoing maintenance burden. If there's
only one embedded target (nRF52840), a capacity-limited `MemoryStorage`
might be strictly better.

**4. Conditional compilation is the pragmatic alternative.** Instead of
sub-traits, use `#[cfg(feature = "transport")]` to gate
TransportRelay collections in MemoryStorage. Simpler, less
generic-parameter noise, but loses per-instance flexibility (can't mix
endpoint and transport nodes in the same binary).

---

## Recommendation

Remove the 7 dead methods now. Defer the sub-trait split until the first
embedded target actually needs it. The current MemoryStorage with
configurable capacity limits (already has `packet_hash_cap`,
`identity_cap`) extended to all collections covers the nRF52840 case
without any trait refactoring. The sub-trait design above is the right
split IF the refactor becomes necessary -- but it's a premature
abstraction today.
