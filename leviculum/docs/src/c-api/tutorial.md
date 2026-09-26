# Tutorial: Build levcat, a Pipe over Reticulum

This tutorial builds one small, complete, genuinely useful program from scratch:
**levcat**, a bidirectional pipe over the mesh, the netcat of Reticulum. Run it
in two terminals and it is a chat. Feed it a file and it is a file transfer
(`levcat connect ... < file`). Drop it in a shell pipeline and it carries bytes
between machines, over TCP, over LoRa, over anything Reticulum reaches.

Along the way you learn the patterns every Leviculum C program needs: bring up a
node, announce and discover a destination, open a link, and — the heart of it —
run the node's event loop **inside your own** `poll(2)` loop, alongside your own
file descriptors. After this you can write your own Leviculum program.

This builds on the [Overview](overview.md) (opaque handles, the read(2) buffer
convention, the pollable event fd); skim it first. The [How-To](howto.md) is the
recipe companion, and the [API Reference](reference.md) has every signature. The
finished program is `leviculum-ffi/examples/c/levcat.c`, compiled and tested in
the repo, so the code here is real, not pseudo-code.

## What we build

Two roles share one transport and one steady-state loop:

```sh
levcat listen  <storage> <bind host:port>           # the listening end
levcat connect <storage> <peer host:port> <dest-hex> # the dialing end
```

The listener registers a destination, announces it, and prints its address. The
connector is handed that address, finds a path to it, and opens a link. Once
linked, both ends pump stdin to the link and link data to stdout.

## 1. Skeleton

Start with argument parsing, one-time init, and a signal flag so Ctrl-C exits
cleanly. `lev_init()` is optional (other calls run it lazily) but it is the
place to set up logging before anything else.

```c
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "leviculum.h"

static volatile sig_atomic_t stop = 0;
static void on_signal(int s) { (void)s; stop = 1; }

int main(int argc, char **argv) {
    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    lev_init();

    if (argc == 4 && strcmp(argv[1], "listen") == 0)
        return run_listen(argv[2], argv[3]);
    if (argc == 5 && strcmp(argv[1], "connect") == 0)
        return run_connect(argv[2], argv[3], argv[4]);

    fprintf(stderr, "usage:\n  %s listen  <storage> <bind host:port>\n"
                    "  %s connect <storage> <peer host:port> <dest-hex>\n",
            argv[0], argv[0]);
    return 2;
}
```

## 2. Bring up a node

A node is built, then started. The builder is an opaque handle you configure and
then consume; see [reference: node lifecycle and builder](reference.md#node-lifecycle-and-builder).
Both roles share this helper, differing only in the interface they add — a TCP
server for the listener, a TCP client for the connector.

```c
static leviculum_t *build_start(const char *storage,
                                void (*configure)(lev_builder_t *, const char *),
                                const char *arg, lev_identity_t *id) {
    lev_builder_t *b = lev_builder_new();
    if (!b) return NULL;
    if (lev_builder_storage_path(b, storage) != LEV_OK) { lev_builder_free(b); return NULL; }
    if (id) lev_builder_identity(b, id);
    configure(b, arg);                 /* add the interface */
    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);               /* build empties the builder; still free it */
    if (!node) return NULL;
    if (lev_start(node) != LEV_OK) { lev_free(node); return NULL; }
    return node;
}

static void cfg_server(lev_builder_t *b, const char *addr) { lev_builder_add_tcp_server(b, addr); }
static void cfg_client(lev_builder_t *b, const char *addr) { lev_builder_add_tcp_client(b, addr); }
```

## 3. The listening end

The listener owns a destination: an address other nodes can reach. We generate
an identity, register an incoming single destination under the app name
`levcat` with the aspect `pipe`, and read back its 16-byte hash. Then we
announce it so the network learns a path, and print the address — to **stderr**,
because stdout is the data pipe and must stay clean.

```c
lev_identity_t *id = lev_identity_generate();
leviculum_t *node = build_start(storage, cfg_server, bind_addr, id);

const char *aspects[] = {"pipe"};
lev_destination_t *dest =
    lev_destination_new(id, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "levcat", aspects, 1);
uint8_t dh[LEV_ADDR_LEN];
size_t dhl = sizeof(dh);
lev_destination_hash(dest, dh, sizeof(dh), &dhl);
lev_register_destination(node, dest);
lev_destination_free(dest);

char hexhash[2 * LEV_ADDR_LEN + 1];
hex(dh, LEV_ADDR_LEN, hexhash);                 /* lev_hex_encode wrapper */
fprintf(stderr, "destination: %s\n", hexhash);
```

Now wait for someone to dial in. We re-announce in a loop (so a peer that starts
later still discovers us) and watch for a `LEV_EVENT_LINK_REQUEST`. When it
arrives we read the link id from the event and accept it. See
[How-To: announcing and discovering](howto.md#announcing-and-discovering).

```c
lev_link_t *link = NULL;
while (!stop && !link) {
    lev_announce(node, dh, NULL, 0, 2000);
    for (int i = 0; i < 3 && !link; i++) {
        lev_event_t *ev = NULL;
        if (lev_wait_event(node, &ev, 200) != LEV_OK || !ev) continue;
        if (lev_event_type(ev) == LEV_EVENT_LINK_REQUEST) {
            uint8_t lid[LEV_ADDR_LEN];
            size_t l = sizeof(lid);
            lev_event_link_id(ev, lid, sizeof(lid), &l);
            lev_accept_link(node, lid, 5000, &link);
        }
        lev_event_free(ev);
    }
}
```

One subtlety: accepting a link does not make it immediately usable for
**sending**. The responder's link becomes active only after the initiator's RTT
exchange, signalled by the responder's own `LEV_EVENT_LINK_ESTABLISHED`. Sending
before that returns `LEV_ERR_SEND` ("link not active"). So we wait for it before
pumping, writing through any data that arrives meanwhile so none is lost:

```c
int active = 0;
while (link && !stop && !active) {
    lev_event_t *ev = NULL;
    if (lev_wait_event(node, &ev, 200) != LEV_OK || !ev) continue;
    int t = lev_event_type(ev);
    if (t == LEV_EVENT_LINK_ESTABLISHED) active = 1;
    else if (t == LEV_EVENT_LINK_MESSAGE) emit_message(ev);   /* don't drop early data */
    else if (t == LEV_EVENT_LINK_CLOSED)  stop = 1;
    lev_event_free(ev);
}
if (active) pump(node, link);
```

`lev_wait_event` is the blocking drain we use during setup; the steady-state
loop (`pump`, below) uses the pollable fd instead. Every event must be freed
with `lev_event_free`.

## 4. The dialing end

The connector is given the listener's address as hex. Decode it to 16 bytes,
bring up a node with a TCP client interface, and wait for a path: the listener's
announce arrives over the link and installs one. `lev_request_path` nudges it
along; `lev_has_path` reports when it is ready. See
[reference: paths, connect, and links](reference.md#paths-connect-and-links).

```c
uint8_t dest[LEV_ADDR_LEN];
size_t dlen = sizeof(dest);
lev_hex_decode((const uint8_t *)dest_hex, strlen(dest_hex), dest, sizeof(dest), &dlen);

leviculum_t *node = build_start(storage, cfg_client, peer_addr, NULL);

lev_request_path(node, dest, 2000);
for (int i = 0; i < 300 && lev_has_path(node, dest) != 1; i++) {
    lev_event_t *ev = NULL;
    if (lev_wait_event(node, &ev, 200) == LEV_OK && ev) lev_event_free(ev);
}
```

With a path in hand, open the link. `lev_connect` returns as soon as the request
is sent — the link is usable only after the handshake, which the engine signals
with `LEV_EVENT_LINK_ESTABLISHED`. Wait for it, then start pumping.

```c
lev_link_t *link = NULL;
lev_connect(node, dest, 8000, &link);

int established = 0;
for (int i = 0; i < 100 && !established; i++) {
    lev_event_t *ev = NULL;
    if (lev_wait_event(node, &ev, 200) == LEV_OK && ev) {
        if (lev_event_type(ev) == LEV_EVENT_LINK_ESTABLISHED) established = 1;
        lev_event_free(ev);
    }
}
if (established) pump(node, link);
```

## 5. The pump loop — the heart of it

Both ends now have a link and run the same loop. This is the pattern that makes
Leviculum composable: the node exposes a single readable file descriptor
(`lev_event_fd`), so you put it in your own `poll(2)` set right next to your own
fds. Here that is `stdin`. One `poll` waits for either: local input to send, or
a network event to receive. See [the event model](overview.md#the-event-model-a-pollable-fd).

```c
static void pump(leviculum_t *node, lev_link_t *link) {
    struct pollfd fds[2];
    fds[0].fd = STDIN_FILENO;        fds[0].events = POLLIN;
    fds[1].fd = lev_event_fd(node);  fds[1].events = POLLIN;

    while (!stop) {
        int r = poll(fds, 2, 1000);
        if (r < 0) { if (errno == EINTR) continue; break; }

        if (fds[0].revents & POLLIN) {          /* local input -> link */
            uint8_t buf[CHUNK];
            ssize_t n = read(STDIN_FILENO, buf, sizeof(buf));
            if (n <= 0) { /* EOF: flush, then close — see below */ return; }
            if (lev_link_send(link, buf, (size_t)n, 5000) != LEV_OK) return;
        }

        if ((fds[1].revents & POLLIN) && drain_to_stdout(node)) return; /* link -> stdout */
    }
}
```

Two details:

- **Chunking.** A link's reliable channel has a maximum message size, so we read
  stdin in `#define CHUNK 256`-byte pieces that fit on any interface. `lev_link_send`
  is the reliable, sequenced send; it blocks up to its deadline, retrying
  backpressure internally. (The non-blocking sibling is `lev_link_try_send`,
  which returns `LEV_ERR_AGAIN` instead of waiting — see
  [How-To: links and exchanging data](howto.md#links-and-exchanging-data).)
- **Receiving.** `lev_link_send` on one side surfaces as a `LEV_EVENT_LINK_MESSAGE`
  on the other. We drain every pending event and copy each message's bytes to
  stdout, using the read(2)-style accessor (size query, then fill):

```c
static void emit_message(lev_event_t *ev) {
    size_t need = 0;
    lev_event_data(ev, NULL, 0, &need);              /* size query */
    uint8_t *d = malloc(need ? need : 1);
    size_t got = need;
    lev_event_data(ev, d, need, &got);               /* fill */
    fwrite(d, 1, got, stdout);
    fflush(stdout);
    free(d);
}

static int drain_to_stdout(leviculum_t *node) {
    int closed = 0;
    lev_event_t *ev = NULL;
    while (lev_next_event(node, &ev) == LEV_OK && ev) {
        int t = lev_event_type(ev);
        if (t == LEV_EVENT_LINK_MESSAGE) {
            emit_message(ev);
        } else if (t == LEV_EVENT_LINK_CLOSED) {
            closed = 1;
        }
        lev_event_free(ev);
    }
    return closed;
}
```

The fd is **level-triggered**: it stays readable while the queue is non-empty,
so after each wake we drain with `lev_next_event` until it yields `NULL`. The
event side is single-consumer — never drain the same node from two threads.

## 6. Closing cleanly

When local input ends (Ctrl-D, or the end of a piped file), we are done sending.
Give the reliable channel a moment to deliver the last bytes — draining any
final inbound meanwhile — then close our end. The peer sees `LEV_EVENT_LINK_CLOSED`
and exits too, so a `cat file | levcat connect ...` terminates instead of
hanging. This is the `if (n <= 0)` branch of the pump:

```c
for (int g = 0; g < 10 && !stop; g++) {
    struct pollfd ef = {fds[1].fd, POLLIN, 0};
    if (poll(&ef, 1, 100) > 0 && drain_to_stdout(node)) break;
}
lev_close_link(link, 2000);
return;
```

(A production tool would do a real half-close so the reverse direction can keep
flowing; we keep it minimal.) Then tear the node down in order — the link first,
then the node:

```c
lev_link_free(link);   /* NULL-safe; closes the link if still open */
lev_stop(node);        /* persists state, stops the loop */
lev_free(node);        /* releases the runtime and the event fd */
lev_identity_free(id); /* listener only */
```

The shutdown order is mandatory: stop reacting to the event fd before
`lev_free`, which closes it.

## 7. Build and run it

Compile against the installed library with pkg-config
([installing and linking](overview.md#installing-and-linking)):

```sh
cc levcat.c $(pkg-config --cflags --libs leviculum) -o levcat
```

Open two terminals. In the first, listen:

```sh
$ ./levcat listen /tmp/levcat-a 127.0.0.1:4242
destination: a1b2c3d4e5f6...        # printed on stderr
```

In the second, connect with that address, then type on either side:

```sh
$ ./levcat connect /tmp/levcat-b 127.0.0.1:4242 a1b2c3d4e5f6...
hello from the other terminal
```

That is a chat. It is also a pipe — send a file and end with Ctrl-D, or:

```sh
# receiver
./levcat listen  /tmp/levcat-a 127.0.0.1:4242 > received.tar
# sender
tar c somedir | ./levcat connect /tmp/levcat-b 127.0.0.1:4242 <dest-hex>
```

Nothing here is TCP-specific. Swap `lev_builder_add_tcp_*` for
`lev_builder_add_rnode` (or load a config file) and the same program pipes bytes
across a LoRa mesh.

## Where to go next

You have used the core of the API: node setup, announce and discovery, links,
and the event loop. From here:

- Bulk data with progress, compression, and metadata:
  [How-To: resource transfer](howto.md#resource-transfer) (and the full
  `leviculum-ffi/examples/c/lncp.c` file-copy tool).
- Lightweight RPC: [How-To: request and response](howto.md#request-and-response).
- Best-effort single packets: [How-To: datagrams](howto.md#datagrams).
- Inspecting the stack: [How-To: diagnostics](howto.md#diagnostics).
- Every signature and constant: the [API Reference](reference.md).

Full program: `leviculum-ffi/examples/c/levcat.c`.
