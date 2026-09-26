# Leviculum public API design review (consolidated)

Input to the ffi-Agent. Two independent reviews of `docs/leviculum-api-design.md`
were run: a primary review and a separate review with its own context that
verified claims against the actual `leviculum-std` / `leviculum-core` source.

Both reached the same verdict: the architecture is sound and the thin layer
claim mostly holds, but several correctness gaps must be settled before Phase a.
Both independently named the same top blockers (panic and poison, event fd
discipline, the connect key path, unbounded blocking sends), which raises
confidence that these are real. Resolve the blockers, then revise the design
doc, then implement. Source line numbers are current as of master 50df39a.

## Confirmed strengths (keep)

- Pollable event fd plus drain, callbacks rejected. Correct for Linux now and
  for an Android `ALooper_addFd` future. No foreign thread upcalls, no
  re-entrancy hazard.
- read(2) style buffer ownership: caller owns the buffer, `buf == NULL` queries
  the size, fixed size constants for stack allocation, freeing always through a
  typed `lev_*_free`. Kills the C free vs Rust dealloc class.
- cbindgen generated header, never committed. Matches the repo (`.gitignore`
  has `leviculum-ffi/*.h`). No committed copy can go stale.
- Opaque handles plus SONAME 0 plus the 0.x unstable note.
- `LEV_ERR_AGAIN` mapped to `try_send` Busy and PacingDelay, verified against
  `stream.rs:127-135`.

## Phase a blockers (resolve before any implementation)

### B1. Panic guard is unsound as written: poison and post-panic state (section 6)
`catch_unwind(AssertUnwindSafe(|| body))` wraps every function, but the
`AssertUnwindSafe` promise is false: the body touches the thread local last
error and the shared `Arc<Mutex<..>>` inside `leviculum_t`. A panic while the
core mutex is held poisons the mutex. `driver/mod.rs` uses `.lock().unwrap()`
everywhere (1126, 1297, 1352), so after one caught panic every later call
panics inside the guard and returns `LEV_ERR_PANIC` forever with no recovery.
Decide and document one of: a caught panic leaves `leviculum_t` usable only for
`lev_free`, or core access recovers poison with `lock().unwrap_or_else(|e|
e.into_inner())`. The `set_last_error` in the error arm must be panic free
(static string, no allocation that can itself panic). Document that output
parameters are indeterminate after a caught panic. Keep `panic = unwind` so the
guard catches at all.

### B2. Event fd and queue level-trigger contract is one sentence and is a busy-loop or lost-wakeup hazard (section 4)
This is the load-bearing mechanism of the whole library and it is unspecified.
"Bridge keeps the fd readable while the queue is non-empty" plus "drain until
NULL" does not self-balance between bridge writes and consumer reads. A second
event pushed between the consumer draining to empty and reading the fd leaves
the queue non-empty with a drained fd, so the consumer sleeps in `poll` forever
with a pending event. Pin it down: use eventfd in counter mode, the bridge
increments once per enqueue, `lev_next_event` reads and clears it only on the
transition to empty, with a defined re-arm if a push races in. State which
thread owns each side, state level vs edge semantics (the `poll` example
requires level), and state who closes the fd (the library, never the app).

### B3. Key layout constants are misleading and `connect` is unusable without the split (sections 3, 10)
`LEV_IDENTITY_PUB_LEN 64` is not a single 64 byte public key. Verified against
`constants.rs:88`: `IDENTITY_KEY_SIZE = X25519_KEY_SIZE + ED25519_KEY_SIZE = 64`,
so it is two 32 byte keys (X25519 encryption pub, then Ed25519 signing pub).
`lev_connect` needs only the Ed25519 half. `driver/mod.rs:1368-1359` documents
the signing key as bytes 32..64 of `public_key_bytes()`. A C author given a 64
byte blob and a `connect` taking a signing key has no way to know it must pass
the second half. Rename to `LEV_IDENTITY_KEY_LEN 64`, add
`LEV_SIGNING_KEY_LEN 32`, document the 0..32 X25519 and 32..64 Ed25519 split,
and add a helper or explicit note that `lev_connect` takes bytes 32..64. Also
close the flow gap: an `AnnounceReceived` event must expose the remote identity
(or the path to it) so the app can actually call connect. Today the event
accessors give link id, dest hash, and data, but not the identity connect needs.

### B4. Blocking sends can block the C thread without bound (sections 5, 4)
Section 5 says each wrapping call blocks the C thread via `block_on`. The
awaited methods can block indefinitely: `connect`, `announce_destination`,
`close_link`, `send_request`, `send_resource`, `send_single_packet` all do
`action_dispatch_tx.send(..).await` on a bounded channel
(`ACTION_DISPATCH_CAPACITY = 256`, e.g. `mod.rs:1154,1517,1535`) that blocks
until the loop drains, and `LinkHandle::send` (`stream.rs:108-141`) loops with
`tokio::time::sleep` retrying on Busy and PacingDelay with no timeout. On a
congested LoRa link this hangs with no way to interrupt. A single threaded app
parked in a blocking `lev_link_send` also cannot drain events, which can
deadlock if send progress needs a loop turn. Choose one: expose only the non
blocking `try_send` family at the boundary and let the app poll (consistent
with the compose-with-your-loop design), or give every blocking call a
`timeout_ms` and map expiry to `LEV_ERR_TIMEOUT`. State per function whether it
can block unboundedly.

## Settle before the header calcifies

### S1. Identity file IO has no backing and is net new surface (section 10)
`lev_identity_load_file` and `lev_identity_save_file` cannot be projected from
core `Identity`, which has no file IO. File persistence lives in
`leviculum-std`'s `FileIdentityStore`, not on `Identity`. These two functions
are new facade code, not pure re-typing, and they carry a Python file format
compatibility decision (the 64 byte transport identity format at
`builder.rs:924` is load-bearing for Python tool compat). Mark them as new
surface and settle the format question before shipping the header.

### S2. Returned identity ownership (section 10)
`lev_link_remote_identity` maps to `get_remote_identity(&LinkId) ->
Option<Identity>` (`mod.rs:1562`) which returns an owned `Identity`. The FFI
must mint a `lev_identity_t` the caller frees. The handle table in section 1
has no row for an identity created by an accessor. Add it and specify ownership.

### S3. Flat C enums for policy and strategy (sections 8, 10)
`lev_register_request_handler` policy and `lev_set_resource_strategy` strategy
are Rust enums (`RequestPolicy`, `ResourceStrategy`). They must become flat C
enums (`LEV_REQUEST_POLICY_*`, `LEV_RESOURCE_*`). Not mentioned in naming or the
surface. Enumerate them.

### S4. Complete the error code mapping (section 2)
Section 2 calls the codes a flat projection of `leviculum_std::error::Error`,
but the real enum (`error.rs`) has `Io, Config, Storage, Serialization,
NotRunning, Announce, Send, Link, Resource, Request`. The codes have no
`STORAGE` or `SERIALIZATION`, so those variants currently fall through to
nothing. Provide the exact 1:N table.

### S5. Facade type ownership undermines the stability goal (section The Rust facade)
The facade is meant to hide core internals, but it re-exports `Identity` and
`Error` directly from core. That makes the core type part of the stable public
contract. Decide whether the facade owns its types or accepts core as the
contract, and record the choice. The stability promise starts at 1.0, so this
can be deferred, but it should be a decision, not an accident.

### S6. Datagram delivery confirmation is overpromised (section 10)
`lev_send_datagram` returns the 16 byte packet hash (`TRUNCATED_HASHBYTES = 16`
verified). For an unreliable single packet, confirmation only arrives if a
proof comes back. The doc states confirmation as if guaranteed. Reword so C
users do not block waiting for a confirmation that may never come, and say
which sends actually get delivery events (links and resources do).

## Missing dimensions a clean C library must cover

- Logging. The stack uses `tracing` pervasively. A C app gets nothing unless a
  subscriber is installed, and a C caller cannot set a Rust subscriber. Add a
  `lev_log_set_callback` or `lev_log_set_level`, or document a default and a
  way to silence. Compare libcurl `CURLOPT_DEBUGFUNCTION`.
- Global once state. The old stub had `lns_init`. The new design has none. Note
  that `init_clock_anchor` in `build_sync` (`builder.rs:497`) is a process
  global side effect: building two nodes re-anchors the clock. Document whether
  multiple `leviculum_t` in one process is supported, and whether the library
  installs any process global hook (panic hook, tracing subscriber).
- fd lifetime and shutdown ordering. After `lev_free` the eventfd is closed; an
  app still polling it now references a recycled fd. Document the ordering: stop
  draining, remove the fd from your loop, then free. Define what happens if
  `lev_stop` or `lev_free` runs while a `lev_wait_event` is parked on another
  thread (wake it with `LEV_ERR_NOT_RUNNING`, or forbid it).
- String and bytes conventions. State once whether byte buffers are ever NUL
  terminated or always (ptr, len). `app_name`, `aspects`, and request `path`
  are strings. `Destination::new` (`destination.rs:285`) takes `&str` plus
  `&[&str]` aspects; specify how a C `char**` aspects array marshals.
- Event payload lifetime. Affirm that all `lev_*` calls are safe while holding a
  `lev_event_t`, and verify the projection deep copies payload bytes out of the
  `NodeEvent` so the event handle outlives the queue slot.
- last_error across the block_on boundary. The failing op runs on the tokio
  worker thread, but the detail must be set on the calling C thread. Capture the
  error in the block_on return value and set the caller TLS, not the worker.
- lev_free thread rule. The node owns a tokio runtime torn down in Drop
  (`shutdown_background`, `mod.rs:438-449`). `lev_free` must not run on a thread
  that is itself a worker of another runtime (the PyO3 and future JNI hazard the
  code comments flag). Document that `lev_free` runs on a plain OS thread.
- Builder ownership. `build_sync(self)` consumes the builder (`builder.rs:453`).
  A reusable C builder needs the facade to hold builder config, not the Rust
  builder, and to construct a fresh `ReticulumNodeBuilder` per build. New facade
  state, call it out.
- Restart. `stop()` clears the runtime, `start()` rebuilds it (`mod.rs:1084`).
  Decide and test whether stop then start is supported, or document start once.
- `lev_version_number` packs minor and patch in 8 bits each (max 255). State the
  cap or use `major*10000 + minor*100 + patch`.

## Scope

Consider whether resource transfer and request and response belong in v1. The
true minimal core is instance, identity, destination and announce, link send
and receive, datagram, and the event fd. Request, response, and resource could
be a fast follow. The v1 surface is about sixty functions; trimming would
sharpen the first shippable layer. Defensible either way, but decide
deliberately.
