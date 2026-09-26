/*
 * lncp: a minimal file-copy tool on the Leviculum C API, the C analogue of
 * rncp/lncp. It exercises the whole stack end to end from a C program:
 * identity, destination, announce, path discovery, link, and a reliable
 * resource transfer carrying the file (with the file name in the metadata).
 *
 * Usage:
 *   lncp recv <storage_dir> <listen host:port> <out_path>
 *   lncp send <storage_dir> <peer host:port> <in_path>
 *   lncp recv-shared <storage_dir> <instance_name> <out_path>
 *   lncp send-shared <storage_dir> <instance_name> <in_path>
 *
 * The recv/send modes bring up an own node over TCP; the *-shared modes instead
 * attach to a running lnsd/rnsd over its shared-instance IPC socket (named by
 * its instance_name), the way rncp/rnx normally work, reusing the daemon's
 * interfaces. The transfer logic is identical for both.
 *
 * The receiver announces a destination and prints "DEST <hex>" then "READY";
 * the sender discovers that destination from its announce, opens a link, and
 * sends the file. On completion the receiver writes <out_path>, prints "DONE",
 * and exits 0. Returns non-zero on any failure.
 *
 * Built against the real libleviculum.so and driven by the Rust harness in
 * tests/ffi_lncp.rs.
 */

#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "leviculum.h"

static volatile sig_atomic_t stop = 0;
static void on_signal(int s) {
    (void)s;
    stop = 1;
}

static void hex(const uint8_t *in, size_t n, char *out) {
    uint8_t enc[256];
    size_t el = sizeof(enc);
    lev_hex_encode(in, n, enc, sizeof(enc), &el);
    memcpy(out, enc, el);
    out[el] = '\0';
}

/* Read an entire file into a malloc'd buffer. Caller frees. */
static uint8_t *read_file(const char *path, size_t *len) {
    FILE *f = fopen(path, "rb");
    if (!f) {
        return NULL;
    }
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    if (sz < 0) {
        fclose(f);
        return NULL;
    }
    uint8_t *buf = malloc((size_t)sz ? (size_t)sz : 1);
    size_t got = fread(buf, 1, (size_t)sz, f);
    fclose(f);
    *len = got;
    return buf;
}

static int write_file(const char *path, const uint8_t *data, size_t len) {
    FILE *f = fopen(path, "wb");
    if (!f) {
        return -1;
    }
    size_t put = fwrite(data, 1, len, f);
    fclose(f);
    return put == len ? 0 : -1;
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
/* Attach to a running lnsd/rnsd over its shared-instance IPC socket, instead of
 * bringing up an interface of our own. `arg` is the daemon's instance name. */
static void cfg_shared(lev_builder_t *b, const char *instance_name) {
    lev_builder_connect_shared_instance(b, instance_name);
}

static int run_recv(const char *storage, void (*configure)(lev_builder_t *, const char *),
                    const char *endpoint, const char *out_path) {
    lev_identity_t *id = lev_identity_generate();
    leviculum_t *node = build_start(storage, configure, endpoint, id);
    if (!node) {
        fprintf(stderr, "recv: bring-up failed: %s\n", lev_last_error());
        return 1;
    }

    const char *aspects[] = {"file"};
    lev_destination_t *dest =
        lev_destination_new(id, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "lncp", aspects, 1);
    uint8_t dh[LEV_ADDR_LEN];
    size_t dhl = sizeof(dh);
    lev_destination_hash(dest, dh, sizeof(dh), &dhl);
    if (lev_register_destination(node, dest) != LEV_OK) {
        fprintf(stderr, "recv: register failed\n");
        return 1;
    }
    lev_destination_free(dest);

    char hexhash[2 * LEV_ADDR_LEN + 1];
    hex(dh, LEV_ADDR_LEN, hexhash);
    printf("DEST %s\n", hexhash);
    printf("READY\n");
    fflush(stdout);

    struct timespec last = {0, 0};
    int rc = 1;
    lev_link_t *accepted = NULL; /* kept open for the transfer */
    while (!stop) {
        /* Re-announce roughly every 500 ms so the sender can discover us. */
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec != last.tv_sec || now.tv_nsec - last.tv_nsec > 500000000L) {
            lev_announce(node, dh, NULL, 0, 2000);
            last = now;
        }

        lev_event_t *ev = NULL;
        if (lev_wait_event(node, &ev, 200) != LEV_OK || !ev) {
            continue;
        }
        switch (lev_event_type(ev)) {
            case LEV_EVENT_LINK_ESTABLISHED: {
                /* Auto-accept model: the inbound link is already accepted and
                 * proved by the core. is_sender == 0 marks the inbound link
                 * (this node is the responder); mint a handle for it. */
                if (lev_event_is_sender(ev)) {
                    break;
                }
                uint8_t lid[LEV_ADDR_LEN];
                size_t l = sizeof(lid);
                lev_event_link_id(ev, lid, sizeof(lid), &l);
                /* Keep the link open (freeing it would close it) for the
                 * duration of the transfer. */
                if (lev_accept_link(node, lid, 5000, &accepted) == LEV_OK) {
                    lev_set_resource_strategy(node, lid, LEV_RESOURCE_ACCEPT_ALL);
                }
                break;
            }
            case LEV_EVENT_RESOURCE_COMPLETED: {
                size_t need = 0;
                lev_event_data(ev, NULL, 0, &need);
                uint8_t *buf = malloc(need ? need : 1);
                size_t got = need;
                lev_event_data(ev, buf, need, &got);
                if (write_file(out_path, buf, got) == 0) {
                    printf("DONE %zu\n", got);
                    fflush(stdout);
                    rc = 0;
                }
                free(buf);
                stop = 1;
                break;
            }
            default:
                break;
        }
        lev_event_free(ev);
    }

    lev_link_free(accepted);
    lev_stop(node);
    lev_free(node);
    lev_identity_free(id);
    return rc;
}

static int run_send(const char *storage, void (*configure)(lev_builder_t *, const char *),
                    const char *endpoint, const char *in_path) {
    size_t flen = 0;
    uint8_t *fdata = read_file(in_path, &flen);
    if (!fdata) {
        fprintf(stderr, "send: cannot read %s\n", in_path);
        return 1;
    }

    leviculum_t *node = build_start(storage, configure, endpoint, NULL);
    if (!node) {
        fprintf(stderr, "send: bring-up failed: %s\n", lev_last_error());
        free(fdata);
        return 1;
    }

    /* Discover the receiver's destination from its announce, then wait until a
     * path to it is installed before linking. */
    uint8_t dest[LEV_ADDR_LEN];
    int learned = 0;
    for (int i = 0; i < 300 && !(learned && lev_has_path(node, dest) == 1); i++) {
        lev_event_t *ev = NULL;
        if (lev_wait_event(node, &ev, 200) == LEV_OK && ev) {
            if (lev_event_type(ev) == LEV_EVENT_ANNOUNCE_RECEIVED) {
                size_t l = sizeof(dest);
                if (lev_event_dest_hash(ev, dest, sizeof(dest), &l) == LEV_OK) {
                    learned = 1;
                }
            }
            lev_event_free(ev);
        }
    }
    if (!(learned && lev_has_path(node, dest) == 1)) {
        fprintf(stderr, "send: no path to any destination\n");
        free(fdata);
        return 1;
    }

    lev_link_t *link = NULL;
    if (lev_connect(node, dest, 8000, &link) != LEV_OK) {
        fprintf(stderr, "send: connect failed: %s\n", lev_last_error());
        free(fdata);
        return 1;
    }
    uint8_t lid[LEV_ADDR_LEN];
    size_t ll = sizeof(lid);
    lev_link_id(link, lid, sizeof(lid), &ll);

    /* lev_connect returns once the request is sent; the link is usable only
     * after the handshake completes, signalled by LINK_ESTABLISHED. */
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
        fprintf(stderr, "send: link never established\n");
        lev_link_free(link);
        free(fdata);
        return 1;
    }

    /* The basename as msgpack metadata (fixstr, for names shorter than 32). */
    const char *base = strrchr(in_path, '/');
    base = base ? base + 1 : in_path;
    size_t blen = strlen(base);
    uint8_t meta[64];
    size_t mlen = 0;
    if (blen < 32) {
        meta[0] = (uint8_t)(0xA0 | blen);
        memcpy(meta + 1, base, blen);
        mlen = blen + 1;
    }

    uint8_t rhash[32];
    int rc = 1;
    if (lev_send_resource(node, lid, fdata, flen, mlen ? meta : NULL, mlen, 1, rhash,
                          30000) == LEV_OK) {
        printf("SENT %zu\n", flen);
        fflush(stdout);
        rc = 0;
        /* The receiver pulls the resource part by part after the send call
         * returns; keep the link and runtime alive until our own (sender-side)
         * RESOURCE_COMPLETED confirms it finished (or the link drops). */
        for (int i = 0; i < 200; i++) {
            lev_event_t *ev = NULL;
            if (lev_wait_event(node, &ev, 100) == LEV_OK && ev) {
                int t = lev_event_type(ev);
                int sender = lev_event_is_sender(ev);
                lev_event_free(ev);
                if ((t == LEV_EVENT_RESOURCE_COMPLETED && sender) ||
                    t == LEV_EVENT_LINK_CLOSED) {
                    break;
                }
                if (t == LEV_EVENT_RESOURCE_FAILED && sender) {
                    rc = 1;
                    break;
                }
            }
        }
    } else {
        fprintf(stderr, "send: resource transfer failed: %s\n", lev_last_error());
    }

    lev_link_free(link);
    lev_stop(node);
    lev_free(node);
    free(fdata);
    return rc;
}

int main(int argc, char **argv) {
    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);
    lev_init();

    if (argc == 5 && strcmp(argv[1], "recv") == 0) {
        return run_recv(argv[2], cfg_server, argv[3], argv[4]);
    }
    if (argc == 5 && strcmp(argv[1], "send") == 0) {
        return run_send(argv[2], cfg_client, argv[3], argv[4]);
    }
    /* Daemon-client modes: attach to a running lnsd's shared instance by name
     * instead of bringing up an own interface. */
    if (argc == 5 && strcmp(argv[1], "recv-shared") == 0) {
        return run_recv(argv[2], cfg_shared, argv[3], argv[4]);
    }
    if (argc == 5 && strcmp(argv[1], "send-shared") == 0) {
        return run_send(argv[2], cfg_shared, argv[3], argv[4]);
    }
    fprintf(stderr,
            "usage:\n  %s recv <storage> <listen host:port> <out_path>\n"
            "  %s send <storage> <peer host:port> <in_path>\n"
            "  %s recv-shared <storage> <instance_name> <out_path>\n"
            "  %s send-shared <storage> <instance_name> <in_path>\n",
            argv[0], argv[0], argv[0], argv[0]);
    return 2;
}
