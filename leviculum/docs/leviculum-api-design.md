# Leviculum public API design

Status: living document. This is the design of record for the Leviculum
public API: a curated Rust consumer facade plus a thin C FFI on top of it.
The goal is that a developer consumes Leviculum exactly like a normal Unix
C library:

```sh
apt install leviculum-dev
cc app.c -lleviculum
```

```c
#include <leviculum.h>
```

The C constraints (opaque handles, no generics, no async, byte buffers,
flat enums) are the forcing function. They are applied first; the Rust
facade is whatever those constraints project back into clean Rust. KISS is
the highest virtue.

Revision note. This revision folds in the consolidated design review
(`docs/api-design-review.md`). The load-bearing changes are: a precise
level-triggered eventfd contract (§4), the identity key split and an
identity-resolving `lev_connect` (§3, §10), a panic and mutex-poison
contract (§6), a bounded-blocking policy with timeouts (§5), and two
dimensions the first draft missed, logging (§12) and process-global state
plus init (§13). Where the review and the task brief disagree (resource and
request/response scope) the brief wins; where a review suggestion sits
outside the additive-only boundary (driver-internal poison recovery) the
achievable contract is stated instead.

A third review round (`docs/api-design-review-2.md`) is also folded in: the
eventfd invariant is made a true per-instant one by doing the counter
syscalls under the FIFO lock, with the drop policy never evicting a counted
event (§4); status and value are never multiplexed, value-returning calls use
out-parameters (§2); multi-payload events get per-field accessors (§4); and a
set of smaller honesty fixes (double-start maps to `LEV_ERR_CONFIG`, datagram
needs a known path, the action-timeout outcome is at-most-once with unknown
dispatch, `lev_init` runs through one `Once`, no request-handler unregister
in v1, `lev_connect` flagged as new glue).

Implementation status. The full v1 surface described here is implemented:
instance lifecycle, identity (including file load and save), version, the
error and panic harness, logging and init, the event fd bridge and its
accessors, destinations and announce, paths, connect and links (including
identify and remote identity), datagram, request and response, resource
transfer, and the hex helpers. Packaging (SONAME, pkg-config, install
layout) is in `leviculum-ffi/Makefile` and `leviculum.pc.in`. Each area has
a two-node C acceptance test under `examples/c/`. Still later, by design:
the Android per-ABI build, and any surface beyond v1 (LXMF, LXST). The
no-panic guard is enforced by `tests/guard_coverage.rs`.

## Scope and non-goals

In scope for v1: instance lifecycle, identity, destinations and announce,
path and connect, link send and receive and close, datagram send,
request and response, resource transfer, event draining, logging control,
error strings, and hex/bytes helpers.

Out of scope for v1: LXMF, LXST, the shared-instance daemon, RPC, fault
injection, and the diagnostic/stats surface (`transport_stats`,
`path_table_entries`, `rate_table_entries`, `drop_all_paths_via`, ...).
These stay internal to `leviculum-std` and never enter the facade.

The review proposed trimming request/response and resource transfer to a
fast follow. The task brief lists both in v1 (only LXMF and LXST are out),
so they stay in v1. The review's intent, a sharp first shippable layer, is
met by sequencing: phases a to c are the minimal core (instance, identity,
destination and announce, link send and receive, datagram, event fd) and
request/response and resource land last, in phases d and e.

Binding constraints from project policy: additive changes only to
`leviculum-core` and `leviculum-std` (new module plus re-exports, never a
refactor of signatures `lnsd`/`lnstest` depend on); stay out of
`leviculum-std/src/interfaces/` and `leviculum-nrf`. The facade must not
break wire or semantic compatibility with Python-RNS peers; it only
re-projects an already battle-tested engine.

## Architecture in one picture

```
   C application (owns its event loop)
        |  leviculum.h  (extern "C", opaque handles, int codes)
   leviculum-ffi  (cdylib + staticlib)
        |  panic guard, runtime, eventfd bridge, buffer marshalling
   leviculum_std::api  (curated Rust facade, own stable types)
        |  thin re-projection, no new behaviour
   leviculum_std::driver::ReticulumNode  (existing engine, unchanged)
        |
   leviculum_core  (state machine, wire format, crypto)
```

The facade adds no behaviour. It selects the app-relevant entry points out
of the ~40 on `ReticulumNode`, gives them stable types that hide core
internals (`DestinationHash`, `LinkId`, `transport::*`), and presents the
event stream in a shape the C layer can project mechanically. The C layer
adds only what C forces: a hidden runtime, a panic guard, a pollable event
fd, and buffer marshalling.

## 1. Object and handle model

Every C type is an opaque pointer with a `create`/`destroy` pair. The app
never sees a struct layout, so the ABI stays stable across versions.

| C handle | Wraps | Created by | Destroyed by |
| --- | --- | --- | --- |
| `leviculum_t` | the node, its runtime, the event bridge | `lev_builder_build` | `lev_free` |
| `lev_builder_t` | node configuration before build | `lev_builder_new` | `lev_builder_free` |
| `lev_identity_t` | a key pair or public-only identity | `lev_identity_generate`, `lev_identity_from_*`, `lev_link_remote_identity` | `lev_identity_free` |
| `lev_destination_t` | a local destination (in or out) | `lev_destination_new` | `lev_destination_free` |
| `lev_link_t` | one established or pending link | `lev_connect` / `lev_accept_link` | `lev_link_free` (also closes if open) |
| `lev_event_t` | one drained event | `lev_next_event` / `lev_wait_event` | `lev_event_free` |

Note the third row: some accessors mint a handle. `lev_link_remote_identity`
maps to `get_remote_identity(&LinkId) -> Option<Identity>`, which returns an
owned `Identity`; the FFI boxes it into a fresh `lev_identity_t` that the
caller frees with `lev_identity_free`, exactly like a constructor result.

Addresses are not handles. A destination hash and a link id are each a
fixed 16-byte value. They cross the boundary as `uint8_t[16]` (or as a hex
string through the helpers), never as an opaque pointer. This keeps the
common "I read an announce, now connect to it" path free of allocation.

Builder ownership is decided: the builder is single-use. `lev_builder_build`
consumes the builder's configuration and leaves the handle empty but valid;
the caller still calls `lev_builder_free` on the empty shell. A second
`lev_builder_build` on the same handle returns `NULL` with
`LEV_ERR_INVALID_ARG`. This is simpler than a reusable builder and needs no
extra facade state. The general rule holds: every constructor or accessor
that returns a handle has a matching `_free`, and `_free(NULL)` is a no-op.

## 2. Error handling

Classic Unix. Functions that can fail return `int`: `0` on success, a
negative `LEV_ERR_*` code on failure. Constructors that return a handle
return `NULL` on failure and set the thread-local last error. The codes are
emitted as `int` constants (not a C enum) so functions return plain `int`
and the values are the exact `LEV_*` spelling.

Status and value are never multiplexed into one return. A function that both
can fail and produces a value returns the `int` status and writes the value
through an out-parameter, the same read(2) convention the event accessors
use. So a send that yields a packet or resource hash is
`int lev_send_datagram(leviculum_t *node, const uint8_t dest[16],
const uint8_t *data, size_t len, uint8_t out_hash[16])`, not a function that
returns the hash. Section 10's `-> hash` and `-> request id` notes are
shorthand for "the value lands in a fixed-size out-parameter"; the only
functions that return a non-`int` directly are the infallible accessors
(`lev_version_number`, `lev_strerror`, `lev_last_error`) and the
handle-returning constructors.

```c
#define LEV_OK                  0
#define LEV_ERR_NULL_PTR       -1
#define LEV_ERR_INVALID_ARG    -2
#define LEV_ERR_BUFFER_TOO_SMALL -3
#define LEV_ERR_NOT_RUNNING    -4    /* event loop down */
#define LEV_ERR_IO             -5
#define LEV_ERR_CONFIG         -6
#define LEV_ERR_CRYPTO         -7
#define LEV_ERR_NO_PATH        -8    /* path unknown, call lev_request_path */
#define LEV_ERR_LINK           -9    /* link closed/inactive/handshake */
#define LEV_ERR_SEND           -10   /* too-large/no-route */
#define LEV_ERR_RESOURCE       -11
#define LEV_ERR_REQUEST        -12
#define LEV_ERR_TIMEOUT        -13   /* blocking call exceeded its deadline */
#define LEV_ERR_AGAIN          -14   /* non-fatal backpressure, retry */
#define LEV_ERR_UNKNOWN_DEST   -15   /* no cached identity for the dest hash */
#define LEV_ERR_PANIC          -127  /* caught panic, see §6 */
```

Exact mapping from the facade error to a code. The real
`leviculum_std::error::Error` has ten variants; the table is the authority,
no variant falls through:

| `Error` variant | code |
| --- | --- |
| `Io` | `LEV_ERR_IO` |
| `Storage` | `LEV_ERR_IO` |
| `Config` | `LEV_ERR_CONFIG` |
| `Serialization` | `LEV_ERR_INVALID_ARG` |
| `NotRunning` | `LEV_ERR_NOT_RUNNING` |
| `Announce` | `LEV_ERR_SEND` |
| `Send` | `LEV_ERR_SEND` |
| `Link` | `LEV_ERR_LINK` |
| `Resource` | `LEV_ERR_RESOURCE` |
| `Request` | `LEV_ERR_REQUEST` |

The FFI-only codes (`NULL_PTR`, `INVALID_ARG`, `BUFFER_TOO_SMALL`, `AGAIN`,
`TIMEOUT`, `UNKNOWN_DEST`, `PANIC`) have no `Error` source; the boundary
sets them directly.

Two layers of detail:

- `const char *lev_strerror(int code)` returns a static, never-freed string
  for the code. Stable, allocation-free, safe to call any time.
- `const char *lev_last_error(void)` returns a thread-local string with the
  specific failure detail (for example which argument, which address). It
  is owned by the library, valid until the next failing call on the same
  thread, and must not be freed by the caller.

last_error and the runtime boundary. An async engine call is driven with
`block_on`; the failure surfaces as the `Result` returned to the calling C
thread, and the boundary sets `lev_last_error` from that returned value on
the C thread. The library never writes the thread-local from an engine
worker thread, so the detail string always belongs to the thread that made
the call. No `errno` global; the thread-local detail string is the only
mutable error state and it never escapes the thread.

`LEV_ERR_AGAIN` maps the link `try_send` "Busy"/"PacingDelay" case
(`stream.rs`) so an app can poll without treating backpressure as failure,
matching `EAGAIN`.

## 3. Memory ownership

The default is read(2) style: the caller owns the buffer, the library
fills it.

```c
int lev_identity_public_key(const lev_identity_t *id,
                            uint8_t *buf, size_t cap, size_t *out_len);
```

Contract for every such function:

- The caller passes `buf` and its capacity `cap`, plus `out_len`.
- On success the library writes the bytes and sets `*out_len` to the count.
- If `cap` is too small, the library writes nothing, sets `*out_len` to the
  required size, and returns `LEV_ERR_BUFFER_TOO_SMALL`. The caller resizes
  and retries. Passing `buf == NULL` to learn the size is allowed.

Fixed-size outputs document their exact length as a constant so callers can
stack-allocate. The key length is the one place the first draft was
misleading, so it is spelled out here:

```c
#define LEV_ADDR_LEN        16   /* destination hash, link id, identity hash */
#define LEV_IDENTITY_KEY_LEN 64  /* combined public OR private key */
#define LEV_X25519_KEY_LEN  32   /* encryption half, bytes 0..32  */
#define LEV_SIGNING_KEY_LEN 32   /* Ed25519 half, bytes 32..64    */
#define LEV_SIGNATURE_LEN   64
```

The 64-byte key is not one key. Verified against `constants.rs`:
`IDENTITY_KEY_SIZE = X25519_KEY_SIZE + ED25519_KEY_SIZE = 64`. Both
`lev_identity_public_key` and `lev_identity_private_key` return 64 bytes
laid out as the X25519 key in bytes `0..32` (encryption) then the Ed25519
key in bytes `32..64` (signing). A link needs only the Ed25519 signing
half; `driver/mod.rs` documents the signing key as bytes `32..64` of
`public_key_bytes()`. A C author must never split this by hand: see §10,
`lev_connect` takes a destination hash and resolves the signing key from
the cached identity internally.

String and byte conventions, stated once:

- Opaque byte buffers (keys, hashes, datagram payloads, link data, resource
  data, app_data) are always `(pointer, length)` and never NUL-terminated.
  Length is explicit and may contain embedded zero bytes.
- Human-readable strings the library consumes (storage path, destination
  `app_name`, request `path`) are NUL-terminated UTF-8 C strings
  (`const char *`).
- A destination's aspects are a list of strings, marshalled as a
  `const char *const *` array plus a `size_t` count:
  `lev_destination_new(id, direction, type, const char *app_name,
  const char *const *aspects, size_t n_aspects, lev_destination_t **out)`.
  This matches `Destination::new(.., app_name: &str, aspects: &[&str])`.
- Library-returned static strings (`lev_strerror`, `lev_last_error`,
  `lev_version_string`) are NUL-terminated and never freed by the caller.

Where a result has no caller-knowable bound, the library allocates and owns
it behind a handle that the caller frees. This is the case for events: an
event carries a variable payload, so `lev_event_t` is an owned handle and
its payload is read out through accessors into a caller buffer (read(2)
style again) before `lev_event_free`. No raw library-allocated pointer is
ever handed to C for the app to `free()`; freeing always goes through a
typed `lev_*_free`. This removes allocator-mismatch bugs (C `free` vs Rust
`dealloc`).

## 4. The event and receive model

This is the central decision and the library's load-bearing mechanism, so it
is specified exactly. The engine delivers everything inbound, link data
included (`LinkHandle` is send-only; see `driver/stream.rs`), as
`NodeEvent`s on a tokio channel via `EventReceiver`. C has no async and no
tokio. The model must let a C app compose Leviculum with its own event loop
on Linux now and on Android later.

Decision: a level-triggered eventfd in semaphore mode, paired with an
internal queue, plus a drain call.

```c
int  lev_event_fd(const leviculum_t *node);                 /* readable fd */
int  lev_next_event(leviculum_t *node, lev_event_t **out);  /* dequeue, non-blocking */
int  lev_wait_event(leviculum_t *node, lev_event_t **out, int timeout_ms);
void lev_event_free(lev_event_t *ev);
```

Mechanics. The library owns two things: an internal FIFO of projected
events guarded by a mutex, and an eventfd created `EFD_SEMAPHORE |
EFD_NONBLOCK` on Linux (a self-pipe where eventfd is unavailable). A single
bridge task is the only producer; `lev_next_event` is the only consumer.

Which runtime hosts the bridge, and surviving restart. The bridge runs on
the FFI runtime owned by `leviculum_t`, which lives from `lev_builder_build`
to `lev_free`, not on the node's own runtime, which `start`/`stop` create
and tear down. The engine's event channels are created in
`ReticulumNode::new` at build time, not in `start` (`driver/mod.rs`), so the
`EventReceiver` the bridge owns stays valid across a `stop`/`start` cycle:
the bridge keeps awaiting `recv`, sees no events while the node is stopped,
and resumes when `start` brings the loop back. This is why a restart does
not strand the event fd.

Preserving control priority. The engine splits events into a lossless
control plane and a droppable data plane, and `EventReceiver::recv` drains
control with strict priority over data (`driver/mod.rs`). The bridge pulls
exclusively through `EventReceiver::recv` rather than racing the two
channels itself, so control-first ordering is preserved into the FIFO, and
the FIFO's insertion order then keeps control events ahead of later data.
Losslessness is preserved separately by the per-plane caps below.

Invariant: the eventfd counter always equals the number of events currently
in the queue. To make this a true per-instant invariant (not merely an
eventual one), the eventfd syscall is done inside the FIFO mutex, paired with
the push or pop, so a counter and a length never disagree while the lock is
free:

- Bridge, per accepted event: lock, push to the FIFO, `write(fd, 1)` to
  increment, unlock.
- `lev_next_event`: lock, pop one; if a pop happened, `read(fd)` once to
  decrement by 1, unlock.

Doing the syscalls under the lock has two consequences that close the seams a
naive lock-then-syscall ordering would leave open:

- The consumer `read` never sees `EAGAIN`. It reads only after a successful
  pop, when the counter (mutated under the same lock as the FIFO) is provably
  `>= 1`. There is no producer-push-before-fd-write window for the consumer
  to race into. The eventfd stays `EFD_NONBLOCK` purely as a belt-and-braces
  guard; a spurious `EAGAIN` is treated as "nothing to decrement", never an
  error and never an underflow.
- No counted event is ever silently evicted (see the overflow policy below),
  so the counter never runs ahead of the queue and the fd is never readable
  with an empty queue. The eventfd syscalls are cheap (sub-microsecond,
  non-blocking) and the critical section is tiny, so holding the lock across
  them costs nothing at realistic event rates.

Readiness is therefore a pure function of queue-non-empty: `poll`/`epoll`
report the fd readable iff the counter is `> 0` iff at least one event is
queued. This kills both failure modes the review named:

- No lost wakeup. An event accepted just after the consumer drained to empty
  increments the counter under the lock and re-arms the fd. The counter, not
  a one-shot flag, is the signal.
- No busy loop. Once the queue is empty the counter is `0` and `poll`
  blocks.

The poll example is correct exactly as written:

```c
struct pollfd p = { .fd = lev_event_fd(node), .events = POLLIN };
poll(&p, 1, -1);
lev_event_t *ev;
while (lev_next_event(node, &ev) == LEV_OK && ev) {
    switch (lev_event_type(ev)) { /* ... */ }
    lev_event_free(ev);
}
```

`lev_next_event` returns `LEV_OK` with `*out == NULL` when the queue is
empty. The contract is strictly level-triggered. An app may register the fd
edge-triggered (`EPOLLET`) provided it drains with `lev_next_event` until
`NULL` on each wake, which the semaphore counter makes safe.

`lev_wait_event` blocks up to `timeout_ms` (negative means forever) for the
next event, implemented on the same queue. If `lev_stop` or `lev_free` runs
on another thread while a wait is parked, the parked wait returns
`LEV_ERR_NOT_RUNNING` rather than blocking forever.

Ownership and lifetime of the fd. The library creates the fd and closes it
in `lev_free`; the app must never close it. It is valid from `lev_event_fd`
(any time after build) until `lev_free`. Shutdown ordering is mandatory:
stop reacting to the fd, remove it from your loop, then call `lev_free`.
Polling an fd after `lev_free` is a use-after-close, since the integer can
be recycled by the OS. The event side is single-consumer: `lev_next_event`
and `lev_wait_event` for one node must not run concurrently (the fd may be
polled from anywhere, but draining is not concurrent).

Event payload lifetime. The projection deep-copies all payload bytes
(datagram data, link data, completed resource, `app_data`) out of the
`NodeEvent` into the `lev_event_t` at dequeue time, so the handle is fully
self-owned. Every `lev_*` call is safe while one or more `lev_event_t` are
held; an event outlives the queue slot it came from and is valid until
`lev_event_free`.

Queue bound and overflow. The FIFO mirrors the engine's split (Codeberg
#71): control events are lossless up to a high cap, data events are
droppable under backpressure. Crucially, the cap is enforced at enqueue, so a
dropped event is never counted and never writes the fd, keeping the
counter-equals-length invariant intact (the bridge never evicts an
already-enqueued, already-counted event, which would otherwise require a
compensating `read`). When the data region is full the bridge drops the
incoming data event, exactly as the engine's own data plane does when its
bounded `try_send` is full; this is normal backpressure. A control overflow
is coalesced into a single `LEV_EVENT_CONTROL_OVERFLOW` event carrying the
dropped count, itself enqueued (and counted) only once there is room, so loss
is always visible and never itself lost. Capacities are configurable on the
builder (`lev_builder_event_capacity`), defaulting to the engine's control
and data channel capacities.

Why a pollable fd and not callbacks:

- The app keeps ownership of its loop. The library never calls into app
  code, so there is no re-entrancy hazard and no rule against calling
  library functions from inside a callback.
- Callbacks would fire on the library's tokio worker thread. Foreign-thread
  upcalls are exactly what language bindings (JNI in particular) handle
  worst; they force thread attach/detach and global-state locking onto the
  app.
- An fd composes with `select`/`poll`/`epoll` and with Android's
  `ALooper_addFd`, which takes any fd. The same code path serves both
  targets, with no dependency on one event-loop technology.

Callbacks are explicitly the rejected fallback and are recorded as such. The
one place the library does upcall, logging (§12), is opt-in and documented
with its threading rules.

Event payloads are read through typed accessors, never a transparent union
(unions over variable-length data are an ABI and lifetime hazard):

```c
int lev_event_type(const lev_event_t *ev);
int lev_event_link_id(const lev_event_t *ev, uint8_t out[16]);
int lev_event_dest_hash(const lev_event_t *ev, uint8_t out[16]);
int lev_event_data(const lev_event_t *ev,
                   uint8_t *buf, size_t cap, size_t *out_len);
```

A single `lev_event_data` cannot express events that carry more than one
payload, so the projection adds per-field accessors for those, each read(2)
style and each returning `LEV_ERR_INVALID_ARG` on an event that lacks the
field:

- `ResourceCompleted` carries data plus metadata: `lev_event_metadata`
  reads the metadata, `lev_event_data` the data.
- `RequestReceived` carries a path string, a request id, and data:
  `lev_event_path`, `lev_event_request_id` (16 bytes), `lev_event_data`.
- `ResourceProgress` exposes its progress fraction via `lev_event_progress`.
- `ResourceAdvertised` and `ResourceProgress` carry the two sizes an
  accept-or-reject decision turns on: `lev_event_transfer_size` and
  `lev_event_data_size`.
- `ResourceCompleted` also carries its position in a multi-segment transfer:
  `lev_event_segment_index` and `lev_event_total_segments`.
- `LinkClosed` carries a close reason (`lev_event_close_reason`, one of the
  `LEV_CLOSE_*` constants) and `DeliveryFailed` a failure reason
  (`lev_event_delivery_error`, `LEV_DELIVERY_*`). Both are behavioural for the
  caller: `LEV_CLOSE_BLACKHOLED` must not be retried, and
  `LEV_DELIVERY_INVALID_PROOF` means a re-send cannot succeed.
- `AnnounceReceived`, `PathFound` and `PacketReceived` carry the interface they
  arrived on (`lev_event_interface_id`).

Settling these accessors now, before the header for phases d and e calcifies,
is the point: a one-payload event model would have to be widened later.

An accessor that does not apply to the event's type returns
`LEV_ERR_INVALID_ARG`. The facade `Event` enum collapses the `NodeEvent`
variants to the v1-relevant set and drops internal fields
(observability-only `ChannelRetransmit`); the lossless
`ControlPlaneOverflow` is surfaced as `LEV_EVENT_CONTROL_OVERFLOW` so loss
stays visible. Reaching a peer from an `AnnounceReceived` event needs only
its destination hash: `lev_connect` takes the hash and resolves the cached
identity internally (§10), so no event accessor needs to export raw keys.

## 5. Threading and blocking

The tokio runtime is created and owned inside `leviculum_t` and never
exposed. There are two independent runtimes, and the distinction matters for
the blocking contract:

- The node's own runtime. `ReticulumNode::start` builds a multi-thread
  runtime with one worker (`new_multi_thread().worker_threads(1)`,
  `driver/mod.rs`) and spawns the event loop on it. That worker is a real OS
  thread, so the loop runs independent of any C thread (this is what makes
  the deadlock-freedom argument below hold; a `new_current_thread` runtime
  would only advance when blocked on).
- The FFI runtime. The boundary holds one current-thread runtime used only
  to `block_on` the engine's async methods (`connect`, `announce`, `send`,
  `stop`). `block_on` drives the future on the calling C thread.

Consequence: while a C thread is parked in a blocking call, the node's event
loop keeps turning on its own runtime and keeps draining the internal action
channel. A blocking send therefore does not stall the loop or deadlock send
progress; it can still block the calling thread for a while, which is what
the bounded-blocking policy below addresses. The review's hard-deadlock
concern does not apply because of this two-runtime split; the
unbounded-blocking concern does, and is bounded here.

Bounded-blocking policy. No boundary call blocks unboundedly.

| function | blocking |
| --- | --- |
| `lev_link_try_send` | never blocks; `LEV_ERR_AGAIN` on backpressure (the loop-friendly default) |
| `lev_link_send` | blocks up to `timeout_ms`; `LEV_ERR_TIMEOUT` on expiry |
| `lev_connect`, `lev_accept_link`, `lev_announce`, `lev_close_link`, `lev_request_path`, `lev_send_datagram`, `lev_send_request`, `lev_send_resource` | enqueue one action on the bounded action channel; block only briefly while the independent loop drains it, capped by `timeout_ms`, `LEV_ERR_TIMEOUT` on expiry |
| `lev_start`, `lev_stop` | block until the loop is up or down |
| queries (`lev_has_path`, `lev_hops_to`, `lev_is_running`, `lev_link_id`, ...) | lock-and-return, never block on I/O |

The link data path is `try_send`-first by design, matching the
compose-with-your-loop philosophy: an app polls and re-tries on
`LEV_ERR_AGAIN`. `lev_link_send` is the convenience wrapper with a deadline.
`LinkHandle::send`'s internal retry-on-Busy loop (`stream.rs`) is bounded by
the `timeout_ms` the boundary enforces, so a congested LoRa link surfaces
`LEV_ERR_TIMEOUT` instead of hanging.

Where the deadline lives. The facade async methods carry no timeout (only
`send_request` has a request-response timeout, which is a different thing),
and the action-enqueue methods just `.await` a bounded channel send
(`action_dispatch_tx.send(..).await`, `driver/mod.rs`). So the boundary, not
the facade, enforces every deadline, by driving the future under
`block_on(tokio::time::timeout(dur, fut))`. Two outcomes are distinguished:
the FFI deadline expiring maps to `LEV_ERR_TIMEOUT`, while the event loop
being down (the channel closed) maps to `LEV_ERR_NOT_RUNNING`. In practice
the action channel only backs up while the loop is mid-iteration (it drains
every iteration and runs on its own runtime, see above), so these enqueue
calls return promptly; the deadline is the backstop, and the genuinely
unbounded case it tames is `LinkHandle::send`'s retry loop.

What a timeout means for the action. On `LEV_ERR_TIMEOUT` from an
action-enqueue call, the action may or may not have been dispatched: the
`block_on(timeout(..))` abandons the future, but the send may have already
landed on the channel just as the deadline fired. The library does not
retry, and the C caller cannot tell from the return code alone. The contract
is therefore at-most-once with an unknown outcome on timeout. For the
naturally idempotent calls (`lev_announce`, `lev_request_path`) a retry is
harmless. For `lev_connect` a timeout that did dispatch may still surface a
later `LinkEstablished` or `LinkClosed` event, which the app should be
prepared to see; the safe pattern is to treat the link id as tentative until
its establishment event arrives.

Thread-safety guarantees, documented in the header:

- `leviculum_t` is thread-safe. Its methods may be called concurrently from
  multiple threads (the engine is `Arc<Mutex<..>>` internally). Sends and
  connects from different threads are serialized correctly.
- The event side is single-consumer (§4).
- `lev_identity_t` and `lev_destination_t` are not internally synchronized.
  Treat one handle as owned by one thread, or guard it externally.
- Every handle is safe to `*_free` exactly once; double free is the app's
  bug, `_free(NULL)` is a no-op.

`lev_free` and `lev_stop` thread rule. The node owns a tokio runtime torn
down in `Drop` via `shutdown_background` (`driver/mod.rs`), and `lev_stop`
and `lev_free` drive `block_on`. `block_on` panics if called from inside
another runtime's worker thread, so `lev_free`, `lev_stop`, and every other
blocking boundary call must run on a plain OS thread, never on a thread that
is itself a worker of a host runtime (the PyO3 and future JNI hazard the
engine comments flag). This is a documented precondition.

The same rule makes the logging callback constraint in §12 load-bearing, not
mere etiquette: a log record can be emitted from the node's runtime worker,
so the callback runs on that worker; if it called a blocking `lev_*`
function, that function's `block_on` would run inside a runtime worker and
panic. The "a log callback must not call back into `lev_*`" rule in §12 is
this soundness constraint, cross-referenced here.

## 6. No panic across the FFI boundary

Unwinding into C is undefined behaviour. Every `extern "C"` function wraps
its entire body in `std::panic::catch_unwind` and converts a caught panic
into `LEV_ERR_PANIC` (or `NULL` for constructors). A single guard enforces
this so no function can forget it:

```rust
pub(crate) fn guard<T>(default: T, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => { set_last_error_static("panic in libleviculum"); default }
    }
}
```

`AssertUnwindSafe` is a deliberate assertion, not an oversight: the boundary
state that survives a panic is the thread-local last error (a string, always
consistent) and the engine's `Arc<Mutex<..>>`. The real hazard is mutex
poison, and the contract names it explicitly.

Post-panic contract. If an operation panics while holding the engine's core
mutex, `std::sync::Mutex` marks it poisoned. The driver locks with
`.lock().unwrap()` throughout (`driver/mod.rs`), so a later call on the same
node would panic inside the guard and return `LEV_ERR_PANIC` again. The
review suggested recovering poison with `lock().unwrap_or_else(|e|
e.into_inner())`, but those lock sites are driver-internal and changing them
is outside the additive-only boundary for code `lnsd`/`lnstest` depend on. The
achievable and stated contract is therefore:

- A caught panic from a node operation may leave that `leviculum_t` in a
  poisoned state. After a `LEV_ERR_PANIC` from a node call, the only
  supported operation on that node is `lev_free`. Other handles
  (`lev_identity_t`, other nodes) are unaffected.
- `lev_free` after a poisoning panic still reclaims all memory, but it
  cannot persist state cleanly: the graceful `stop()` path locks the same
  poisoned mutex (`save_persistent_state` -> `inner.lock().unwrap()`,
  `driver/mod.rs`) and would re-panic, so `lev_free` skips the graceful stop
  on a poisoned node and tears down via `Drop` (`shutdown_background`). The
  consequence is an honest one: the final storage `flush()` is lost in the
  panic case, recovered later from fresh announces. No clean shutdown is
  implied.
- Output parameters are indeterminate after a caught panic; the caller must
  not read them.
- The error arm is allocation-free: `set_last_error_static` stores a
  `&'static CStr` (a C string literal), doing no work that could itself
  panic. The fallible `set_last_error` path that allocates is used only on
  the non-panic error arms.

`panic = unwind` is pinned in the workspace release profile (it is already
the default, so this is behaviour-preserving), never `abort`, so
`catch_unwind` actually catches. This is not merely a tidiness choice: a
`cdylib` built with `panic = abort` would turn every internal Rust panic
into a process abort of the host application, which is unacceptable for a
library.

Enforcement. The `guard` function is the wrapper every boundary body calls,
but a function can still be forgotten when a new `extern "C"` fn is written.
v1 backs it with a test that greps the FFI source and fails if any
`#[no_mangle] extern "C"` body does not go through `guard`. A stronger option
is a proc-macro attribute that generates the guarded body so it cannot be
written without it; that is recorded as a later hardening, kept out of v1 to
avoid the macro-crate weight, with the grep test as the backstop until then.

## 7. ABI and versioning

Semantic versioning, exposed at compile time and run time, both sourced from
the crate version (a single source of truth, not a second hand-maintained
number):

```c
#define LEVICULUM_VERSION_MAJOR <from crate>
#define LEVICULUM_VERSION_MINOR <from crate>
#define LEVICULUM_VERSION_PATCH <from crate>
const char *lev_version_string(void);   /* e.g. "0.7.0" */
uint32_t    lev_version_number(void);    /* (major<<16)|(minor<<8)|patch */
```

`lev_version_number` packs the components into one `uint32_t` as
`(major << 16) | (minor << 8) | patch`. Minor and patch are therefore capped
at 255; this is documented in the header and is ample for the foreseeable
series. The string accessor is authoritative and has no cap. The packed
number is a host-byte-order integer meant for in-process comparisons
(`lev_version_number() >= 0x000603`), not for serialization or
wire exchange; serialize the string if a version must cross a process or
host boundary.

All structs are opaque, so adding fields never breaks the ABI. New
functionality is added as new functions; existing signatures are frozen once
shipped. SONAME plan: `libleviculum.so.0` for the 0.x series, bumping the
major soname only on a breaking ABI change. While the crate version is
`0.x`, the C ABI is explicitly unstable and the header carries a note to
that effect. The stability promise begins at 1.0.

## 8. Naming

One prefix on every symbol, because C has no namespaces.

- Functions: `lev_*` (`lev_connect`, `lev_announce`, `lev_identity_generate`).
- Constants and enum values: `LEV_*` (`LEV_OK`, `LEV_ADDR_LEN`,
  `LEV_EVENT_LINK_ESTABLISHED`, `LEV_REQUEST_POLICY_ALLOW_ALL`).
- Version macros: `LEVICULUM_VERSION_*` (the brand spelled out).
- Types: the main handle is `leviculum_t`; all others are `lev_*_t`
  (`lev_identity_t`, `lev_link_t`, `lev_event_t`).

Flat enums replace the Rust sum types at the boundary:

- Request policy: `LEV_REQUEST_POLICY_ALLOW_NONE`,
  `LEV_REQUEST_POLICY_ALLOW_ALL`, `LEV_REQUEST_POLICY_ALLOW_LIST`. The list
  variant carries data in Rust (`AllowList(Vec<[u8; 16]>)`), which a flat
  enum cannot, so the allowlist is passed alongside as
  `lev_register_request_handler(node, dest_hash, path, policy, const uint8_t
  *allow_identity_hashes, size_t n_ids)`, where `allow_identity_hashes` is
  `n_ids * 16` bytes of identity hashes (not generic ids) and is read only
  for the `ALLOW_LIST` policy. There is no unregister in v1: registering a
  handler for a `(dest_hash, path)` pair overwrites any previous one, and
  handlers live for the node's lifetime. A C author should not look for a
  `lev_unregister_request_handler`.
- Resource strategy: `LEV_RESOURCE_ACCEPT_NONE`, `LEV_RESOURCE_ACCEPT_ALL`,
  `LEV_RESOURCE_ACCEPT_APP`.
- Destination direction and type, and event types, are likewise flat
  `LEV_*` enums.

Rename recorded: the existing stub uses the `lns_` prefix and emits
`reticulum.h`. v1 renames to the `lev_` prefix, header `leviculum.h`,
library `libleviculum.so`. No `lns_` symbol survives into v1. The cdylib
output name is set to `leviculum` so the linker flag is `-lleviculum`.

## 9. Header generation

Decision: cbindgen generates `leviculum.h` from the Rust source on every
build, and the header is a build artifact, not committed (the repo already
gitignores `leviculum-ffi/*.h`). The Rust source is the single source of
truth; packaging installs the freshly generated header.

Rationale. The worst class of C-library bug is a header that disagrees with
the shipped symbols. Regenerating the header on every build from the same
crate that produces the symbols removes that class entirely and by
construction: there is no committed copy that can go stale. This is strictly
safer than committing the header with a CI no-diff guard, and it matches the
existing repo convention. cbindgen is already wired (`build.rs`,
`cbindgen.toml`); we invest in making the output read like a hand-written
header (doc comments carried over, ordering and wrapping controlled, opaque
types as clean forward declarations).

The one cost is that someone browsing the repo does not see the header
without building. That is acceptable for a generated artifact. If a hand
curated header is ever wanted, the fallback is to commit it and guard it
with a symbol-match CI check (`nm` on the cdylib vs the declarations).
Recorded as the alternative, not chosen for v1.

## 10. The v1 surface

Kept deliberately tiny. Grouped by area; every line is one C function unless
noted.

Initialisation and logging (§12, §13):
- `lev_init` (one-time process setup), `lev_log_set_level`,
  `lev_log_set_callback`

Instance and version:
- `lev_builder_new`, `lev_builder_free`
- `lev_builder_identity`, `lev_builder_storage_path`,
  `lev_builder_add_tcp_client`, `lev_builder_add_tcp_server`,
  `lev_builder_add_udp`, `lev_builder_add_auto_interface`,
  `lev_builder_enable_transport`, `lev_builder_event_capacity`
- `lev_builder_build` -> `leviculum_t`
- `lev_start`, `lev_stop`, `lev_is_running`, `lev_free`
- `lev_version_string`, `lev_version_number`

`lev_start` on an already-running node is not a no-op: the engine returns
`Error::Config("node already running")`, which maps to `LEV_ERR_CONFIG`.
Check `lev_is_running` first if a double start is possible.

Identity:
- `lev_identity_generate`, `lev_identity_from_private_key`,
  `lev_identity_from_public_key`, `lev_identity_free`
- `lev_identity_hash`, `lev_identity_public_key`,
  `lev_identity_private_key`, `lev_identity_has_private_keys`
- `lev_identity_load_file`, `lev_identity_save_file` (NEW surface, see note)

The two file functions are not pure re-typing of core `Identity`, which has
no file IO. They are new facade code over `leviculum-std`'s
`FileIdentityStore`, and they carry a Python file-format compatibility
decision (the 64-byte transport identity format, `builder.rs`). Decision:
reuse `FileIdentityStore` so the on-disk format stays byte-compatible with
Python RNS and the existing daemons; do not invent a new format.

Destinations and announce:
- `lev_destination_new` (identity, direction, type, app_name, aspects array),
  `lev_destination_free`
- `lev_destination_hash`
- `lev_register_destination`
- `lev_announce` (dest hash, optional app_data)

Path and connect:
- `lev_has_path`, `lev_request_path`, `lev_hops_to`
- `lev_connect` (node, dest hash) -> `lev_link_t`. Resolves the peer's
  signing key from the cached identity learned via announce; returns
  `LEV_ERR_UNKNOWN_DEST` if no identity is known yet (request a path or wait
  for the announce first), and does not auto-request a path
  (`LEV_ERR_NO_PATH` if the path is unknown).
- `lev_connect_with_key` (node, dest hash, signing_key[32]) -> `lev_link_t`,
  for out-of-band keys where no announce was seen.
- `lev_accept_link` (link id from a `LinkRequest` event) -> `lev_link_t`

Hiding the key split behind `lev_connect` is the resolution of review point
B3: a C author never extracts bytes `32..64` by hand. The explicit variant
remains for completeness.

`lev_connect` is NEW facade glue, not a pure re-typing, on par with the file
IO above. The engine and core `connect` both take the Ed25519 signing key as
an argument; neither does the lookup. The facade does it: call
`Storage::get_identity(dest_hash) -> Option<Identity>` (`driver/mod.rs`),
take bytes `32..64` of its public key as the signing key, then call the
engine `connect`. It is additive and small, but phase c budgets for it as
real code rather than a one-liner.

Link send, receive, close:
- `lev_link_try_send` (non-blocking, `LEV_ERR_AGAIN` on backpressure),
  `lev_link_send` (blocks up to `timeout_ms`)
- `lev_link_id`, `lev_link_is_closed`
- `lev_link_identify`, `lev_link_remote_identity` (mints a `lev_identity_t`)
- `lev_close_link`, `lev_link_free`
- receive is via the event stream (`LinkDataReceived`, `MessageReceived`)

Datagram:
- `lev_send_datagram` (dest hash + bytes, single packet), packet hash into a
  16-byte out-parameter. Precondition: a path to the destination must already
  be known (`send_single_packet` requires it, `driver/mod.rs`); if not, it
  returns `LEV_ERR_NO_PATH`, the same precondition as `lev_connect`, so call
  `lev_request_path` first. Unreliable: a `PacketDeliveryConfirmed` event
  arrives only if the destination returns a proof (proof strategy dependent),
  so a C app must not block waiting for a confirmation that may never come.
  Reliable delivery events are a property of links and resources, not
  datagrams.

Request and response:
- `lev_register_request_handler` (dest hash, path, policy,
  allow_identity_hashes, n_ids)
- `lev_send_request` (link, path, optional data, timeout) -> request id
- `lev_send_response` (link, request id, data)
- request/response arrival and timeout arrive as events

Resource transfer:
- `lev_send_resource` (link, data, optional metadata, auto_compress)
  -> 32-byte resource hash
- `lev_set_resource_strategy` (flat `LEV_RESOURCE_*`),
  `lev_accept_resource`, `lev_reject_resource`
- progress and completion arrive as events

Events:
- `lev_event_fd`, `lev_next_event`, `lev_wait_event`, `lev_event_free`
- `lev_event_type` and typed accessors

Errors and helpers:
- `lev_strerror`, `lev_last_error`
- `lev_hex_encode`, `lev_hex_decode`

## 11. Packaging

Linux first, Android-aware in the design.

Linux v1:
- `crate-type = ["cdylib", "staticlib"]` (already set), output name
  `leviculum` so the link flag is `-lleviculum`. The cdylib is built against
  glibc (the `build-ffi` alias targets `x86_64-unknown-linux-gnu`); the
  workspace musl default cannot emit a cdylib and only produces the
  staticlib.
- SONAME `libleviculum.so.0`, with the usual `libleviculum.so` ->
  `libleviculum.so.0` dev symlink.
- Install layout: `libleviculum.so*` in the lib dir, `leviculum.h` in the
  include dir, and a generated `leviculum.pc` pkg-config file so
  `pkg-config --cflags --libs leviculum` and the `apt install leviculum-dev`
  feel both work.
- A worked C example under `examples/c/` that links the real `.so` and does
  a live send/recv is the per-phase acceptance test.

Android later (designed for, not built in v1):
- Per-ABI `cdylib` via cargo-ndk (arm64-v8a, armeabi-v7a, x86_64).
- No Linux-only assumptions: the event fd uses eventfd with a self-pipe
  fallback, both present on Android; the hidden runtime is tokio, which
  builds for Android; no `epoll`-specific API leaks into the public surface;
  `ALooper_addFd` consumes the same fd.

## 12. Logging

A clean C library must let the app see and control diagnostics. The stack
uses `tracing` pervasively, and a C app can neither install a Rust
subscriber nor read `tracing` output by default, so the library owns this.

- `lev_log_set_level(int level)` sets a global filter
  (`LEV_LOG_OFF`/`ERROR`/`WARN`/`INFO`/`DEBUG`/`TRACE`). Default is
  `LEV_LOG_OFF` (a library that is silent unless asked, like most C libs).
- `lev_log_set_callback(void (*cb)(int level, const char *msg, void *user),
  void *user)` routes log records to the app, in the spirit of libcurl's
  `CURLOPT_DEBUGFUNCTION`. Passing `NULL` restores the default sink
  (stderr).

Implementation: the library installs one process-global `tracing`
subscriber (once, via `lev_init`, see §13) that forwards records to the
current level filter and callback. Threading rule for the callback: it may
fire on any internal worker thread, must not call back into `lev_*`, and
must be cheap and non-blocking. This is the one sanctioned upcall and its
constraints are explicit, unlike the rejected event-callback model.

## 13. Process-global state, initialisation, and multiple nodes

Some state is unavoidably process-global, and the first draft did not say
so. Pinned down here:

- `lev_init(void)` performs one-time process setup: installs the `tracing`
  subscriber (§12) and a panic hook compatible with the `catch_unwind` guard
  (§6). It runs the setup through a single `std::sync::Once`, so it is
  idempotent and safe from multiple threads; the first call wins. The lazy
  path taken when another `lev_*` function runs before an explicit `lev_init`
  goes through the same `Once`, so concurrent first calls cannot race on
  subscriber or panic-hook installation. An app that wants logging configured
  before the first node should still call `lev_init` explicitly. This
  restores the global-setup role the old stub's `lns_init` had.
- Clock anchor. `build_sync` calls `init_clock_anchor` (`builder.rs`), which
  writes a process-global `static CLOCK_ANCHOR: OnceLock<Instant>`
  (`interfaces/mod.rs`). It is first-writer-wins: the first `build_sync` sets
  the anchor and every later build is a silent no-op that inherits it. This
  makes multiple `leviculum_t` in one process trivially safe with respect to
  the clock: they all share one fixed monotonic reference set at the first
  build, with no re-anchoring. Documented as a known shared-global that is set
  once for the process lifetime, not a per-node property.
- Restart. `lev_stop` clears the node's runtime and `lev_start` rebuilds a
  fresh one (`driver/mod.rs`), so `start` then `stop` then `start` is the
  intended lifecycle. The design supports restart; phase a adds a test that
  asserts a stop/start cycle works, and if it does not the contract narrows
  to start-once with that recorded.
- No other hidden process-global hooks are installed beyond the subscriber,
  the panic hook, and the clock anchor.

## Implementation phases

Test-first throughout. Each phase ends green on `cargo fmt`, `cargo clippy
--workspace -- -D warnings`, the glibc build (`cargo build-ffi`), the Rust
facade tests, and the C example link/run test for that phase
(`cargo test-ffi`).

- Phase a: facade skeleton, instance, identity, version, error plumbing, the
  `catch_unwind` guard and poison contract, `lev_init`, logging control, and
  a stop/start restart test. C example: create node, print version, generate
  identity, round-trip keys, restart once.
- Phase b: destinations, announce, and the eventfd bridge with the semaphore
  contract and overflow policy. C example: announce, observe an
  `AnnounceReceived` event over the fd from a peer.
- Phase c: path, `lev_connect` with identity resolution, link send and
  receive and close. C example: connect to a peer and exchange link data.
- Phase d: datagram, request and response.
- Phase e: resource transfer.
- Phase f: packaging (pkg-config, SONAME, header install layout).

## The Rust facade

The facade is a new additive module `leviculum_std::api`, re-exported as
needed. It is the projection target of the C constraints: it exposes only
the v1 surface and gives it stable types that hide core internals.

Proposed types:
- `api::Node`, `api::NodeBuilder` (thin wrappers over `ReticulumNode` /
  `ReticulumNodeBuilder`, exposing only v1 methods).
- `api::Identity` (re-export of the core identity, already clean).
- `api::Address` for the 16-byte destination hash and link id, replacing the
  leaked `DestinationHash` / `LinkId` in the facade signatures with one
  stable byte-addressed newtype.
- `api::Event` (projection of `NodeEvent` to the v1 set, internal fields
  dropped).
- `api::Error` (already close to `leviculum_std::error::Error`; the facade
  re-exports it and the C codes map from it per §2).

Type-ownership decision (review S5). The facade re-exports core `Identity`
and `Error` rather than wrapping them in newtypes. This is deliberate for
the 0.x series: both are already clean, and wrapping adds boilerplate for no
gain while the API is pre-1.0. It matters only to Rust facade consumers; the
C ABI never exposes these types, since identities are opaque handles and
errors are `int` codes. Before 1.0 this is revisited, and if core stability
needs decoupling the facade introduces owned newtypes then. Recorded as a
decision, not an accident.

The facade adds no new behaviour and no new wire format. It is curation and
re-typing only, which keeps the additive-only and Python-compatibility
constraints trivially satisfied: everything underneath is unchanged.

## Decisions and remaining open questions

Resolved in this revision:
- Event fd is a level-triggered `EFD_SEMAPHORE` eventfd with the
  counter-equals-queue-length invariant (§4).
- `lev_connect(node, dest_hash)` resolves the signing key internally;
  `lev_connect_with_key` covers out-of-band (§10).
- Blocking calls are bounded by `timeout_ms`; link data is `try_send`-first
  (§5).
- Builder is single-use; the caller frees the empty shell (§1).
- Post-panic, a node is only safe to `lev_free` (§6).
- Logging and `lev_init` own the process-global concerns (§12, §13).
- Request policy and resource strategy become flat enums plus an allowlist
  array (§8).

Still open, to settle against running code:
- Event queue default capacities and whether the data region should drop
  oldest or newest under flood. To be tuned in phase b against a real flood
  test; the control-overflow marker is fixed regardless.
- Whether `lev_connect` should optionally auto-request a path behind a flag
  rather than returning `LEV_ERR_NO_PATH`. Default stays explicit; revisit if
  the phase c C example proves it too sharp.
