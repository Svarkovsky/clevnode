/*
 * Leviculum C API example and acceptance test, phase b.
 *
 * Two nodes over TCP loopback. Node A registers an incoming destination and
 * announces it; node B observes the announce as an event over its pollable
 * event fd, identified by the destination hash.
 *
 * Takes the loopback listen address as argv[1]. `127.0.0.1:0` is the
 * intended form: node A binds it, reads the kernel-assigned port back with
 * lev_tcp_listen_addr, and node B dials that (Codeberg #221). The Rust
 * harness in tests/ffi_c_tests.rs passes exactly this.
 *
 * Returns 0 on success, non-zero on failure. Compiled and run by the Rust
 * harness in tests/ffi_c_tests.rs.
 */

#include <stdio.h>
#include <string.h>

#include "leviculum.h"

static int failures = 0;

#define CHECK(cond)                                                            \
    do {                                                                       \
        if (!(cond)) {                                                         \
            fprintf(stderr, "  CHECK failed at %s:%d: %s\n", __FILE__,         \
                    __LINE__, #cond);                                          \
            failures++;                                                        \
        }                                                                      \
    } while (0)

/* Drain B's events once, returning 1 if an AnnounceReceived for `want` is
 * seen. */
static int drain_for_announce(leviculum_t *b, const uint8_t want[LEV_ADDR_LEN]) {
    int got = 0;
    lev_event_t *ev = NULL;
    /* Block briefly for the first event, then drain the rest non-blocking. */
    if (lev_wait_event(b, &ev, 300) != LEV_OK) {
        return 0;
    }
    while (ev) {
        if (lev_event_type(ev) == LEV_EVENT_ANNOUNCE_RECEIVED) {
            uint8_t eh[LEV_ADDR_LEN];
            size_t ehl = sizeof(eh);
            if (lev_event_dest_hash(ev, eh, sizeof(eh), &ehl) == LEV_OK &&
                ehl == LEV_ADDR_LEN && memcmp(eh, want, LEV_ADDR_LEN) == 0) {
                got = 1;
            }
        }
        lev_event_free(ev);
        ev = NULL;
        if (got || lev_next_event(b, &ev) != LEV_OK) {
            break;
        }
    }
    return got;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr,
                "usage: %s <host:port>\n"
                "  Listen address for node A's TCP server; 127.0.0.1:0 is the\n"
                "  intended form. A binds it and B dials the kernel-assigned\n"
                "  port A reports (Codeberg #221). No default on purpose: a\n"
                "  compiled-in literal is what made ports collide across the\n"
                "  suite (Codeberg #206).\n",
                argv[0]);
        return 2;
    }
    const char *addr = argv[1];

    printf("leviculum phase b C acceptance test\n");
    CHECK(lev_init() == LEV_OK);

    /* Node A: TCP server, with an identity and an incoming destination. */
    lev_identity_t *ida = lev_identity_generate();
    CHECK(ida != NULL);
    lev_builder_t *ba = lev_builder_new();
    CHECK(lev_builder_identity(ba, ida) == LEV_OK);
    CHECK(lev_builder_storage_path(ba, "/tmp/leviculum-c-phase-b-a") == LEV_OK);
    CHECK(lev_builder_add_tcp_server(ba, addr) == LEV_OK);
    CHECK(lev_builder_enable_transport(ba, 1) == LEV_OK);
    leviculum_t *a = lev_builder_build(ba);
    lev_builder_free(ba);
    CHECK(a != NULL);
    CHECK(lev_start(a) == LEV_OK);

    /* The listen address is normally "127.0.0.1:0": A binds it and reports
     * the kernel-assigned port, which B dials (Codeberg #221). */
    char bound[64];
    size_t bound_len = 0;
    CHECK(lev_tcp_listen_addr(a, 0, (uint8_t *)bound, sizeof(bound) - 1,
                              &bound_len) == LEV_OK);
    bound[bound_len] = '\0';

    /* Node B: TCP client to A. */
    lev_builder_t *bb = lev_builder_new();
    CHECK(lev_builder_storage_path(bb, "/tmp/leviculum-c-phase-b-b") == LEV_OK);
    CHECK(lev_builder_add_tcp_client(bb, bound) == LEV_OK);
    CHECK(lev_builder_enable_transport(bb, 1) == LEV_OK);
    leviculum_t *b = lev_builder_build(bb);
    lev_builder_free(bb);
    CHECK(b != NULL);
    CHECK(lev_start(b) == LEV_OK);

    /* A: register an incoming SINGLE destination and capture its hash. */
    const char *aspects[] = {"announce"};
    lev_destination_t *dest = lev_destination_new(
        ida, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "leviculum-demo", aspects, 1);
    CHECK(dest != NULL);
    uint8_t dh[LEV_ADDR_LEN];
    size_t dhl = sizeof(dh);
    CHECK(lev_destination_hash(dest, dh, sizeof(dh), &dhl) == LEV_OK);
    CHECK(dhl == LEV_ADDR_LEN);
    CHECK(lev_register_destination(a, dest) == LEV_OK);
    lev_destination_free(dest);

    /* Re-announce each round until B observes it, so the test does not race the
     * TCP connect. Up to ~15s, normally one or two rounds. */
    int got = 0;
    for (int round = 0; round < 50 && !got; round++) {
        CHECK(lev_announce(a, dh, (const uint8_t *)"hello", 5, 2000) == LEV_OK);
        if (drain_for_announce(b, dh)) {
            got = 1;
        }
    }
    CHECK(got == 1);

    CHECK(lev_stop(a) == LEV_OK);
    CHECK(lev_stop(b) == LEV_OK);
    lev_free(a);
    lev_free(b);
    lev_identity_free(ida);

    if (failures == 0) {
        printf("OK\n");
        return 0;
    }
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
}
