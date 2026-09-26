# C API: Overview and Concepts

Leviculum ships a C API so an application can use the Reticulum network stack
the way it uses any normal Unix C library: a clean header, opaque handle
types, integer error codes, and composition with the application's own event
loop. This chapter explains the model that the [How-To](howto.md) and the
[API Reference](reference.md) build on. For the design rationale behind these
choices, see the design-of-record at `docs/leviculum-api-design.md`.

Every symbol is prefixed `lev_` (functions) or `LEV_` (constants). The header
is `leviculum.h`, the library is `libleviculum.so`.

## Installing and linking

Once the development package is installed, building against Leviculum is the
usual two lines:

```c
#include <leviculum.h>
```

```sh
cc app.c $(pkg-config --cflags --libs leviculum)
```

The `pkg-config` call expands to `-lleviculum` plus the include and library
paths. To build from source and install the header, the shared object (with its
SONAME and dev symlinks), the static archive, and the pkg-config file:

```sh
make -C leviculum-ffi install PREFIX=/usr/local   # builds, then installs
```

To link Leviculum statically while glibc stays dynamic, pass `--static` so
pkg-config adds the archive's system dependencies, and force the archive:

```sh
cc app.c $(pkg-config --cflags leviculum) \
    -l:libleviculum.a $(pkg-config --static --libs-only-l leviculum | sed 's/-lleviculum//')
```

See [Installation](../guide/installation.md) for the full toolchain setup. The
install is verified end to end (dynamic and static, x86_64 and aarch64) by
`scripts/verify-packaging.sh`.

## Opaque handles

Every complex object is an opaque pointer. The application never sees a struct
layout, so the ABI stays stable across versions. Each handle has a constructor
and a matching free function; `_free(NULL)` is always a no-op.

| Handle | Represents | Created by | Freed by |
| --- | --- | --- | --- |
| `leviculum_t` | a node (runtime, engine, event bridge) | `lev_builder_build` | `lev_free` |
| `lev_builder_t` | node configuration before build | `lev_builder_new` | `lev_builder_free` |
| `lev_identity_t` | a key pair or public-only identity | `lev_identity_generate`, `lev_identity_from_*`, `lev_identity_load_file`, `lev_link_remote_identity` | `lev_identity_free` |
| `lev_destination_t` | a local destination | `lev_destination_new` | `lev_destination_free` |
| `lev_link_t` | one link to a peer | `lev_connect`, `lev_connect_with_key`, `lev_accept_link` | `lev_link_free` |
| `lev_event_t` | one drained event | `lev_next_event`, `lev_wait_event` | `lev_event_free` |

Two builders are single-use: `lev_builder_build` and `lev_register_destination`
take the contents of their handle and leave an empty shell that the caller
still frees.

Addresses are not handles. A destination hash, a link id, and an identity hash
are each a fixed 16-byte value (`LEV_ADDR_LEN`); a resource hash is 32 bytes
(`LEV_RESOURCE_HASH_LEN`). They cross the boundary as plain `uint8_t` arrays.

## Error handling

Functions that can fail return `int`: `0` (`LEV_OK`) on success, a negative
`LEV_ERR_*` code on failure. Constructors that return a handle return `NULL`
on failure. Two helpers turn a code into text:

- `lev_strerror(code)` returns a static, never-freed string for the code.
- `lev_last_error()` returns a thread-local string with the specific detail of
  the most recent failing call on the calling thread (which argument, which
  address). It is owned by the library and must not be freed.

```c
int rc = lev_start(node);
if (rc != LEV_OK) {
    fprintf(stderr, "start failed: %s (%s)\n", lev_strerror(rc), lev_last_error());
}
```

The full code list is in the [reference](reference.md#error-codes).

## Buffers: the read(2) convention

Every function that returns bytes into a caller buffer uses the same shape,
modelled on `read(2)`:

```c
int lev_identity_hash(const lev_identity_t *id,
                      uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
```

- The caller owns `buf` and passes its capacity `cap` plus an `out_len`.
- On success the library writes the bytes and sets `*out_len` to the count.
- If `cap` is too small (or `buf` is `NULL`), nothing is written, `*out_len` is
  set to the required size, and the call returns `LEV_ERR_BUFFER_TOO_SMALL`.
  Passing `buf == NULL` is therefore a valid size query.

```c
uint8_t hash[LEV_ADDR_LEN];
uintptr_t len = sizeof(hash);
if (lev_identity_hash(id, hash, sizeof(hash), &len) == LEV_OK) {
    /* `hash` holds `len` bytes */
}
```

The library never hands C a raw pointer to free: all freeing goes through a
typed `lev_*_free`, which removes C-`free`-versus-Rust-`dealloc` mistakes.

## Out-parameters for returned values

Status and value are never multiplexed into one return. A call that both can
fail and produces a value returns the `int` status and writes the value
through an out-parameter:

```c
uint8_t packet_hash[LEV_ADDR_LEN];
int rc = lev_send_datagram(node, dest, data, len, packet_hash, 3000);

lev_link_t *link = NULL;
int rc2 = lev_connect(node, dest, 5000, &link);   /* link in *out */
```

## Strings and bytes

- Opaque byte payloads (keys, hashes, datagram data, link data, resource data)
  are always a pointer plus a length, never NUL-terminated, and may contain
  zero bytes.
- Human-readable strings the library consumes (storage path, destination
  `app_name`, request `path`) are NUL-terminated UTF-8 C strings.
- A destination's aspects are passed as a `const char *const *` array plus a
  count.
- Library-returned static strings (`lev_strerror`, `lev_last_error`,
  `lev_version_string`) are NUL-terminated and must not be freed.

## The event model: a pollable fd

Everything inbound (received announces, link data, request and response
arrivals, resource progress and completion) reaches the application as events.
A node exposes a single readable file descriptor that the application adds to
its own `poll`/`epoll`/`select` loop:

```c
struct pollfd p = { .fd = lev_event_fd(node), .events = POLLIN };
poll(&p, 1, -1);

lev_event_t *ev;
while (lev_next_event(node, &ev) == LEV_OK && ev) {
    switch (lev_event_type(ev)) {
        case LEV_EVENT_ANNOUNCE_RECEIVED: /* ... */ break;
        case LEV_EVENT_LINK_DATA:         /* ... */ break;
    }
    lev_event_free(ev);
}
```

The fd is level-triggered: it is readable exactly while the queue is
non-empty. After each wake, drain with `lev_next_event` until it yields `NULL`.
`lev_wait_event(node, &ev, timeout_ms)` is a convenience that blocks for the
next event without your own loop. The event side is single-consumer: do not
call the two drain functions concurrently for the same node.

The fd is owned by the library and closed by `lev_free`. The shutdown order is
mandatory: stop reacting to the fd, remove it from your loop, then call
`lev_free`. Polling the fd after `lev_free` is a use-after-close.

Event handles are fully self-owned (payloads are copied out at dequeue), so an
event stays valid until `lev_event_free` regardless of later calls. Read its
fields with the typed accessors (`lev_event_link_id`, `lev_event_data`,
`lev_event_request_id`, `lev_event_resource_hash`, and so on); an accessor that
does not apply to the event type returns `LEV_ERR_INVALID_ARG`.

## Threading and blocking

The tokio runtime is created and owned inside the node and never exposed.

- A `leviculum_t` is thread-safe: its methods may be called concurrently from
  multiple threads.
- The event side is single-consumer (above).
- Every potentially-blocking call takes a `timeout_ms` (negative means wait
  forever); on expiry it returns `LEV_ERR_TIMEOUT`. The link data path is
  `try_send`-first: `lev_link_try_send` never blocks and returns
  `LEV_ERR_AGAIN` under backpressure, while `lev_link_send` retries up to its
  deadline.
- `lev_free`, `lev_stop`, and the other blocking calls must run on a plain OS
  thread, never on a worker thread of another runtime (for example a host
  async runtime); doing so would panic the embedded `block_on`.
- The log callback may fire on any internal worker thread and must not call
  back into any `lev_*` function.

## No panic crosses the boundary

Every exported function wraps its body so that an internal Rust panic is caught
and converted to `LEV_ERR_PANIC` (or `NULL` for a constructor) instead of
unwinding into C, which would be undefined behaviour. After a caught panic the
affected node should be freed and not reused.

## One-time setup and logging

`lev_init()` performs idempotent process setup (logging subscriber and panic
hook). It is optional, since other entry points run it lazily, but call it
explicitly to configure logging before the first node. Logging is silent by
default; raise it with `lev_log_set_level(LEV_LOG_INFO)` and route records with
`lev_log_set_callback`, or leave the default which writes to stderr.

With these conventions in hand, the [Tutorial](tutorial.md) builds a complete,
useful program (`levcat`, a pipe over the mesh) step by step, the
[How-To](howto.md) is the recipe book for every flow, and the
[API Reference](reference.md) documents every function.
