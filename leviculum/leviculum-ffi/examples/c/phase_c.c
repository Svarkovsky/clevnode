/*
 * Leviculum C API example and acceptance test, phase c.
 *
 * Two nodes over TCP loopback establish a link and exchange data: node B
 * learns node A from an announce, connects, A accepts, and B sends a message
 * that A receives as a link-data event.
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

/* Wait up to `rounds` * 200ms for an event of `want` type on `n`. On a match,
 * optionally copy the link id and data out. Returns 1 if seen. */
static int wait_for(leviculum_t *n, int want, uint8_t *link_id, uint8_t *data,
                    size_t *data_len, int rounds) {
    for (int r = 0; r < rounds; r++) {
        lev_event_t *ev = NULL;
        if (lev_wait_event(n, &ev, 200) != LEV_OK) {
            return 0;
        }
        if (!ev) {
            continue;
        }
        int matched = 0;
        if (lev_event_type(ev) == want) {
            if (link_id) {
                size_t l = LEV_ADDR_LEN;
                lev_event_link_id(ev, link_id, LEV_ADDR_LEN, &l);
            }
            if (data && data_len) {
                lev_event_data(ev, data, *data_len, data_len);
            }
            matched = 1;
        }
        lev_event_free(ev);
        if (matched) {
            return 1;
        }
    }
    return 0;
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

    printf("leviculum phase c C acceptance test\n");
    CHECK(lev_init() == LEV_OK);

    /* Node A: TCP server with an identity and an incoming destination. */
    lev_identity_t *ida = lev_identity_generate();
    lev_builder_t *ba = lev_builder_new();
    CHECK(lev_builder_identity(ba, ida) == LEV_OK);
    CHECK(lev_builder_storage_path(ba, "/tmp/leviculum-c-phase-c-a") == LEV_OK);
    CHECK(lev_builder_add_tcp_server(ba, addr) == LEV_OK);
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
    CHECK(lev_builder_storage_path(bb, "/tmp/leviculum-c-phase-c-b") == LEV_OK);
    CHECK(lev_builder_add_tcp_client(bb, bound) == LEV_OK);
    leviculum_t *b = lev_builder_build(bb);
    lev_builder_free(bb);
    CHECK(b != NULL);
    CHECK(lev_start(b) == LEV_OK);

    /* A registers an incoming SINGLE destination. */
    const char *aspects[] = {"link"};
    lev_destination_t *dest = lev_destination_new(
        ida, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "leviculum-demo", aspects, 1);
    CHECK(dest != NULL);
    uint8_t dh[LEV_ADDR_LEN];
    size_t dhl = sizeof(dh);
    CHECK(lev_destination_hash(dest, dh, sizeof(dh), &dhl) == LEV_OK);
    CHECK(lev_register_destination(a, dest) == LEV_OK);
    lev_destination_free(dest);

    /* B learns A: re-announce until B has a path (and the cached identity). */
    int ready = 0;
    for (int round = 0; round < 50 && !ready; round++) {
        CHECK(lev_announce(a, dh, NULL, 0, 2000) == LEV_OK);
        lev_event_t *ev = NULL;
        lev_wait_event(b, &ev, 300);
        while (ev) {
            lev_event_free(ev);
            ev = NULL;
            if (lev_next_event(b, &ev) != LEV_OK) {
                break;
            }
        }
        if (lev_has_path(b, dh)) {
            ready = 1;
        }
    }
    CHECK(ready == 1);

    /* B connects; the signing key is resolved from the cached identity. */
    lev_link_t *lb = NULL;
    CHECK(lev_connect(b, dh, 5000, &lb) == LEV_OK);
    CHECK(lb != NULL);

    /* Auto-accept model: the core accepts and proves the inbound link, so A
     * sees an inbound LINK_ESTABLISHED instead of a request event. Mint its
     * handle. (A is purely a responder here, so the only established link it
     * sees is this inbound one.) */
    uint8_t lid[LEV_ADDR_LEN];
    CHECK(wait_for(a, LEV_EVENT_LINK_ESTABLISHED, lid, NULL, NULL, 50));
    lev_link_t *la = NULL;
    CHECK(lev_accept_link(a, lid, 5000, &la) == LEV_OK);
    CHECK(la != NULL);

    /* B sees the link come up, then sends a message A receives as link data. */
    CHECK(wait_for(b, LEV_EVENT_LINK_ESTABLISHED, NULL, NULL, NULL, 50));

    const char *msg = "ping";
    CHECK(lev_link_send(lb, (const uint8_t *)msg, 4, 5000) == LEV_OK);

    /* lev_link_send goes through the reliable channel, so the peer sees a
     * sequenced LINK_MESSAGE, not a raw LINK_DATA packet. Drain it directly to
     * read the channel metadata (message type and sequence number). */
    int got_msg = 0;
    for (int r = 0; r < 50 && !got_msg; r++) {
        lev_event_t *ev = NULL;
        if (lev_wait_event(a, &ev, 200) != LEV_OK) {
            break;
        }
        if (!ev) {
            continue;
        }
        if (lev_event_type(ev) == LEV_EVENT_LINK_MESSAGE) {
            uint8_t rx[64];
            size_t rxl = sizeof(rx);
            CHECK(lev_event_data(ev, rx, sizeof(rx), &rxl) == LEV_OK);
            CHECK(rxl == 4 && memcmp(rx, msg, 4) == 0);
            uint16_t msgtype = 1, sequence = 9;
            CHECK(lev_event_msgtype(ev, &msgtype) == LEV_OK);
            CHECK(lev_event_sequence(ev, &sequence) == LEV_OK);
            CHECK(msgtype == 0);  /* raw-bytes channel message */
            CHECK(sequence == 0); /* first message on the channel */
            got_msg = 1;
        }
        lev_event_free(ev);
    }
    CHECK(got_msg);

    /* B proves an identity on the link; A reads it back by hash. */
    lev_identity_t *bident = lev_identity_generate();
    uint8_t bident_hash[LEV_ADDR_LEN];
    size_t bidl = sizeof(bident_hash);
    CHECK(lev_identity_hash(bident, bident_hash, sizeof(bident_hash), &bidl) ==
          LEV_OK);
    uint8_t lb_id[LEV_ADDR_LEN];
    size_t lbidl = sizeof(lb_id);
    CHECK(lev_link_id(lb, lb_id, sizeof(lb_id), &lbidl) == LEV_OK);
    CHECK(lev_link_identify(b, lb_id, bident, 3000) == LEV_OK);

    CHECK(wait_for(a, LEV_EVENT_LINK_IDENTIFIED, NULL, NULL, NULL, 50));
    lev_identity_t *remote = lev_link_remote_identity(a, lid);
    CHECK(remote != NULL);
    if (remote) {
        uint8_t rh[LEV_ADDR_LEN];
        size_t rhl = sizeof(rh);
        CHECK(lev_identity_hash(remote, rh, sizeof(rh), &rhl) == LEV_OK);
        CHECK(memcmp(rh, bident_hash, LEV_ADDR_LEN) == 0);
        lev_identity_free(remote);
    }
    lev_identity_free(bident);

    /* Tear down. */
    CHECK(lev_close_link(lb, 2000) == LEV_OK);
    lev_link_free(lb);
    lev_link_free(la);
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
