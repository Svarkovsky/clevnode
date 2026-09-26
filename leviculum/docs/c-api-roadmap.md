# Leviculum C API roadmap, towards feature-complete

Status: living roadmap. Goal: a C developer can write every kind of Reticulum
application, tool, or server in C, without touching Reticulum internals.
"Every kind" here means servers, client tools, file copy (lncp/rncp
compatible), reliable custom protocols, and LoRa/off-grid mesh apps. LXMF
(messaging) is out of scope until the Rust LXMF layer lands; it then gets its
own C API and roadmap.

This roadmap is the plan of record for closing the gap between the shipped v1
surface and that goal. The current gap analysis: v1 covers IP-based
point-to-point (links, request/response, datagram, resource transfer) well,
so an lncp/rncp-compatible file tool over TCP is already buildable, but several
whole capabilities are missing. The Reticulum engine (`leviculum-std`,
`leviculum-core`) already implements almost all of it; the work is projecting
it through the additive facade (`leviculum_std::api`) and the thin C FFI
(`leviculum-ffi`), never refactoring signatures `lnsd`/`lnstest` depend on.

## Guiding principles

- Additive only. New facade methods plus FFI wrappers and re-exports. Do not
  change the driver or core signatures the daemons use.
- Thin projection. The C FFI stays a near-mechanical projection of the curated
  Rust facade. KISS.
- Every phase ships together: the FFI surface, a worked C example, the full
  test set for that surface (see Testing strategy), a `mdbook` doc update
  (overview/how-to/reference), and a clean `just sanitize-ffi` pass.
- Wire and semantic compatibility with Python Reticulum is non-negotiable;
  every phase adds a Python-interop test.
- No phase is "done" until its tests are green and non-flaky.

Effort sizes are rough: S = days, M = one to two weeks, L = more.

## Phase 1: run as or with a daemon (config + shared instance). Size S. DONE

Shipped: `lev_builder_config_file`, `lev_builder_share_instance`,
`lev_builder_connect_shared_instance` on the facade and the C FFI; unit and
in-process integration tests (config-file node bring-up, shared-instance
announce forwarding); the `daemon.c` acceptance program; the `lnsd.c` C daemon
with a spawn-and-signal lifecycle test; a `c-lnsd` adapter in periculum
(static `c-lnsd` mounted like `lnsd`, driven by the existing tools); ASan/LSan/TSan clean (tokio-reactor false positives scoped out);
reference and how-to docs.

The highest-leverage, lowest-effort phase. The engine already supports all of
it (`ReticulumNodeBuilder::config_file`, `share_instance`, `instance_name`,
`connect_to_shared_instance`), and enabling the shared instance also starts the
RPC server (`spawn_rpc_server`) that makes `rnstatus`/`rnpath`/`rnprobe`
compatible. Exposing this unlocks two things at once:

- Drop-in client tools: a C program attaches to a running `rnsd`/`lnsd` via
  `connect_to_shared_instance`, instead of bringing up its own stack, which is
  how `rncp`/`rnx`/`rnstatus` normally work.
- A minimal C `lnsd`: load a config, offer a shared instance plus RPC, run.
  Because it speaks the same control surface as `lnsd`, it plugs into the
  periculum docker harness as a node whose `adapter = "c-lnsd"`, driven
  by the existing `rnprobe`/`rnstatus`/`rnpath`/`lnstest`/`lncp` tools, with no new
  harness steps. This is the on-ramp to the deep in-mesh network tests.
- Side effect: loading a config file also brings every interface type
  (including RNode and Serial) into a C node, so LoRa is reachable via config
  even before Phase 2.

FFI surface: `lev_builder_config_file`, `lev_builder_share_instance` (name +
enable), `lev_builder_connect_shared_instance`.

Deliverable: the C `lnsd` example program plus its integration into
periculum as a `c-lnsd`-adapter node, validated in a TCP scenario.

## Phase 2: programmatic radio interfaces (RNode + Serial). Size M. DONE

Shipped: `lev_builder_add_rnode` and `lev_builder_add_serial` on the facade and
the C FFI, projecting two additive driver builder methods; NULL-guard unit
tests; an in-process test bringing a node up over a serial interface backed by
a pty; the `radio.c` acceptance program; reference and how-to docs. The over-
the-lora-proxy and real-RNode validation remains for periculum's `hardware/`
corpus (the Phase 1 `c-lnsd`-adapter node with an RNode/serial config over the
proxy device).

LoRa and off-grid mesh is the signature Reticulum use. Phase 1 already reaches
it via a config file; this phase adds the ergonomic, programmatic path so a C
app needs no config file. The driver already handles `RNodeInterface` and
`SerialInterface` from interface config; only builder methods are missing
(today only TCP/UDP/auto exist).

FFI surface: `lev_builder_add_rnode(port, frequency, bandwidth, sf, cr,
tx_power, ...)`, `lev_builder_add_serial(port, speed, ...)`.

Deliverable: a C node running over real RNodes, and over the mock-LoRa
`lora-proxy` (serial over a pty), in periculum's `hardware/` corpus. This is
the in-mesh hardware test with a C program in the loop.

## Phase 3: reliable streams (Channel/Buffer). Size M to L. DONE

Finding on entry: reliable channel send was already exposed. `lev_link_send`
sends over the link's reliable channel (the `RawBytesMessage`, msgtype 0, both
Rust and Python use), so no new send method was needed. The real gap was on
receive: incoming channel messages and raw link packets were both projected as
`LEV_EVENT_LINK_DATA`, indistinguishable and dropping the message type and
sequence.

Shipped: a distinct `LEV_EVENT_LINK_MESSAGE` for channel messages with
`lev_event_msgtype` and `lev_event_sequence` accessors, keeping
`LEV_EVENT_LINK_DATA` for raw unsequenced link packets; in-process tests
asserting the sequence advances; a Python interop test against `RawBytesMessage`
over the channel (daemon `--echo-channel`); the `phase_c.c` metadata reads;
reference and how-to docs; ASan/LSan/TSan clean.

Many protocols need reliable, sequenced messages over a link, beyond raw
packets and file-sized resources.

FFI surface: `lev_link_send_reliable` (sequenced, retransmitted) plus a
distinct `LEV_EVENT_LINK_MESSAGE` so channel and raw link data are
distinguishable.

Deliverable: interop with Python's `RawBytesMessage` channel (the message type
the test daemon already uses).

## Phase 4: crypto and destination semantics. Mixed size

Several small, mostly independent items.

- 4a, identity sign/verify/encrypt/decrypt. Size S. DONE. Re-exposed as
  `lev_identity_sign/_verify/_encrypt/_decrypt`; unit round-trip, Python
  Ed25519 verify interop, `crypto.c`, docs, Miri clean.
- 4b, ratchets (forward secrecy on destinations). Size M. DONE.
  `lev_destination_enable_ratchets` and `lev_destination_ratchet_public`; unit,
  in-process link-over-ratchet, Python ratcheted-announce interop, `ratchet.c`,
  docs, ASan/TSan/Miri clean.
- 4c, proof strategies and proof-request events. Size M. DONE.
  `lev_destination_set_proof_strategy` (None/App/All) and `lev_send_proof`, plus
  the `PACKET_PROOF_REQUESTED`/`LINK_PROOF_REQUESTED`/`LINK_DELIVERY_CONFIRMED`
  events; additive facade and driver `send_proof`; unit, in-process App/All
  tests, `proof.c`, docs, ASan/TSan/Miri clean.
- 4d, group destination keys. Size M, lowest priority. BLOCKED on the engine,
  tracked in Codeberg #75. Verified: the core has the Group destination type
  but no shared-key crypto (no group-key generation/load, no group AES field,
  no group encrypt/decrypt branch), a wire-compatibility gap with Python-RNS
  GROUP destinations. Wiring it is core work, not an additive FFI projection, so
  it is out of scope here until the engine implements group keys; then the FFI
  is a thin add (set/load group key, mark the destination Group).

FFI surface: `lev_identity_sign/verify/encrypt/decrypt`,
`lev_destination_enable_ratchets`, `lev_destination_set_proof_strategy` plus
proof events, group-key functions.

## Phase 5: diagnostics. Size M, nice to have. DONE

A C `rnstatus`/`rnpath` is buildable. The data existed in the engine but was
excluded from the curated facade as internal; re-exposed deliberately and
read-only.

Shipped:
- `lev_transport_stats` reads the transport counters and the path-table size
  into out-parameters (struct-free).
- `lev_path_table_snapshot`/`_count`/`_entry`/`_free`: an owned, frozen
  snapshot of the path table read by index (hash, hops, next hop, interface,
  expiry), for an `rnpath` view.
- `lev_interface_stats_snapshot`/`_count`/`_name`/`_entry`/`_free`: the same
  snapshot pattern over interfaces (name, online, is-local-client, rx/tx
  bytes), for the `rnstatus` interface section. An additive driver method joins
  the core's names and online status with the I/O byte counters.

Design decision taken: the arrays of structured records use the existing
opaque-handle plus per-index-accessor pattern (as `lev_event_t` does), so the
boundary stays struct-free and the snapshots are frozen copies, immune to
mid-read changes in the live tables.

All with additive facade accessors, unit and in-process tests, the `stats.c`
acceptance program, docs, and ASan/LSan/TSan clean (the three-lock interface
join has no lock-order issues).

## Postponed: LXMF C API (messaging)

The `leviculum-lxmf` Rust layer is implemented as a separate `no_std + alloc`
crate. Exposing it through `leviculum-ffi` remains out of scope for this
roadmap; that work needs its own C ownership and event-surface design.
Messenger, mail, and NomadNet-style apps depend on that future FFI layer.

## Ordering rationale

Phase 1 first: it is cheap (the engine is ready) and immediately unlocks the C
`lnsd` in the test infrastructure, so every later phase can be validated in a
real mesh. Phase 2 next, because LoRa is the core use and the C node can then
be tested over (mock) LoRa. Phases 3 and 4 for completeness and parity. Phase 5
for convenience. After 1 to 4 (without LXMF), a C developer can write servers,
client tools, lncp-compatible file copy, reliable protocols, and LoRa mesh
apps, that is every kind of tool and server except messaging.

## Testing strategy: broad coverage with every kind of test

The test system is layered. Each layer catches a different class of bug, and
every phase extends the layers that touch its surface. The bug classes a thin
C-over-Rust FFI is prone to are memory and ownership, the event bridge and
threading, buffer marshalling, the panic boundary, and wire and semantic
compatibility with the reference, so the layers are chosen to attack exactly
those.

1. Function unit tests, Rust-driven (`leviculum-ffi/tests/ffi_unit.rs`). Call
   the `lev_*` functions directly, no network. Cover every function's NULL
   guards, the read(2) size-query and buffer-too-small protocol, invalid enums
   and lengths, value round-trips, and error reporting. Add cases for each new
   function per phase.
2. In-process integration (`tests/ffi_integration.rs`). Two or more real nodes
   over TCP loopback, driven through the C API, asserting via the event fd.
   Cover every flow and its unhappy paths. Extended per phase with the new
   capability and its failure modes.
3. In-process multi-hop and relay. Three to five nodes in one process with
   transport enabled, forming chains and small topologies, exercising
   multi-hop announce, connect, and data, and path healing after a node
   restart, all through the C API. Covers realistic topologies without Docker.
4. Python interop (`tests/ffi_interop.rs`). A C node against a real Python RNS
   daemon (`scripts/test_daemon.py` over JSON-RPC). Proves the FFI keeps wire
   and semantic compatibility. Every phase adds the matching interop direction
   (for example reliable channel against `RawBytesMessage`, RNode framing,
   ratchet negotiation). Skips cleanly without Python RNS.
5. C acceptance programs (`examples/c/*.c`, run by `tests/ffi_c_tests.rs`).
   Real C compiled against the real `.so`, proving it works from a C compiler
   and linker, not only from Rust. Each phase adds a focused C example.
6. Sanitizers and Miri (`just sanitize-ffi`). AddressSanitizer and
   LeakSanitizer and ThreadSanitizer over the integration suite (memory, leaks,
   data races in the bridge and the two-runtime threading); Miri over the pure
   unsafe marshalling paths (undefined behaviour). Run each phase.
7. Property and fuzz tests. `proptest`-driven random sequences of `lev_*` calls
   (build, start, connect, send, close, free in random orders, random buffer
   sizes including zero and boundaries, concurrent threads) to surface lifecycle
   and marshalling bugs that fixed scenarios miss. Grows with the surface.
8. Soak and stress. Long-running runs with many links, high concurrent send
   volume, churn (open, close, free repeatedly), and event floods over minutes,
   to surface leaks, fd exhaustion, drift, and bridge robustness under load.
9. Docker integration with a C `lnsd` (periculum, from Phase 1). The C
   daemon as a `c-api` node in the multi-container mesh alongside Rust `lnsd`
   and Python `rnsd`, driven by the existing `rnprobe`/`rnstatus`/`rnpath`
   tools. Multi-node, multi-hop, relay, and mixed-implementation scenarios with
   a C program in the loop.
10. LoRa, mock and hardware (from Phase 2). The C node over the `lora-proxy`
    mock-LoRa serial bridge for CI, and over real RNodes in the hardware
    nightly tier.
11. Cross-cutting guards. The panic-guard coverage test
    (`tests/guard_coverage.rs`) enforcing that every exported function is
    catch_unwind-wrapped; the generated header staying in sync with the symbols;
    `cargo fmt` and `cargo clippy -- -D warnings`; and an Android per-ABI build
    gate once the cdylib targets Android.

How the layers run: the fast and cheap layers (1, 2, 3, 4, 5, 7, 11) run in the
Just `standard` tier on every commit, via the `test-ffi` recipe and
`cargo test -p leviculum-ffi`. The heavy layers run on demand or nightly:
sanitizers and Miri (6) via `just sanitize-ffi`, soak and stress (8) on a timer,
Docker integration (9) in the extensive tier, and LoRa hardware (10) in the
nightly tier, matching the existing test tiers.

## Definition of done, per phase

- The FFI surface compiles, with `cargo fmt` and `cargo clippy --workspace --
  -D warnings` clean and no `unwrap`/`expect` in non-test FFI code.
- Unit, in-process integration, Python interop, and a C acceptance example for
  the new surface, all green and non-flaky.
- `just sanitize-ffi` clean for the new code paths.
- mdbook docs updated (overview, how-to, reference) and the book builds.
- For Phases 1 and 2, the C `lnsd` runs in a periculum mesh for at
  least one scenario.
