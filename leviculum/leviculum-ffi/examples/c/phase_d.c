/*
 * Leviculum C API example and acceptance test, phase d.
 *
 * Datagram and request/response between two nodes over TCP loopback. Node B
 * learns node A from an announce, sends a datagram that A receives, then opens
 * a link and sends a request to a path A handles, getting a response back.
 *
 * Request and response payloads are msgpack-encoded values; this test uses
 * fixstr values and compares the raw bytes round-trip.
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

/* Wait up to `rounds` * 200ms for an event of `want` type on `n`, returning the
 * matched event (caller frees) or NULL. Non-matching events are drained. */
static lev_event_t *wait_for_ev(leviculum_t *n, int want, int rounds) {
    for (int r = 0; r < rounds; r++) {
        lev_event_t *ev = NULL;
        if (lev_wait_event(n, &ev, 200) != LEV_OK) {
            return NULL;
        }
        if (!ev) {
            continue;
        }
        if (lev_event_type(ev) == want) {
            return ev;
        }
        lev_event_free(ev);
    }
    return NULL;
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

    printf("leviculum phase d C acceptance test\n");
    CHECK(lev_init() == LEV_OK);

    /* Node A: TCP server, identity, incoming destination, request handler. */
    lev_identity_t *ida = lev_identity_generate();
    lev_builder_t *ba = lev_builder_new();
    CHECK(lev_builder_identity(ba, ida) == LEV_OK);
    CHECK(lev_builder_storage_path(ba, "/tmp/leviculum-c-phase-d-a") == LEV_OK);
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

    /* Node B: TCP client. */
    lev_builder_t *bb = lev_builder_new();
    CHECK(lev_builder_storage_path(bb, "/tmp/leviculum-c-phase-d-b") == LEV_OK);
    CHECK(lev_builder_add_tcp_client(bb, bound) == LEV_OK);
    leviculum_t *b = lev_builder_build(bb);
    lev_builder_free(bb);
    CHECK(b != NULL);
    CHECK(lev_start(b) == LEV_OK);

    const char *aspects[] = {"msg"};
    lev_destination_t *dest = lev_destination_new(
        ida, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "leviculum-demo", aspects, 1);
    CHECK(dest != NULL);
    uint8_t dh[LEV_ADDR_LEN];
    size_t dhl = sizeof(dh);
    CHECK(lev_destination_hash(dest, dh, sizeof(dh), &dhl) == LEV_OK);
    CHECK(lev_register_destination(a, dest) == LEV_OK);
    lev_destination_free(dest);

    /* A handles requests to "/echo" from anyone. */
    CHECK(lev_register_request_handler(a, dh, "/echo",
                                       LEV_REQUEST_POLICY_ALLOW_ALL, NULL,
                                       0) == LEV_OK);

    /* B learns A. */
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

    /* Datagram: B -> A, A receives it as a packet event. */
    const uint8_t dgram[] = {'h', 'i'};
    uint8_t phash[LEV_ADDR_LEN];
    CHECK(lev_send_datagram(b, dh, dgram, sizeof(dgram), phash, 3000) == LEV_OK);
    lev_event_t *pe = wait_for_ev(a, LEV_EVENT_PACKET_RECEIVED, 50);
    CHECK(pe != NULL);
    if (pe) {
        uint8_t rx[16];
        size_t rxl = sizeof(rx);
        CHECK(lev_event_data(pe, rx, sizeof(rx), &rxl) == LEV_OK);
        CHECK(rxl == sizeof(dgram) && memcmp(rx, dgram, sizeof(dgram)) == 0);
        lev_event_free(pe);
    }

    /* Link: B connects, the core auto-accepts on A (Python-RNS parity), so A
     * sees an inbound LINK_ESTABLISHED; mint its handle. B sees its own. */
    lev_link_t *lb = NULL;
    CHECK(lev_connect(b, dh, 5000, &lb) == LEV_OK);
    CHECK(lb != NULL);
    lev_event_t *lr = wait_for_ev(a, LEV_EVENT_LINK_ESTABLISHED, 50);
    CHECK(lr != NULL);
    lev_link_t *la = NULL;
    if (lr) {
        uint8_t lid[LEV_ADDR_LEN];
        size_t lidl = sizeof(lid);
        CHECK(lev_event_link_id(lr, lid, sizeof(lid), &lidl) == LEV_OK);
        lev_event_free(lr);
        CHECK(lev_accept_link(a, lid, 5000, &la) == LEV_OK);
        CHECK(la != NULL);
    }
    lev_event_t *est = wait_for_ev(b, LEV_EVENT_LINK_ESTABLISHED, 50);
    CHECK(est != NULL);
    if (est) {
        lev_event_free(est);
    }

    /* Request/response over the link. */
    uint8_t lb_id[LEV_ADDR_LEN];
    size_t lb_idl = sizeof(lb_id);
    CHECK(lev_link_id(lb, lb_id, sizeof(lb_id), &lb_idl) == LEV_OK);

    const uint8_t req[] = {0xA4, 'p', 'i', 'n', 'g'};  /* msgpack "ping" */
    const uint8_t resp[] = {0xA4, 'p', 'o', 'n', 'g'}; /* msgpack "pong" */
    uint8_t req_id[LEV_ADDR_LEN];
    CHECK(lev_send_request(b, lb_id, "/echo", req, sizeof(req), 5000, req_id) ==
          LEV_OK);

    /* A receives the request, checks path and data, responds. */
    lev_event_t *rr = wait_for_ev(a, LEV_EVENT_REQUEST_RECEIVED, 50);
    CHECK(rr != NULL);
    if (rr) {
        uint8_t path[32];
        size_t pl = sizeof(path);
        CHECK(lev_event_path(rr, path, sizeof(path), &pl) == LEV_OK);
        CHECK(pl == 5 && memcmp(path, "/echo", 5) == 0);
        uint8_t rdata[16];
        size_t rdl = sizeof(rdata);
        CHECK(lev_event_data(rr, rdata, sizeof(rdata), &rdl) == LEV_OK);
        CHECK(rdl == sizeof(req) && memcmp(rdata, req, sizeof(req)) == 0);
        uint8_t a_link[LEV_ADDR_LEN];
        size_t all = sizeof(a_link);
        CHECK(lev_event_link_id(rr, a_link, sizeof(a_link), &all) == LEV_OK);
        uint8_t got_id[LEV_ADDR_LEN];
        size_t gil = sizeof(got_id);
        CHECK(lev_event_request_id(rr, got_id, sizeof(got_id), &gil) == LEV_OK);
        lev_event_free(rr);
        CHECK(lev_send_response(a, a_link, got_id, resp, sizeof(resp), 3000) ==
              LEV_OK);
    }

    /* B receives the response matching its request id. */
    lev_event_t *re = wait_for_ev(b, LEV_EVENT_RESPONSE_RECEIVED, 50);
    CHECK(re != NULL);
    if (re) {
        uint8_t got_id[LEV_ADDR_LEN];
        size_t gil = sizeof(got_id);
        CHECK(lev_event_request_id(re, got_id, sizeof(got_id), &gil) == LEV_OK);
        CHECK(memcmp(got_id, req_id, LEV_ADDR_LEN) == 0);
        uint8_t rdata[16];
        size_t rdl = sizeof(rdata);
        CHECK(lev_event_data(re, rdata, sizeof(rdata), &rdl) == LEV_OK);
        CHECK(rdl == sizeof(resp) && memcmp(rdata, resp, sizeof(resp)) == 0);
        lev_event_free(re);
    }

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
