# C API: Reference

Every public function and constant of `leviculum.h`, grouped by area. The
header `leviculum-ffi/leviculum.h` is generated from the Rust source and is the
canonical statement of the exact prototypes; this reference is kept in sync
with it and adds semantics. For the model behind these signatures (handles,
the read(2) buffer convention, out-parameters, the event fd, threading), read
the [Overview](overview.md) first.

Conventions used below:

- Functions return `int`: `LEV_OK` (0) on success, a negative `LEV_ERR_*` on
  failure; constructors return `NULL` on failure. After any failure,
  `lev_last_error()` holds a detail string.
- A `(uint8_t *buf, uintptr_t cap, uintptr_t *out_len)` triple is read(2)
  style: `buf == NULL` or too-small `cap` returns `LEV_ERR_BUFFER_TOO_SMALL`
  with `*out_len` set to the required size.
- `timeout_ms` is a deadline in milliseconds; negative means wait forever; on
  expiry the call returns `LEV_ERR_TIMEOUT`.

## Opaque types

| Type | Created by | Freed by | Thread-safety |
| --- | --- | --- | --- |
| `leviculum_t` | `lev_builder_build` | `lev_free` | thread-safe; events single-consumer |
| `lev_builder_t` | `lev_builder_new` | `lev_builder_free` | one thread |
| `lev_identity_t` | `lev_identity_generate`, `lev_identity_from_private_key`, `lev_identity_from_public_key`, `lev_identity_load_file`, `lev_link_remote_identity` | `lev_identity_free` | one thread |
| `lev_destination_t` | `lev_destination_new` | `lev_destination_free` | one thread |
| `lev_link_t` | `lev_connect`, `lev_connect_with_key`, `lev_accept_link` | `lev_link_free` | sends thread-safe; do not close/free concurrently with other calls on the same link |
| `lev_event_t` | `lev_next_event`, `lev_wait_event` | `lev_event_free` | one thread |
| `lev_path_table_t` | `lev_path_table_snapshot` | `lev_path_table_free` | one thread |
| `lev_interface_stats_t` | `lev_interface_stats_snapshot` | `lev_interface_stats_free` | one thread |

## Initialisation and logging

```c
int lev_init(void);
int lev_log_set_level(int level);
int lev_log_set_callback(lev_log_callback cb, void *user);
typedef void (*lev_log_callback)(int level, const char *message, void *user);
```

- `lev_init` runs one-time process setup (logging subscriber, panic hook) once,
  through an internal `Once`. Idempotent and thread-safe. Optional: other
  entry points run it lazily.
- `lev_log_set_level` sets the global verbosity to one of the `LEV_LOG_*`
  constants. Returns `LEV_ERR_INVALID_ARG` for an out-of-range level.
- `lev_log_set_callback` routes log records to `cb` (with `user` passed back
  unchanged), or restores the stderr default when `cb` is `NULL`. The callback
  may run on any internal worker thread, receives a NUL-terminated message
  valid only for the call, and must not call back into any `lev_*` function.

| Constant | Value | Meaning |
| --- | --- | --- |
| `LEV_LOG_OFF` | 0 | no logging (default) |
| `LEV_LOG_ERROR` | 1 | errors only |
| `LEV_LOG_WARN` | 2 | warnings and above |
| `LEV_LOG_INFO` | 3 | info and above |
| `LEV_LOG_DEBUG` | 4 | debug and above |
| `LEV_LOG_TRACE` | 5 | everything |

## Versioning

```c
const char *lev_version_string(void);
uint32_t    lev_version_number(void);
```

- `lev_version_string` returns the workspace version from `Cargo.toml` as a
  static, never-freed string.
- `lev_version_number` packs it as `(major << 16) | (minor << 8) | patch`, a
  host-byte-order integer for in-process comparison only.

## Errors

```c
const char *lev_strerror(int code);
const char *lev_last_error(void);
```

- `lev_strerror` returns a static message for a `LEV_ERR_*` code; safe any
  time, never freed.
- `lev_last_error` returns the thread-local detail string for the most recent
  failing call on the calling thread, or `NULL` if there is none. Owned by the
  library, valid until the next failing call on the same thread, never freed.

### Error codes

| Constant | Value | Meaning |
| --- | --- | --- |
| `LEV_OK` | 0 | success |
| `LEV_ERR_NULL_PTR` | -1 | a required pointer argument was NULL |
| `LEV_ERR_INVALID_ARG` | -2 | malformed argument (bad length, unparseable string) |
| `LEV_ERR_BUFFER_TOO_SMALL` | -3 | caller buffer too small; `*out_len` holds the needed size |
| `LEV_ERR_NOT_RUNNING` | -4 | the node event loop is not running |
| `LEV_ERR_IO` | -5 | an I/O or storage error |
| `LEV_ERR_CONFIG` | -6 | a configuration error |
| `LEV_ERR_CRYPTO` | -7 | a cryptographic operation failed |
| `LEV_ERR_NO_PATH` | -8 | no path to the destination is known |
| `LEV_ERR_LINK` | -9 | a link operation failed (closed, inactive, handshake) |
| `LEV_ERR_SEND` | -10 | a send failed (no route, payload too large) |
| `LEV_ERR_RESOURCE` | -11 | a resource transfer operation failed |
| `LEV_ERR_REQUEST` | -12 | a request or response operation failed |
| `LEV_ERR_TIMEOUT` | -13 | the operation timed out |
| `LEV_ERR_AGAIN` | -14 | non-fatal backpressure; retry later |
| `LEV_ERR_UNKNOWN_DEST` | -15 | no cached identity for the destination |
| `LEV_ERR_PANIC` | -127 | a panic was caught at the FFI boundary |

## Identity

```c
struct lev_identity_t *lev_identity_generate(void);
struct lev_identity_t *lev_identity_from_private_key(const uint8_t *key, uintptr_t len);
struct lev_identity_t *lev_identity_from_public_key(const uint8_t *key, uintptr_t len);
struct lev_identity_t *lev_identity_load_file(const char *path);
int  lev_identity_save_file(const struct lev_identity_t *id, const char *path);
void lev_identity_free(struct lev_identity_t *id);
int  lev_identity_hash(const struct lev_identity_t *id, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_identity_public_key(const struct lev_identity_t *id, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_identity_private_key(const struct lev_identity_t *id, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_identity_has_private_keys(const struct lev_identity_t *id);
int  lev_identity_sign(const struct lev_identity_t *id, const uint8_t *msg, uintptr_t msg_len, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_identity_verify(const struct lev_identity_t *id, const uint8_t *msg, uintptr_t msg_len, const uint8_t *sig, uintptr_t sig_len);
int  lev_identity_encrypt(const struct lev_identity_t *id, const uint8_t *plaintext, uintptr_t plaintext_len, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_identity_decrypt(const struct lev_identity_t *id, const uint8_t *ciphertext, uintptr_t ciphertext_len, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
```

- `lev_identity_generate` makes a new random full identity; `NULL` on failure.
- `lev_identity_from_private_key` / `lev_identity_from_public_key` build an
  identity from a 64-byte combined key (`len` must equal `LEV_IDENTITY_KEY_LEN`);
  the public-key variant yields a public-only identity. `NULL` on failure.
- `lev_identity_load_file` reads the raw 64-byte private key file (the
  Python-Reticulum format); `NULL` if missing, wrong size, or invalid.
- `lev_identity_save_file` writes the private key to `path` atomically;
  `LEV_ERR_CRYPTO` if the identity is public-only.
- `lev_identity_hash` writes the 16-byte identity hash; `_public_key` and
  `_private_key` write the 64-byte combined keys (`_private_key` returns
  `LEV_ERR_CRYPTO` for a public-only identity). All read(2) style.
- `lev_identity_has_private_keys` returns 1 for a full identity, 0 otherwise
  (and 0 on `NULL`).
- `lev_identity_sign` writes the 64-byte Ed25519 signature of `msg` read(2)
  style; `LEV_ERR_CRYPTO` if the identity is public-only. `lev_identity_verify`
  returns 1 if the signature is valid, 0 if not (including a wrong-length
  signature), and a negative `LEV_ERR_*` on a NULL argument; it needs only the
  public key.
- `lev_identity_encrypt` encrypts `plaintext` to the identity's public key
  (the Reticulum X25519+AES scheme) and writes the ciphertext read(2) style;
  encryption is randomised, so a length query and the real call differ in bytes
  but not length. `lev_identity_decrypt` reverses it with the private key and
  returns `LEV_ERR_CRYPTO` for a public-only identity or a ciphertext that
  fails to authenticate.

Every returned `lev_identity_t` is owned by the caller and freed with
`lev_identity_free`.

| Constant | Value | Meaning |
| --- | --- | --- |
| `LEV_ADDR_LEN` | 16 | destination, link, and identity hash length |
| `LEV_IDENTITY_KEY_LEN` | 64 | combined key length (public or private) |
| `LEV_X25519_KEY_LEN` | 32 | encryption half, bytes 0..32 |
| `LEV_SIGNING_KEY_LEN` | 32 | Ed25519 signing half, bytes 32..64 |

## Node lifecycle and builder

```c
struct lev_builder_t *lev_builder_new(void);
void lev_builder_free(struct lev_builder_t *b);
int  lev_builder_identity(struct lev_builder_t *b, const struct lev_identity_t *id);
int  lev_builder_storage_path(struct lev_builder_t *b, const char *path);
int  lev_builder_add_tcp_client(struct lev_builder_t *b, const char *addr);
int  lev_builder_add_tcp_server(struct lev_builder_t *b, const char *addr);
int  lev_builder_add_udp(struct lev_builder_t *b, const char *listen_addr, const char *forward_addr);
int  lev_builder_add_auto_interface(struct lev_builder_t *b);
int  lev_builder_add_rnode(struct lev_builder_t *b, const char *port, uint64_t frequency, uint32_t bandwidth, uint8_t spreading_factor, uint8_t coding_rate, int8_t tx_power);
int  lev_builder_add_serial(struct lev_builder_t *b, const char *port, uint32_t speed, uint8_t databits, const char *parity, uint8_t stopbits);
int  lev_builder_enable_transport(struct lev_builder_t *b, int enabled);
int  lev_builder_event_capacity(struct lev_builder_t *b, uintptr_t control_cap, uintptr_t data_cap);
int  lev_builder_link_keepalive(struct lev_builder_t *b, uint64_t secs);
int  lev_builder_config_file(struct lev_builder_t *b, const char *path);
int  lev_builder_share_instance(struct lev_builder_t *b, const char *name);
int  lev_builder_connect_shared_instance(struct lev_builder_t *b, const char *name);
struct leviculum_t *lev_builder_build(struct lev_builder_t *b);

int  lev_start(struct leviculum_t *node);
int  lev_stop(struct leviculum_t *node);
int  lev_is_running(const struct leviculum_t *node);
int  lev_identity_hash_self(const struct leviculum_t *node, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
void lev_free(struct leviculum_t *node);
```

- `lev_builder_new` allocates a builder; `lev_builder_free` releases it
  (`lev_builder_free(NULL)` is a no-op).
- The setters configure the node: `_identity` pins a (cloned) identity,
  `_storage_path` sets the state directory, the `_add_*` calls add interfaces
  (TCP addresses are `host:port`), `_enable_transport` toggles relay mode, and
  `_event_capacity` sets the event-queue sizes (control and data planes; a 0
  keeps the current default). Each setter returns `LEV_ERR_INVALID_ARG` if the
  builder was already consumed.
- `lev_builder_link_keepalive` overrides the link keepalive interval, in
  seconds, for every link the node creates; the stale-link timeout scales with
  it (a link goes stale after twice the keepalive). The value is clamped to the
  protocol minimum. The default (no call) derives the interval from the link
  RTT. Useful for slow links, and for making `LEV_EVENT_LINK_STALE` observable
  quickly. The same knob is the `keepalive_interval` key in a config file.
- `lev_builder_add_rnode` adds a LoRa interface over an RNode: `port` is the
  serial device, then the required radio settings (`frequency` and `bandwidth`
  in Hz, `spreading_factor`, `coding_rate` denominator, `tx_power` in dBm).
  `lev_builder_add_serial` adds a raw KISS serial interface: `port`, `speed`,
  `databits`, `parity` (`"N"`, `"E"`, or `"O"`), `stopbits`. Both return
  `LEV_ERR_INVALID_ARG` on a NULL device path (or NULL parity). For the
  optional RNode tuning (airtime limits, flow control, buffer size) use a
  config file. The device is opened at `lev_start`, not when the setter runs.
- `lev_builder_config_file` loads an RNS-style INI config (the same format
  `rnsd`/`lnsd` read) from `path`; its `[reticulum]` and `[interfaces]`
  sections add to whatever the builder set programmatically. Loading a config
  brings up every interface type it names, including RNode and Serial, so a C
  node reaches LoRa through a config file.
- `lev_builder_share_instance` makes the node offer a shared instance under
  `name`: it opens a local IPC endpoint and the `rnstatus`/`rnpath`/`rnprobe`
  RPC server, so other local programs (and tools) attach to this one stack.
- `lev_builder_connect_shared_instance` makes the node a client of a shared
  instance named `name` instead of bringing up its own interfaces, the way
  `rncp`/`rnx` attach to a running daemon. A `NULL` path or name returns
  `LEV_ERR_INVALID_ARG`.
- `lev_builder_build` produces a `leviculum_t` and empties the builder; you
  still call `lev_builder_free` on the empty handle. `NULL` on failure.
- `lev_start` spawns the event loop and brings up interfaces; `lev_stop`
  persists state and tears it down; a stopped node can be started again.
  `lev_start` on a running node returns `LEV_ERR_CONFIG`, as does starting a
  node configured to *serve* a shared instance whose name another daemon on
  the host already serves — `lev_last_error` then carries the instance name.
  A node meant to *use* the running daemon should connect as a client
  instead, not serve.
- `lev_is_running` returns 1 while the loop runs (0 on `NULL`).
- `lev_identity_hash_self` writes the node's own 16-byte identity hash.
- `lev_free` stops a running node and releases it (`lev_free(NULL)` is a
  no-op). Call it, and the other blocking calls, from a plain OS thread.

The event-side functions on a node (`lev_event_fd`, `lev_next_event`,
`lev_wait_event`) are documented under [Events](#events).

## Destinations and announce

```c
struct lev_destination_t *lev_destination_new(const struct lev_identity_t *identity,
                                              int direction, int dest_type,
                                              const char *app_name,
                                              const char *const *aspects, uintptr_t n_aspects);
void lev_destination_free(struct lev_destination_t *dest);
int  lev_destination_hash(const struct lev_destination_t *dest, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_destination_enable_ratchets(struct lev_destination_t *dest, uint64_t now_ms);
int  lev_destination_ratchet_public(const struct leviculum_t *node, const uint8_t *dest_hash, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_destination_set_proof_strategy(struct lev_destination_t *dest, int strategy);
int  lev_send_proof(const struct leviculum_t *node, const uint8_t *dest_hash, const uint8_t *packet_hash, int timeout_ms);
int  lev_register_destination(const struct leviculum_t *node, struct lev_destination_t *dest);
int  lev_announce(const struct leviculum_t *node, const uint8_t *dest_hash,
                  const uint8_t *app_data, uintptr_t app_data_len, int timeout_ms);
int  lev_send_datagram(const struct leviculum_t *node, const uint8_t *dest_hash,
                       const uint8_t *data, uintptr_t data_len, uint8_t *out_hash, int timeout_ms);
```

- `lev_destination_new` builds a destination from an identity (may be `NULL`;
  required for some types, forbidden for `LEV_DEST_PLAIN`), a direction, a type,
  an `app_name`, and an array of `n_aspects` NUL-terminated aspect strings.
  `NULL` on failure.
- `lev_destination_hash` writes the 16-byte hash; read it before registering.
  Returns `LEV_ERR_INVALID_ARG` once the destination has been consumed.
- `lev_destination_enable_ratchets` turns on forward secrecy for an inbound
  destination before it is registered; `now_ms` is the current time in
  milliseconds, seeding ratchet rotation. `LEV_ERR_INVALID_ARG` for an outbound
  destination or one already registered. `lev_destination_ratchet_public` reads
  the current 32-byte ratchet public key of a registered destination (read(2)
  style), or `LEV_ERR_INVALID_ARG` if it has no ratchets. Ratcheted
  destinations interoperate with Python peers.
- `lev_destination_set_proof_strategy` sets, before registration, how a
  destination proves delivery of received packets: `LEV_PROOF_NONE` (default,
  never), `LEV_PROOF_APP` (emit `LEV_EVENT_PACKET_PROOF_REQUESTED` so the app
  decides, then calls `lev_send_proof`), or `LEV_PROOF_ALL` (auto-prove every
  packet, Python's PROVE_ALL). `lev_send_proof` sends a delivery proof for the
  `packet_hash` from a proof-requested event; `LEV_ERR_SEND` if no return path
  exists.
- `lev_register_destination` registers the destination on the node so it can be
  announced and accept links and packets. It consumes the destination (the
  handle is emptied; still free it). `LEV_ERR_INVALID_ARG` if already
  registered.
- `lev_announce` broadcasts a registered destination (by 16-byte hash) on all
  interfaces, with optional `app_data`.
- `lev_send_datagram` sends one unreliable packet to a destination and writes
  the 16-byte packet hash into `out_hash`. A path must be known
  (`LEV_ERR_NO_PATH` otherwise).

| Constant | Value | Meaning |
| --- | --- | --- |
| `LEV_DIRECTION_IN` | 0 | incoming: receives announces, links, packets |
| `LEV_DIRECTION_OUT` | 1 | outgoing: a source address for sending |
| `LEV_DEST_SINGLE` | 0 | point-to-point, ephemeral encryption |
| `LEV_DEST_GROUP` | 1 | shared-key broadcast |
| `LEV_DEST_PLAIN` | 2 | unencrypted |

## Paths, connect, and links

```c
int lev_has_path(const struct leviculum_t *node, const uint8_t *dest_hash);
int lev_hops_to(const struct leviculum_t *node, const uint8_t *dest_hash, uint8_t *out);
int lev_request_path(const struct leviculum_t *node, const uint8_t *dest_hash, int timeout_ms);

int lev_connect(const struct leviculum_t *node, const uint8_t *dest_hash,
                int timeout_ms, struct lev_link_t **out);
int lev_connect_with_key(const struct leviculum_t *node, const uint8_t *dest_hash,
                         const uint8_t *signing_key, int timeout_ms, struct lev_link_t **out);
int lev_accept_link(const struct leviculum_t *node, const uint8_t *link_id,
                    int timeout_ms, struct lev_link_t **out);

int  lev_link_send(const struct lev_link_t *link, const uint8_t *data, uintptr_t len, int timeout_ms);
int  lev_link_try_send(const struct lev_link_t *link, const uint8_t *data, uintptr_t len);
int  lev_link_id(const struct lev_link_t *link, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_link_is_closed(const struct lev_link_t *link);
int  lev_link_identify(const struct leviculum_t *node, const uint8_t *link_id,
                       const struct lev_identity_t *identity, int timeout_ms);
struct lev_identity_t *lev_link_remote_identity(const struct leviculum_t *node, const uint8_t *link_id);
int  lev_close_link(struct lev_link_t *link, int timeout_ms);
void lev_link_free(struct lev_link_t *link);
```

- `lev_has_path` returns 1 if a path to the destination is known, else 0
  (negative on a NULL argument). `lev_hops_to` writes the hop count into `*out`
  or returns `LEV_ERR_NO_PATH`. `lev_request_path` asks the network for a path;
  the result arrives as an event and `lev_has_path` then returns 1.
- `lev_connect` opens a link by destination hash, resolving the peer's signing
  key from the identity cached by an announce; `*out` receives the link.
  Returns `LEV_ERR_UNKNOWN_DEST` if no identity is cached and `LEV_ERR_NO_PATH`
  if no path is known (it does not auto-request one).
- `lev_connect_with_key` is the same with an explicit 32-byte Ed25519 signing
  key, for out-of-band peers.
- `lev_accept_link` accepts an incoming link request (16-byte link id from a
  `LEV_EVENT_LINK_REQUEST` event); `*out` receives the link.
- `lev_link_send` sends data, retrying backpressure up to the deadline (then
  `LEV_ERR_TIMEOUT`); `lev_link_try_send` never blocks and returns
  `LEV_ERR_AGAIN` under backpressure. Inbound data arrives as
  `LEV_EVENT_LINK_DATA`.
- `lev_link_id` writes the 16-byte link id. `lev_link_is_closed` returns 1 if
  closed (0 on `NULL`).
- `lev_link_identify` proves an identity to the peer (who sees
  `LEV_EVENT_LINK_IDENTIFIED`); `lev_link_remote_identity` returns the peer's
  identity as a new handle the caller frees, or `NULL` if the peer has not
  identified.
- `lev_close_link` closes gracefully (idempotent); `lev_link_free` releases the
  handle, closing an open link first.

## Request and response

```c
int lev_register_request_handler(const struct leviculum_t *node, const uint8_t *dest_hash,
                                 const char *path, int policy,
                                 const uint8_t *allow_identity_hashes, uintptr_t n_ids);
int lev_send_request(const struct leviculum_t *node, const uint8_t *link_id, const char *path,
                     const uint8_t *data, uintptr_t data_len,
                     int response_timeout_ms, uint8_t *out_request_id);
int lev_send_response(const struct leviculum_t *node, const uint8_t *link_id,
                      const uint8_t *request_id, const uint8_t *data, uintptr_t data_len, int timeout_ms);
```

- `lev_register_request_handler` registers a handler for `path` on a local
  destination. For `LEV_REQUEST_POLICY_ALLOW_LIST`, `allow_identity_hashes` is
  `n_ids * 16` bytes of identity hashes; otherwise pass `NULL, 0`. Registering
  overwrites a previous handler for the same destination and path; there is no
  unregister.
- `lev_send_request` sends a request on an established link to `path` and writes
  the 16-byte request id into `out_request_id`. `data` is the msgpack-encoded
  payload (`NULL, 0` for none); `response_timeout_ms` is the request-response
  deadline. The response (`LEV_EVENT_RESPONSE_RECEIVED`) or a timeout
  (`LEV_EVENT_REQUEST_TIMEOUT`) arrives as an event.
- `lev_send_response` replies to a received request (link id and request id from
  the `LEV_EVENT_REQUEST_RECEIVED` event); `data` must be one valid
  msgpack-encoded value.

| Constant | Value | Meaning |
| --- | --- | --- |
| `LEV_REQUEST_POLICY_ALLOW_NONE` | 0 | drop all requests |
| `LEV_REQUEST_POLICY_ALLOW_ALL` | 1 | allow any identity |
| `LEV_REQUEST_POLICY_ALLOW_LIST` | 2 | allow only listed identity hashes |

## Resource transfer

```c
int lev_send_resource(const struct leviculum_t *node, const uint8_t *link_id,
                      const uint8_t *data, uintptr_t data_len,
                      const uint8_t *metadata, uintptr_t metadata_len,
                      int auto_compress, uint8_t *out_hash, int timeout_ms);
int lev_set_resource_strategy(const struct leviculum_t *node, const uint8_t *link_id, int strategy);
int lev_accept_resource(const struct leviculum_t *node, const uint8_t *link_id, int timeout_ms);
int lev_reject_resource(const struct leviculum_t *node, const uint8_t *link_id, int timeout_ms);
```

- `lev_send_resource` sends bulk data over a link and writes the 32-byte
  resource hash into `out_hash`. `metadata`, if present, must be
  msgpack-encoded; `auto_compress` is 0 or 1. The call blocks only for the
  initial dispatch; progress and completion arrive as events.
- `lev_set_resource_strategy` sets how incoming resources on a link are handled
  (one of the `LEV_RESOURCE_*` constants).
- `lev_accept_resource` / `lev_reject_resource` answer a
  `LEV_EVENT_RESOURCE_ADVERTISED` event under the AcceptApp strategy.

| Constant | Value | Meaning |
| --- | --- | --- |
| `LEV_RESOURCE_ACCEPT_NONE` | 0 | reject all incoming resources |
| `LEV_RESOURCE_ACCEPT_ALL` | 1 | accept all automatically |
| `LEV_RESOURCE_ACCEPT_APP` | 2 | advertise to the app to accept or reject |
| `LEV_RESOURCE_HASH_LEN` | 32 | resource hash length |

## Events

```c
int  lev_event_fd(const struct leviculum_t *node);
int  lev_next_event(struct leviculum_t *node, struct lev_event_t **out);
int  lev_wait_event(struct leviculum_t *node, struct lev_event_t **out, int timeout_ms);
void lev_event_free(struct lev_event_t *ev);

int  lev_event_type(const struct lev_event_t *ev);
int  lev_event_link_id(const struct lev_event_t *ev, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_event_dest_hash(const struct lev_event_t *ev, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_event_request_id(const struct lev_event_t *ev, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_event_resource_hash(const struct lev_event_t *ev, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_event_path(const struct lev_event_t *ev, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_event_data(const struct lev_event_t *ev, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_event_metadata(const struct lev_event_t *ev, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_event_progress(const struct lev_event_t *ev, double *out);
int  lev_event_dropped_count(const struct lev_event_t *ev, uint64_t *out);
int  lev_event_msgtype(const struct lev_event_t *ev, uint16_t *out);
int  lev_event_sequence(const struct lev_event_t *ev, uint16_t *out);
int  lev_event_is_sender(const struct lev_event_t *ev);
int  lev_event_interface_id(const struct lev_event_t *ev, uint64_t *out);
int  lev_event_close_reason(const struct lev_event_t *ev, int *out);
int  lev_event_delivery_error(const struct lev_event_t *ev, int *out);
int  lev_event_transfer_size(const struct lev_event_t *ev, uint64_t *out);
int  lev_event_data_size(const struct lev_event_t *ev, uint64_t *out);
int  lev_event_segment_index(const struct lev_event_t *ev, uint32_t *out);
int  lev_event_total_segments(const struct lev_event_t *ev, uint32_t *out);
```

- `lev_event_fd` returns the readable fd to add to a `poll`/`epoll`/`select`
  loop. The library owns it and closes it in `lev_free`; never close it.
- `lev_next_event` dequeues without blocking: on success `*out` is an event
  handle, or `NULL` when the queue is empty. `lev_wait_event` blocks up to
  `timeout_ms` (negative forever); `*out` is `NULL` if the timeout elapses. It
  wakes promptly when an event arrives (the event fd is the real wake source)
  and otherwise rechecks at most every 250 ms, so an infinite wait still
  returns soon after an event lands. Both are single-consumer for a node. Free
  each event with `lev_event_free`.
- `lev_event_type` returns the event's `LEV_EVENT_*` type (0 on `NULL`).
- The accessors read a field of the event, read(2) style for the byte fields:
  `_link_id`, `_dest_hash`, `_request_id` (16 bytes each), `_resource_hash`
  (32 bytes), `_path` (UTF-8 bytes, not NUL-terminated), `_data` (the primary
  payload, possibly empty), `_metadata` (msgpack bytes). `_progress` writes a
  `double` in `0.0..1.0` for resource-progress events; `_dropped_count` writes
  the count of a `LEV_EVENT_CONTROL_OVERFLOW` event; `_msgtype` and `_sequence`
  write the message type and sequence of a `LEV_EVENT_LINK_MESSAGE` event. An
  accessor that does not apply to the event type returns `LEV_ERR_INVALID_ARG`.
- `lev_event_is_sender` returns 1 on the sender side of a resource event
  (`_PROGRESS`/`_COMPLETED`/`_FAILED`), 0 on the receiver side. A sender's
  `LEV_EVENT_RESOURCE_COMPLETED` (empty data) signals that an outgoing transfer
  finished; a receiver's carries the data. A node that both sends and receives
  resources uses this to tell the two apart. `LEV_EVENT_RESOURCE_STARTED` is not
  in that list because the engine emits it on the receiver only, so 0 there is
  the truth rather than a gap. One non-resource event also sets the flag:
  `LEV_EVENT_LINK_ESTABLISHED` returns 1 for a link this node initiated and 0
  for an inbound one. Everything else returns 0.
- `lev_event_interface_id` writes the node-assigned id of the interface an
  `_ANNOUNCE_RECEIVED`, `_PATH_FOUND` or `_PACKET_RECEIVED` event arrived on.
  It is an **id, not a position**: resolve it by walking
  `lev_interface_stats_snapshot` and comparing `lev_interface_stats_id`. It is
  the same numbering `lev_path_table_entry` reports as `interface_index`.
- `lev_event_close_reason` writes one of the `LEV_CLOSE_*` constants for a
  `LEV_EVENT_LINK_CLOSED` event. The reason decides how to reconnect:
  `LEV_CLOSE_BLACKHOLED` must not be retried at all, `LEV_CLOSE_TIMEOUT` wants
  the path re-resolved first.
- `lev_event_delivery_error` writes one of the `LEV_DELIVERY_*` constants for a
  `LEV_EVENT_DELIVERY_FAILED` event. `_TIMEOUT` and `_LINK_FAILED` mean re-send;
  `_INVALID_PROOF` means the peer answered with a proof that did not verify, so
  a re-send produces the same result and the destination's identity needs
  re-resolving instead.
- `lev_event_transfer_size` and `lev_event_data_size` write the encrypted
  transfer size and the uncompressed payload size of a resource, on
  `LEV_EVENT_RESOURCE_ADVERTISED` (what the accept-or-reject decision turns on)
  and on `LEV_EVENT_RESOURCE_PROGRESS` (the only place an auto-accepting
  receiver sees them, since no advertisement is surfaced under `ACCEPT_ALL`).
- `lev_event_segment_index` and `lev_event_total_segments` write the 1-based
  position of a `LEV_EVENT_RESOURCE_COMPLETED` segment and how many segments the
  transfer has. A multi-segment resource fires one completion per segment under
  the same `resource_hash`, so these are how a receiver orders the chunks and
  recognises the last one (`segment_index == total_segments`); metadata is
  present on segment 1 only.

| Constant | Value | Link close reason |
| --- | --- | --- |
| `LEV_CLOSE_NORMAL` | 0 | closed deliberately; reconnect freely |
| `LEV_CLOSE_TIMEOUT` | 1 | handshake did not complete; re-resolve the path |
| `LEV_CLOSE_INVALID_PROOF` | 2 | a proof did not verify |
| `LEV_CLOSE_PEER_CLOSED` | 3 | the peer closed it |
| `LEV_CLOSE_STALE` | 4 | inactive past the keepalive deadline |
| `LEV_CLOSE_CHANNEL_EXHAUSTED` | 5 | a channel message ran out of retries |
| `LEV_CLOSE_BLACKHOLED` | 6 | peer is blackholed; **do not retry** |
| `LEV_CLOSE_OTHER` | 255 | a reason this ABI version does not name |

| Constant | Value | Delivery failure |
| --- | --- | --- |
| `LEV_DELIVERY_TIMEOUT` | 0 | no proof before the receipt expired; re-send |
| `LEV_DELIVERY_LINK_FAILED` | 1 | the carrying link failed; re-send |
| `LEV_DELIVERY_INVALID_PROOF` | 2 | proof did not verify; re-resolve the identity |
| `LEV_DELIVERY_OTHER` | 255 | a failure this ABI version does not name |

### Event types

| Constant | Value | Fields available |
| --- | --- | --- |
| `LEV_EVENT_OTHER` | 0 | catch-all for events without a typed projection |
| `LEV_EVENT_ANNOUNCE_RECEIVED` | 1 | dest_hash, data (app_data), interface_id |
| `LEV_EVENT_PATH_FOUND` | 2 | dest_hash, interface_id |
| `LEV_EVENT_LINK_REQUEST` | 3 | link_id, dest_hash |
| `LEV_EVENT_LINK_ESTABLISHED` | 4 | link_id, dest_hash, is_sender |
| `LEV_EVENT_LINK_CLOSED` | 5 | link_id, dest_hash, close_reason |
| `LEV_EVENT_LINK_DATA` | 6 | link_id, data |
| `LEV_EVENT_PACKET_RECEIVED` | 7 | dest_hash, data, interface_id |
| `LEV_EVENT_CONTROL_OVERFLOW` | 8 | dropped_count |
| `LEV_EVENT_REQUEST_RECEIVED` | 9 | link_id, dest_hash, request_id, path, data |
| `LEV_EVENT_RESPONSE_RECEIVED` | 10 | link_id, request_id, data, metadata |
| `LEV_EVENT_REQUEST_TIMEOUT` | 11 | link_id, request_id |
| `LEV_EVENT_RESOURCE_ADVERTISED` | 12 | link_id, resource_hash, transfer_size, data_size |
| `LEV_EVENT_RESOURCE_STARTED` | 13 | link_id, resource_hash |
| `LEV_EVENT_RESOURCE_PROGRESS` | 14 | link_id, resource_hash, progress, transfer_size, data_size, is_sender |
| `LEV_EVENT_RESOURCE_COMPLETED` | 15 | link_id, resource_hash, data, metadata, segment_index, total_segments, is_sender |
| `LEV_EVENT_RESOURCE_FAILED` | 16 | link_id, resource_hash, is_sender |
| `LEV_EVENT_LINK_IDENTIFIED` | 17 | link_id, data (16-byte identity hash) |
| `LEV_EVENT_LINK_MESSAGE` | 18 | link_id, data, msgtype, sequence (reliable channel) |
| `LEV_EVENT_PACKET_PROOF_REQUESTED` | 19 | dest_hash, data (32-byte packet hash), interface_id |
| `LEV_EVENT_LINK_PROOF_REQUESTED` | 20 | link_id, data (32-byte packet hash) |
| `LEV_EVENT_LINK_DELIVERY_CONFIRMED` | 21 | link_id, data (32-byte packet hash) |
| `LEV_EVENT_LINK_STALE` | 22 | link_id (link inactive past keepalive) |
| `LEV_EVENT_LINK_RECOVERED` | 23 | link_id (stale link resumed) |
| `LEV_EVENT_PATH_LOST` | 24 | dest_hash (path expired) |
| `LEV_EVENT_PACKET_DELIVERY_CONFIRMED` | 25 | data (16-byte packet hash) |
| `LEV_EVENT_DELIVERY_FAILED` | 26 | data (16-byte packet hash), delivery_error |
| `LEV_EVENT_LINK_DELIVERY_FAILED` | 27 | link_id, data (32-byte packet hash) |

## Diagnostics

```c
int lev_transport_stats(const struct leviculum_t *node,
                        uint64_t *out_packets_sent, uint64_t *out_packets_received,
                        uint64_t *out_packets_forwarded, uint64_t *out_announces_processed,
                        uint64_t *out_packets_dropped, uint64_t *out_path_count);

struct lev_path_table_t *lev_path_table_snapshot(const struct leviculum_t *node);
int  lev_path_table_count(const struct lev_path_table_t *table);
int  lev_path_table_entry(const struct lev_path_table_t *table, uintptr_t index,
                          uint8_t *dest_hash, uint8_t *hops, uint8_t *next_hop,
                          int *has_next_hop, uint64_t *interface_index, uint64_t *expires_ms);
void lev_path_table_free(struct lev_path_table_t *table);

struct lev_interface_stats_t *lev_interface_stats_snapshot(const struct leviculum_t *node);
int  lev_interface_stats_count(const struct lev_interface_stats_t *table);
int  lev_interface_stats_name(const struct lev_interface_stats_t *table, uintptr_t index,
                              uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int  lev_interface_stats_entry(const struct lev_interface_stats_t *table, uintptr_t index,
                               int *online, int *is_local_client,
                               uint64_t *rx_bytes, uint64_t *tx_bytes);
int  lev_interface_stats_id(const struct lev_interface_stats_t *table, uintptr_t index,
                            uint64_t *out_id);
void lev_interface_stats_free(struct lev_interface_stats_t *table);

int  lev_tcp_listen_addr(const struct leviculum_t *node, uintptr_t index,
                         uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
```

- `lev_transport_stats` reads the node's transport counters and the current
  path-table size into the out-parameters, the basis for an `rnstatus`-style
  view. Any out-pointer may be `NULL` to skip that counter; a `NULL` node
  returns `LEV_ERR_NULL_PTR`. The values are a point-in-time snapshot.
- `lev_path_table_snapshot` returns an owned, frozen copy of the path table for
  an `rnpath`-style view, or `NULL` on a `NULL` node; free it with
  `lev_path_table_free` (`NULL` is a no-op). Because it is frozen, reads never
  race a changing table. `lev_path_table_count` gives the number of entries.
  `lev_path_table_entry` reads one entry by index into the out-parameters:
  `dest_hash` and `next_hop` (each at least `LEV_ADDR_LEN` bytes when non-NULL),
  `hops`, `has_next_hop` (1 for a relayed path, 0 for a direct one),
  `interface_index`, and `expires_ms`. Any out-pointer may be `NULL`;
  `LEV_ERR_INVALID_ARG` if `index` is out of range.
- `lev_interface_stats_snapshot` returns an owned, frozen copy of the interface
  list for an `rnstatus`-style interface view, freed with
  `lev_interface_stats_free`. `lev_interface_stats_count` gives the number of
  interfaces. `lev_interface_stats_name` reads the interface name read(2) style
  (variable length), and `lev_interface_stats_entry` reads the scalar fields
  (`online`, `is_local_client`, `rx_bytes`, `tx_bytes`) into out-parameters.
  Both return `LEV_ERR_INVALID_ARG` for an out-of-range index.
- `lev_interface_stats_id` reads the node-assigned id of the interface at a
  position. The position is not the identity: the node numbers interfaces as
  they are registered and never renumbers, so a removed interface leaves a gap.
  This is the accessor that resolves the ids the rest of the API hands out —
  `lev_path_table_entry`'s `interface_index` and `lev_event_interface_id` — to an
  entry whose name and counters can be read.
- `lev_tcp_listen_addr` writes the bound `ip:port` of the node's TCP server
  listener number `index` (in start order), read(2) style. A server added
  with port 0 reports the kernel-assigned port here after `lev_start` — bind
  `:0` and read the port back instead of probing a free port up front and
  racing other processes for the re-bind. `LEV_ERR_INVALID_ARG` if `index`
  is out of range, including before start.

## Helpers

```c
int lev_hex_encode(const uint8_t *data, uintptr_t len, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
int lev_hex_decode(const uint8_t *hex, uintptr_t hex_len, uint8_t *buf, uintptr_t cap, uintptr_t *out_len);
```

- `lev_hex_encode` writes `2 * len` lowercase hex bytes (not NUL-terminated),
  read(2) style.
- `lev_hex_decode` writes `hex_len / 2` bytes; `LEV_ERR_INVALID_ARG` on an odd
  length or a non-hex digit.
