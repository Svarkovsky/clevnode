/*
 * Leviculum C API example and acceptance test: programmatic radio interfaces.
 *
 * Exercises the RNode and serial builder surface (lev_builder_add_rnode,
 * lev_builder_add_serial): argument guards, and a node brought up over a
 * serial interface backed by a pseudo-terminal (a serial port is raw KISS with
 * no link-up handshake, so it comes up over a bare pty). RNode needs the
 * CMD_DETECT handshake of a real or proxied device, so it is exercised in the
 * reticulum-integ LoRa tier rather than here.
 *
 * Links the real libleviculum.so. Returns 0 on success. Compiled and run by
 * the Rust harness in tests/ffi_c_tests.rs.
 */

#define _GNU_SOURCE

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

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

static void test_argument_guards(void) {
    lev_builder_t *b = lev_builder_new();
    CHECK(b != NULL);
    /* NULL device path / parity is rejected; the builder stays usable. */
    CHECK(lev_builder_add_rnode(b, NULL, 867200000, 125000, 8, 5, 0) ==
          LEV_ERR_INVALID_ARG);
    CHECK(lev_builder_add_serial(b, NULL, 115200, 8, "N", 1) ==
          LEV_ERR_INVALID_ARG);
    CHECK(lev_builder_add_serial(b, "/dev/null", 115200, 8, NULL, 1) ==
          LEV_ERR_INVALID_ARG);
    /* Valid argument shapes are accepted (no device opened until build). */
    CHECK(lev_builder_add_rnode(b, "/dev/null", 867200000, 125000, 8, 5, 0) ==
          LEV_OK);
    lev_builder_free(b);
}

static void test_serial_node_over_pty(void) {
    int master = posix_openpt(O_RDWR | O_NOCTTY);
    CHECK(master >= 0);
    CHECK(grantpt(master) == 0);
    CHECK(unlockpt(master) == 0);
    char slave[256];
    CHECK(ptsname_r(master, slave, sizeof(slave)) == 0);

    char dir[] = "/tmp/leviculum-c-radio-XXXXXX";
    CHECK(mkdtemp(dir) != NULL);

    lev_builder_t *b = lev_builder_new();
    CHECK(b != NULL);
    CHECK(lev_builder_storage_path(b, dir) == LEV_OK);
    CHECK(lev_builder_add_serial(b, slave, 115200, 8, "N", 1) == LEV_OK);

    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);
    CHECK(node != NULL);
    CHECK(lev_start(node) == LEV_OK);
    CHECK(lev_is_running(node) == 1);
    CHECK(lev_stop(node) == LEV_OK);
    lev_free(node);

    close(master);
}

int main(void) {
    printf("leviculum radio C acceptance test\n");
    test_argument_guards();
    test_serial_node_over_pty();

    if (failures == 0) {
        printf("OK\n");
        return 0;
    }
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
}
