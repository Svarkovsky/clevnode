# Storage and Embedding

`leviculum-core` is `#![no_std]` with only `alloc`
(`leviculum-core/src/lib.rs:59-70`). It contains no I/O, no clock, no
filesystem, and no async runtime. That is what lets the *exact same*
protocol code run on a Linux daemon, a future Android app, and a
bare-metal nRF52 firmware image. The bridge to the outside world is a
small set of traits the core depends on but does not implement.

## Three injected dependencies

The core declares its platform needs as traits in
`leviculum-core/src/traits.rs` and takes implementations from the
driver:

- **`Clock`** (`traits.rs:317`) — supplies the monotonic `now_ms()`
  and, only where the platform has a real wall clock,
  `wall_unix_secs()` (default `None`). The core never calls a system
  clock; time is handed in. `now_ms()` is a timer, not a calendar —
  on the host it counts milliseconds since process start
  (`leviculum-std/src/clock.rs:41`), on the nRF52 it is the Embassy
  timer (`leviculum-nrf/src/clock.rs:8`). Which wire fields need
  calendar time instead, and where a clockless node gets it, is the
  subject of [Time and Clocks](time-and-clocks.md).
- **`Storage`** (`traits.rs:372`) — supplies persistence and lookup
  for every collection the protocol maintains. `flush()` defaults to a
  no-op (`traits.rs:671`) so a RAM-only backend needs to implement
  nothing extra.
- **`Interface`** (`traits.rs:239`) — supplies framing and the wire (see
  [Interface Isolation](interface-isolation.md) and the
  [Interface trait](../architecture.md#interface-trait)).

Randomness is injected the same way, as an explicit
`rng: &mut impl CryptoRngCore` parameter rather than a global
(`leviculum-core/src/lib.rs`, "Platform Dependencies").

## The Storage trait

Rather than a generic key/value blob store, `Storage` exposes
**type-safe methods grouped by collection** — packet-dedup hashes, the
path table, the reverse table, link/announce tables, receipts, and
ratchets — with typed entries from `storage_types.rs`. The full method
inventory is tabulated in
[Architecture](../architecture.md#storage-trait).

This shape was a deliberate decision. The deep analysis of every
method — who calls it, how often, and whether it matters on an
embedded target — is in
[Storage Trait Split Analysis](../storage-trait-analysis.md). Read that
page before changing the trait surface.

## Three backends, one core

The same `NodeCore` is parameterised over its `Storage`
implementation, so embedding is a matter of choosing a backend
(`leviculum-core/src/node/mod.rs:242`,
`NodeCore<R: CryptoRngCore, C: Clock, S: Storage>`):

| Backend | Where | Behaviour |
|---------|-------|-----------|
| `NoStorage` | tiny / stateless | no-op |
| `MemoryStorage` | host / tests | `BTreeMap`, RAM only (inner store of `FileStorage`) |
| `EmbeddedStorage` | embedded (nRF52) | `heapless::FnvIndexMap`, fixed capacity, no allocator for maps |
| `FileStorage` | host (`leviculum-std`) | wraps `MemoryStorage` + disk |

`FileStorage` persists only what must survive a restart — known
destinations, the packet dedup hashlist, and ratchet keys — and keeps
the rest (paths, reverses, links, announces, receipts) in RAM,
rebuilt from the network on restart. The file formats and flush
strategy are in
[Architecture](../architecture.md#filestorage-persistence).

## What the split buys you

- **Host vs. embedded from one source tree.** `leviculum-std` builds a
  tokio driver around the core; `leviculum-nrf` builds an Embassy
  driver around the *same* core
  (`leviculum-nrf/src/bin/t114.rs`,
  `leviculum-nrf/src/bin/rak4631.rs`, both `#![no_std]` and both
  constructing the core via `NodeCoreBuilder`).
- **Testability.** Because time and storage are injected, the core is
  driven deterministically in tests — feed bytes and a fixed clock,
  drain the `TickOutput`, assert. This is the basis of the
  minimal-reproducer tests under `leviculum-std/tests/mvr/`.
- **No host concerns in the core.** Backpressure, airtime budgeting,
  and serial queueing live host-side in `leviculum-std` and never leak
  into the `no_std` core (`leviculum-std/src/interfaces/airtime.rs:1`).

See [Architecture](../architecture.md) for the sans-IO core diagram and
the driver event loop that pumps these traits.
