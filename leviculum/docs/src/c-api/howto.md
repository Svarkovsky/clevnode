# C API: How-To, Building Applications

This chapter shows how the functions combine into working programs. It assumes
the model from the [Overview](overview.md): opaque handles, integer error
codes, read(2) buffers, and the pollable event fd. Each recipe gives the
functions involved and a focused snippet; the complete, compiling programs are
the acceptance tests under `leviculum-ffi/examples/c/`, named per recipe. For a
single program built end to end from these pieces, see the
[Tutorial](tutorial.md).

Error checks are abbreviated in the snippets for readability. In real code,
check every `int` return against `LEV_OK` and report `lev_last_error()` (see
[Errors and logging](#errors-and-logging)).

## A minimal node

Build a node, attach an interface, start it, and shut it down. The builder is
single-use: `lev_builder_build` consumes its configuration and you still free
the empty handle.

```c
#include <leviculum.h>
#include <stdio.h>

int main(void) {
    lev_init();
    printf("leviculum %s\n", lev_version_string());

    lev_builder_t *b = lev_builder_new();
    lev_builder_storage_path(b, "/var/lib/myapp/reticulum");
    lev_builder_add_tcp_client(b, "127.0.0.1:4242");   /* a Reticulum hub */

    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);                               /* build emptied it */
    if (!node) {
        fprintf(stderr, "build failed: %s\n", lev_last_error());
        return 1;
    }

    if (lev_start(node) != LEV_OK) {
        fprintf(stderr, "start failed: %s\n", lev_last_error());
        lev_free(node);
        return 1;
    }

    /* ... run the application ... */

    lev_stop(node);
    lev_free(node);     /* lev_free also stops a still-running node */
    return 0;
}
```

Interfaces are added on the builder: `lev_builder_add_tcp_client`,
`lev_builder_add_tcp_server`, `lev_builder_add_udp`,
`lev_builder_add_auto_interface`. Use `lev_builder_identity` to pin a specific
identity (otherwise one is generated), and `lev_builder_enable_transport(b, 1)`
to act as a relay.

Full program: `leviculum-ffi/examples/c/phase_a.c`.

## Running the event loop

Everything inbound arrives as events. Add `lev_event_fd(node)` to your loop,
and on each wake drain with `lev_next_event` until it yields `NULL`.

```c
#include <poll.h>

int fd = lev_event_fd(node);
for (;;) {
    struct pollfd p = { .fd = fd, .events = POLLIN };
    poll(&p, 1, -1);

    lev_event_t *ev;
    while (lev_next_event(node, &ev) == LEV_OK && ev) {
        switch (lev_event_type(ev)) {
            case LEV_EVENT_ANNOUNCE_RECEIVED: on_announce(ev); break;
            case LEV_EVENT_LINK_REQUEST:      on_link_request(ev); break;
            case LEV_EVENT_LINK_MESSAGE:      on_link_message(ev); break;
            /* ... */
        }
        lev_event_free(ev);
    }
}
```

If you do not want to own a loop, block for one event at a time:

```c
lev_event_t *ev = NULL;
if (lev_wait_event(node, &ev, 1000) == LEV_OK && ev) {   /* up to 1s */
    /* handle ev */
    lev_event_free(ev);
}
```

Rules: the fd is level-triggered (readable while the queue is non-empty); the
two drain functions are single-consumer (one thread at a time); and the
shutdown order is stop reacting to the fd, then `lev_free`. Reading an event's
fields uses the typed accessors shown in the recipes below.

## Running as or with a daemon

A node need not bring up its own interfaces in code. Three builder calls cover
the daemon use cases.

Load an RNS-style config (the same INI `rnsd`/`lnsd` read), so interfaces,
transport, and the shared instance come from a file an operator edits. This is
also how a C node reaches LoRa without programmatic radio setup, the config
names an `RNodeInterface` or `SerialInterface` and the stack brings it up.

```c
lev_builder_t *b = lev_builder_new();
lev_builder_config_file(b, "/etc/leviculum/config");
leviculum_t *node = lev_builder_build(b);
lev_builder_free(b);
lev_start(node);   /* now a daemon: run the event loop until signalled */
```

Offer a shared instance, so other local programs and the Reticulum tools
(`rnstatus`, `rnpath`, `rnprobe`) attach to this one stack instead of each
opening the radio:

```c
lev_builder_share_instance(b, "leviculum");   /* opens the IPC + RPC endpoint */
```

Or attach to a running daemon as a client, the way `rncp`/`rnx` do, instead of
bringing up interfaces of your own:

```c
lev_builder_connect_shared_instance(b, "leviculum");
```

A `NULL` path or name returns `LEV_ERR_INVALID_ARG`. The `daemon.c` example is
a worked acceptance program for all three calls. The `lncp.c` file-copy tool
has both styles: its `recv`/`send` modes bring up their own interface, while its
`recv-shared`/`send-shared` modes attach to a running `lnsd` by instance name,
so several tools share one daemon's radio.

## Radio interfaces (LoRa and serial)

For off-grid mesh, add an RNode (LoRa) or a raw serial interface
programmatically, no config file needed:

```c
lev_builder_t *b = lev_builder_new();
/* RNode: device, frequency Hz, bandwidth Hz, spreading factor, coding rate,
 * tx power dBm. */
lev_builder_add_rnode(b, "/dev/ttyUSB0", 867200000, 125000, 8, 5, 0);
/* Serial: device, speed, data bits, parity ("N"/"E"/"O"), stop bits. */
lev_builder_add_serial(b, "/dev/ttyACM0", 115200, 8, "N", 1);
```

The device is opened at `lev_start`, so a wrong path surfaces there, not at the
setter (which only rejects a NULL path with `LEV_ERR_INVALID_ARG`). A serial
port is raw KISS with no handshake; an RNode performs the RNode detect and
config handshake on start. For the optional RNode knobs (airtime limits, flow
control, buffer size), load a config file instead. The `radio.c` example brings
a node up over a serial interface.

## Identities

An identity is a key pair. Generate one, persist it, and reload it next run.
The on-disk format is the raw 64-byte private key, compatible with Python
Reticulum.

```c
lev_identity_t *id;
id = lev_identity_load_file("/var/lib/myapp/identity");
if (!id) {                                   /* first run: make one */
    id = lev_identity_generate();
    lev_identity_save_file(id, "/var/lib/myapp/identity");
}

uint8_t hash[LEV_ADDR_LEN];
uintptr_t len = sizeof(hash);
lev_identity_hash(id, hash, sizeof(hash), &len);   /* the 16-byte address */
```

A combined key is 64 bytes (`LEV_IDENTITY_KEY_LEN`): the X25519 encryption key
in bytes `0..32` and the Ed25519 signing key in bytes `32..64`. Applications
rarely split it by hand, because `lev_connect` resolves the signing key for
you (see below). Use `lev_builder_identity(b, id)` to give a node a fixed
identity, and `lev_identity_free(id)` when done.

An identity also signs, verifies, encrypts, and decrypts directly, for crypto
tooling and signed application data, interoperable with Python peers (Ed25519
for signatures, X25519+AES for encryption):

```c
uint8_t sig[64];
uintptr_t n = sizeof(sig);
lev_identity_sign(id, msg, msg_len, sig, sizeof(sig), &n);
int ok = lev_identity_verify(id, msg, msg_len, sig, n);   /* 1 valid, 0 not */

/* Encrypt to a peer's public-only identity; only its private key recovers it. */
uint8_t ct[512];
uintptr_t ctl = sizeof(ct);
lev_identity_encrypt(peer, msg, msg_len, ct, sizeof(ct), &ctl);
```

Sign, encrypt, and decrypt write read(2) style (a NULL buffer queries the
length); signing and decryption need the private key and return
`LEV_ERR_CRYPTO` on a public-only identity, while verify needs only the public
key.

Full programs: `leviculum-ffi/examples/c/phase_a.c` and `crypto.c`.

## Announcing and discovering

To be reachable, a node registers an incoming destination and announces it.
Other nodes learn the destination (its address, identity, and a path) from the
announce, which arrives as `LEV_EVENT_ANNOUNCE_RECEIVED`.

Announcing side:

```c
const char *aspects[] = { "inbox" };
lev_destination_t *dest = lev_destination_new(
    id, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "myapp", aspects, 1);

uint8_t dh[LEV_ADDR_LEN];
uintptr_t dhl = sizeof(dh);
lev_destination_hash(dest, dh, sizeof(dh), &dhl);   /* read before registering */

lev_register_destination(node, dest);   /* consumes dest */
lev_destination_free(dest);             /* free the empty shell */

lev_announce(node, dh, NULL, 0, 2000);  /* optional app_data, here none */
```

For forward secrecy, call `lev_destination_enable_ratchets(dest, now_ms)` on an
inbound destination before registering it (`now_ms` is the current time in
milliseconds); peers, including Python ones, then encrypt to a rotating ratchet
key. `lev_destination_ratchet_public(node, dh, ...)` reads the current key. See
`leviculum-ffi/examples/c/ratchet.c`.

For delivery proofs, call `lev_destination_set_proof_strategy(dest, strategy)`
before registering. `LEV_PROOF_ALL` auto-proves every received packet (Python's
PROVE_ALL). `LEV_PROOF_APP` raises a `LEV_EVENT_PACKET_PROOF_REQUESTED` event
whose data is the 32-byte packet hash; the app decides and calls
`lev_send_proof(node, dest_hash, packet_hash, timeout_ms)`. See
`leviculum-ffi/examples/c/proof.c`.

Receiving side, in the event loop:

```c
case LEV_EVENT_ANNOUNCE_RECEIVED: {
    uint8_t peer[LEV_ADDR_LEN];
    uintptr_t n = sizeof(peer);
    lev_event_dest_hash(ev, peer, sizeof(peer), &n);   /* who announced */
    /* optional payload via lev_event_data(ev, ...) */
    break;
}
```

After processing the announce, the receiver has a path and the announcer's
cached identity, so `lev_has_path(node, peer)` returns 1 and `lev_connect` will
work.

Full program: `leviculum-ffi/examples/c/phase_b.c`.

## Links and exchanging data

A link is an encrypted session to a destination. `lev_connect` resolves the
peer's signing key from the identity cached by an announce, so you pass only
the destination hash:

```c
lev_link_t *link = NULL;
int rc = lev_connect(node, peer, 5000, &link);
if (rc == LEV_ERR_UNKNOWN_DEST) { /* no announce seen yet */ }
else if (rc == LEV_ERR_NO_PATH) { lev_request_path(node, peer, 3000); }
else if (rc == LEV_OK) { /* link is pending; wait for established */ }
```

The connecting node watches for `LEV_EVENT_LINK_ESTABLISHED`; the destination
node watches for `LEV_EVENT_LINK_REQUEST` and accepts it:

```c
case LEV_EVENT_LINK_REQUEST: {
    uint8_t lid[LEV_ADDR_LEN];
    uintptr_t n = sizeof(lid);
    lev_event_link_id(ev, lid, sizeof(lid), &n);
    lev_link_t *accepted = NULL;
    lev_accept_link(node, lid, 5000, &accepted);
    /* keep `accepted` to send on this link */
    break;
}
```

Send and receive link data. `lev_link_send` blocks up to its deadline,
retrying backpressure; `lev_link_try_send` returns `LEV_ERR_AGAIN` instead of
blocking. It sends over the link's reliable channel (sequenced and
retransmitted, the same `RawBytesMessage` Python peers use), so the peer sees a
`LEV_EVENT_LINK_MESSAGE`, with a message type and a sequence number:

```c
lev_link_send(link, (const uint8_t *)"hello", 5, 5000);

case LEV_EVENT_LINK_MESSAGE: {
    uint8_t buf[512];
    uintptr_t n = sizeof(buf);
    uint16_t msgtype = 0, sequence = 0;
    if (lev_event_data(ev, buf, sizeof(buf), &n) == LEV_OK) {
        lev_event_msgtype(ev, &msgtype);   /* 0 for raw bytes */
        lev_event_sequence(ev, &sequence); /* per-channel send order */
        /* `n` bytes received */
    }
    break;
}
```

A peer that sends a raw, unsequenced link packet instead of using the channel
(for example Python's `RNS.Packet(link, data).send()`) arrives as the
lower-level `LEV_EVENT_LINK_DATA`, which carries only `link_id` and `data`.

Close with `lev_close_link(link, 2000)` and release with `lev_link_free(link)`
(which also closes an open link). A `LEV_EVENT_LINK_CLOSED` event reports a
link that drops for any reason.

Full program: `leviculum-ffi/examples/c/phase_c.c`.

## Proving identity on a link

By default a link is anonymous. Either side can prove an identity to the peer;
the peer is notified with `LEV_EVENT_LINK_IDENTIFIED` and can read it back.

```c
/* prover */
lev_link_identify(node, my_link_id, my_identity, 3000);

/* peer, in the event loop */
case LEV_EVENT_LINK_IDENTIFIED: {
    lev_identity_t *who = lev_link_remote_identity(node, my_link_id);
    if (who) {
        uint8_t h[LEV_ADDR_LEN];
        uintptr_t n = sizeof(h);
        lev_identity_hash(who, h, sizeof(h), &n);   /* the peer's address */
        lev_identity_free(who);
    }
    break;
}
```

The 16-byte identity hash is also the payload of the
`LEV_EVENT_LINK_IDENTIFIED` event (`lev_event_data`).

Full program: `leviculum-ffi/examples/c/phase_c.c`.

## Request and response

For a request/response service, the responder registers a handler for a path
on its destination; the requester sends a request over a link. Request and
response payloads are msgpack-encoded values.

Responder:

```c
lev_register_request_handler(node, dh, "/echo",
                             LEV_REQUEST_POLICY_ALLOW_ALL, NULL, 0);

case LEV_EVENT_REQUEST_RECEIVED: {
    uint8_t link_id[LEV_ADDR_LEN], req_id[LEV_ADDR_LEN], data[512];
    uintptr_t a = sizeof(link_id), b = sizeof(req_id), c = sizeof(data);
    lev_event_link_id(ev, link_id, sizeof(link_id), &a);
    lev_event_request_id(ev, req_id, sizeof(req_id), &b);
    lev_event_data(ev, data, sizeof(data), &c);          /* the request body */
    /* path is available via lev_event_path(ev, ...) */
    lev_send_response(node, link_id, req_id, data, c, 3000);  /* echo it */
    break;
}
```

Requester (over an established link, whose id comes from `lev_link_id`):

```c
uint8_t req[] = { 0xA4, 'p','i','n','g' };   /* msgpack "ping" */
uint8_t request_id[LEV_ADDR_LEN];
lev_send_request(node, link_id, "/echo", req, sizeof(req), 5000, request_id);

case LEV_EVENT_RESPONSE_RECEIVED: {
    uint8_t rid[LEV_ADDR_LEN], body[512];
    uintptr_t a = sizeof(rid), b = sizeof(body);
    lev_event_request_id(ev, rid, sizeof(rid), &a);   /* match request_id */
    lev_event_data(ev, body, sizeof(body), &b);
    break;
}
```

A request that gets no reply within its deadline surfaces as
`LEV_EVENT_REQUEST_TIMEOUT`. To restrict callers, use
`LEV_REQUEST_POLICY_ALLOW_LIST` with an array of `n_ids` 16-byte identity
hashes.

Full program: `leviculum-ffi/examples/c/phase_d.c`.

## Datagrams

A datagram is a single, unreliable packet to a destination. A path must
already be known. Delivery is best-effort: a `LEV_EVENT_PACKET_RECEIVED` on the
other side, and a delivery confirmation only if the destination returns a
proof.

```c
uint8_t packet_hash[LEV_ADDR_LEN];
int rc = lev_send_datagram(node, dest_hash, (const uint8_t *)"hi", 2,
                           packet_hash, 3000);
if (rc == LEV_ERR_NO_PATH) { lev_request_path(node, dest_hash, 3000); }

/* receiver */
case LEV_EVENT_PACKET_RECEIVED: {
    uint8_t buf[256];
    uintptr_t n = sizeof(buf);
    lev_event_data(ev, buf, sizeof(buf), &n);
    break;
}
```

Full program: `leviculum-ffi/examples/c/phase_d.c`.

## Resource transfer

A resource carries bulk data (a file) over a link, in segments, with optional
compression and msgpack metadata. The receiver chooses a strategy: accept all,
reject all, or be asked per transfer.

Receiver sets a strategy on the link, then accepts when advertised:

```c
lev_set_resource_strategy(node, link_id, LEV_RESOURCE_ACCEPT_APP);

case LEV_EVENT_RESOURCE_ADVERTISED:
    lev_accept_resource(node, link_id, 3000);   /* or lev_reject_resource */
    break;

case LEV_EVENT_RESOURCE_COMPLETED: {
    uint8_t buf[65536];
    uintptr_t n = sizeof(buf);
    lev_event_data(ev, buf, sizeof(buf), &n);    /* the assembled data */
    /* metadata via lev_event_metadata(ev, ...) if present */
    break;
}
```

Sender initiates the transfer and tracks progress:

```c
uint8_t resource_hash[LEV_RESOURCE_HASH_LEN];
lev_send_resource(node, link_id, file_data, file_len,
                  NULL, 0,        /* optional msgpack metadata */
                  1,              /* auto-compress */
                  resource_hash, 5000);

case LEV_EVENT_RESOURCE_PROGRESS: {
    double frac;
    lev_event_progress(ev, &frac);   /* 0.0 .. 1.0 */
    break;
}
```

`LEV_EVENT_RESOURCE_COMPLETED` carries the data only on the receiver;
`LEV_EVENT_RESOURCE_FAILED` reports a transfer that did not finish.

`lev_send_resource` returns once the transfer is *initiated*: the receiver then
pulls the parts part by part. A sending program must keep its node alive and
running the event loop until the transfer is done, the receiver must keep the
link it accepted open (freeing a link closes it), and the receiver applies its
resource strategy on the link before the resource arrives. Exiting the sender
right after the call returns aborts an in-flight transfer.

Full programs: `leviculum-ffi/examples/c/phase_e.c`, and
`leviculum-ffi/examples/c/lncp.c`, a complete two-process file-copy tool
(`lncp send` / `lncp recv`) that exercises the whole stack end to end.

## Errors and logging

Every fallible call returns `int`. Pair the code with the thread-local detail:

```c
int rc = lev_connect(node, peer, 5000, &link);
if (rc != LEV_OK) {
    fprintf(stderr, "connect: %s (%s)\n", lev_strerror(rc), lev_last_error());
}
```

`LEV_ERR_AGAIN` (from `lev_link_try_send`) and `LEV_ERR_TIMEOUT` are normal,
retryable conditions, not hard failures. Logging from the stack itself is off
by default; turn it on and route it to your own sink:

```c
static void log_sink(int level, const char *msg, void *user) {
    (void)user;
    fprintf(stderr, "[lev %d] %s\n", level, msg);
}

lev_init();
lev_log_set_callback(log_sink, NULL);
lev_log_set_level(LEV_LOG_INFO);
```

The callback may run on an internal thread and must not call back into any
`lev_*` function. For hex display of an address, use `lev_hex_encode` and
`lev_hex_decode`.

## Diagnostics

For an `rnstatus`-style view, `lev_transport_stats` reads the transport
counters and the path-table size:

```c
uint64_t sent, received, dropped, paths;
lev_transport_stats(node, &sent, &received, NULL, NULL, &dropped, &paths);
```

Any out-pointer may be `NULL` to skip it.

For an `rnpath`-style listing, take a frozen snapshot of the path table, read
its entries by index, and free it:

```c
lev_path_table_t *table = lev_path_table_snapshot(node);
for (int i = 0; i < lev_path_table_count(table); i++) {
    uint8_t dest[LEV_ADDR_LEN];
    uint8_t hops;
    lev_path_table_entry(table, i, dest, &hops, NULL, NULL, NULL, NULL);
    /* `dest` reachable in `hops` hops */
}
lev_path_table_free(table);
```

The snapshot is a point-in-time copy, so reads never race a changing table.

Interface stats work the same way (`lev_interface_stats_snapshot` /
`_count` / `_name` / `_entry` / `_free`), giving each interface's name, online
status, and byte counters for an `rnstatus`-style interface listing. See
`leviculum-ffi/examples/c/stats.c`.

## Putting it together

A typical application wires these into one loop: it loads or generates an
identity, builds and starts a node with an interface, registers and announces a
destination, then runs the event loop, reacting to announces by connecting,
to link requests by accepting, and to data, request, and resource events by
serving the application. The `phase_b.c` through `phase_e.c` programs are
complete two-node demonstrations of exactly these flows, runnable via
`cargo test-ffi`.
