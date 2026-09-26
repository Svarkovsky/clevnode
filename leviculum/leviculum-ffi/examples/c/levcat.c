/*
 * levcat: a bidirectional pipe over Reticulum, the netcat of the mesh, built on
 * the Leviculum C API. Whatever you type or pipe into one side comes out the
 * other. It is at once a chat (run it in two terminals), a file pipe
 * (`levcat connect ... < file` / `levcat listen ... > file`), and a building
 * block for shell pipelines, and it works over any interface, including LoRa.
 *
 * Usage:
 *   levcat listen  <storage_dir> <bind host:port>
 *   levcat connect <storage_dir> <peer host:port> <dest-hex>
 *
 * The listener registers a destination, announces it, and prints its 32-hex
 * address to STDERR (status goes to stderr, data to stdout, so the address
 * never pollutes the pipe). The connector is given that address, learns a path
 * to it from the announce, and opens a link. Once linked, both sides run the
 * same loop: poll(2) on stdin AND the node's event fd at the same time, sending
 * local input over the link and writing received bytes to stdout. This is the
 * core Leviculum pattern, the event fd composing with the application's own
 * loop. The companion tutorial (docs/src/c-api/tutorial.md) builds this file
 * step by step.
 *
 * Built against the real libleviculum and driven by the harness in
 * tests/ffi_c_tests.rs.
 */

#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include "leviculum.h"

/* The link's reliable channel has a maximum message size; read stdin in chunks
 * that comfortably fit it on any interface (including small LoRa MDUs). */
#define CHUNK 256

static volatile sig_atomic_t stop = 0;
static void on_signal(int s) {
    (void)s;
    stop = 1;
}

static void hex(const uint8_t *in, size_t n, char *out) {
    uint8_t enc[2 * LEV_ADDR_LEN + 1];
    size_t el = sizeof(enc);
    lev_hex_encode(in, n, enc, sizeof(enc), &el);
    memcpy(out, enc, el);
    out[el] = '\0';
}

static leviculum_t *build_start(const char *storage,
                                void (*configure)(lev_builder_t *, const char *),
                                const char *arg, lev_identity_t *id) {
    lev_builder_t *b = lev_builder_new();
    if (!b) {
        return NULL;
    }
    if (lev_builder_storage_path(b, storage) != LEV_OK) {
        lev_builder_free(b);
        return NULL;
    }
    if (id) {
        lev_builder_identity(b, id);
    }
    configure(b, arg);
    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);
    if (!node) {
        return NULL;
    }
    if (lev_start(node) != LEV_OK) {
        lev_free(node);
        return NULL;
    }
    return node;
}

static void cfg_server(lev_builder_t *b, const char *addr) {
    lev_builder_add_tcp_server(b, addr);
}
static void cfg_client(lev_builder_t *b, const char *addr) {
    lev_builder_add_tcp_client(b, addr);
}

/* Write one received link message's bytes to stdout. */
static void emit_message(lev_event_t *ev) {
    size_t need = 0;
    lev_event_data(ev, NULL, 0, &need);
    uint8_t *d = malloc(need ? need : 1);
    size_t got = need;
    lev_event_data(ev, d, need, &got);
    fwrite(d, 1, got, stdout);
    fflush(stdout);
    free(d);
}

/* Drain all pending events, writing received link bytes to stdout. Returns 1 if
 * the peer closed the link. */
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

/* The shared steady state: pump stdin -> link and link -> stdout until either
 * end closes. poll(2) waits on the application's own fd (stdin) and the node's
 * readable event fd together, the whole point of a pollable event fd. */
static void pump(leviculum_t *node, lev_link_t *link) {
    struct pollfd fds[2];
    fds[0].fd = STDIN_FILENO;
    fds[0].events = POLLIN;
    fds[1].fd = lev_event_fd(node);
    fds[1].events = POLLIN;

    while (!stop) {
        int r = poll(fds, 2, 1000);
        if (r < 0) {
            if (errno == EINTR) {
                continue;
            }
            break;
        }

        if (fds[0].revents & POLLIN) {
            uint8_t buf[CHUNK];
            ssize_t n = read(STDIN_FILENO, buf, sizeof(buf));
            if (n <= 0) {
                /* End of local input: flush any final inbound for a moment so
                 * the reliable channel delivers our last bytes, then close our
                 * end so the peer learns we are done and exits too. */
                for (int g = 0; g < 10 && !stop; g++) {
                    struct pollfd ef = {fds[1].fd, POLLIN, 0};
                    if (poll(&ef, 1, 100) > 0 && drain_to_stdout(node)) {
                        break;
                    }
                }
                lev_close_link(link, 2000);
                return;
            }
            if (lev_link_send(link, buf, (size_t)n, 5000) != LEV_OK) {
                return; /* link gone */
            }
        }

        if ((fds[1].revents & POLLIN) && drain_to_stdout(node)) {
            return; /* peer closed */
        }
    }
}

static int run_listen(const char *storage, const char *bind_addr) {
    lev_identity_t *id = lev_identity_generate();
    leviculum_t *node = build_start(storage, cfg_server, bind_addr, id);
    if (!node) {
        fprintf(stderr, "listen: bring-up failed: %s\n", lev_last_error());
        lev_identity_free(id);
        return 1;
    }

    const char *aspects[] = {"pipe"};
    lev_destination_t *dest =
        lev_destination_new(id, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "levcat", aspects, 1);
    uint8_t dh[LEV_ADDR_LEN];
    size_t dhl = sizeof(dh);
    lev_destination_hash(dest, dh, sizeof(dh), &dhl);
    if (lev_register_destination(node, dest) != LEV_OK) {
        fprintf(stderr, "listen: register failed\n");
        lev_destination_free(dest);
        lev_stop(node);
        lev_free(node);
        lev_identity_free(id);
        return 1;
    }
    lev_destination_free(dest);

    char hexhash[2 * LEV_ADDR_LEN + 1];
    hex(dh, LEV_ADDR_LEN, hexhash);
    fprintf(stderr, "destination: %s\n", hexhash);

    /* Announce until a peer opens a link, then start pumping. Auto-accept model
     * (Python-RNS parity): the core accepts and proves the inbound link, so the
     * responder sees a single LINK_ESTABLISHED (is_sender == 0) once the RTT
     * exchange completes and the link is active. Mint its handle and pump; an
     * earlier handshake never surfaces as a separate request event. */
    lev_link_t *link = NULL;
    while (!stop && !link) {
        lev_announce(node, dh, NULL, 0, 2000);
        for (int i = 0; i < 3 && !link; i++) {
            lev_event_t *ev = NULL;
            if (lev_wait_event(node, &ev, 200) != LEV_OK || !ev) {
                continue;
            }
            int t = lev_event_type(ev);
            if (t == LEV_EVENT_LINK_ESTABLISHED && !lev_event_is_sender(ev)) {
                uint8_t lid[LEV_ADDR_LEN];
                size_t l = sizeof(lid);
                lev_event_link_id(ev, lid, sizeof(lid), &l);
                lev_accept_link(node, lid, 5000, &link);
            } else if (t == LEV_EVENT_LINK_MESSAGE) {
                emit_message(ev);
            }
            lev_event_free(ev);
        }
    }

    if (link) {
        pump(node, link);
    }

    lev_link_free(link);
    lev_stop(node);
    lev_free(node);
    lev_identity_free(id);
    return 0;
}

static int run_connect(const char *storage, const char *peer_addr, const char *dest_hex) {
    uint8_t dest[LEV_ADDR_LEN];
    size_t dlen = sizeof(dest);
    if (lev_hex_decode((const uint8_t *)dest_hex, strlen(dest_hex), dest, sizeof(dest), &dlen) !=
            LEV_OK ||
        dlen != LEV_ADDR_LEN) {
        fprintf(stderr, "connect: bad destination hex\n");
        return 1;
    }

    leviculum_t *node = build_start(storage, cfg_client, peer_addr, NULL);
    if (!node) {
        fprintf(stderr, "connect: bring-up failed: %s\n", lev_last_error());
        return 1;
    }

    /* Learn a path to the destination from the listener's announce, nudging it
     * along with a path request, then drain events until the path is installed. */
    lev_request_path(node, dest, 2000);
    for (int i = 0; i < 300 && lev_has_path(node, dest) != 1; i++) {
        lev_event_t *ev = NULL;
        if (lev_wait_event(node, &ev, 200) == LEV_OK && ev) {
            lev_event_free(ev);
        }
    }
    if (lev_has_path(node, dest) != 1) {
        fprintf(stderr, "connect: no path to destination\n");
        lev_stop(node);
        lev_free(node);
        return 1;
    }

    lev_link_t *link = NULL;
    if (lev_connect(node, dest, 8000, &link) != LEV_OK) {
        fprintf(stderr, "connect: link failed: %s\n", lev_last_error());
        lev_stop(node);
        lev_free(node);
        return 1;
    }

    /* lev_connect returns once the request is sent; wait for the handshake. */
    int established = 0;
    for (int i = 0; i < 100 && !established; i++) {
        lev_event_t *ev = NULL;
        if (lev_wait_event(node, &ev, 200) == LEV_OK && ev) {
            if (lev_event_type(ev) == LEV_EVENT_LINK_ESTABLISHED) {
                established = 1;
            }
            lev_event_free(ev);
        }
    }
    if (!established) {
        fprintf(stderr, "connect: link never established\n");
        lev_link_free(link);
        lev_stop(node);
        lev_free(node);
        return 1;
    }

    pump(node, link);

    lev_link_free(link);
    lev_stop(node);
    lev_free(node);
    return 0;
}

int main(int argc, char **argv) {
    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    lev_init();

    if (argc == 4 && strcmp(argv[1], "listen") == 0) {
        return run_listen(argv[2], argv[3]);
    }
    if (argc == 5 && strcmp(argv[1], "connect") == 0) {
        return run_connect(argv[2], argv[3], argv[4]);
    }
    fprintf(stderr,
            "usage:\n  %s listen  <storage> <bind host:port>\n"
            "  %s connect <storage> <peer host:port> <dest-hex>\n",
            argv[0], argv[0]);
    return 2;
}
