/*
 * Leviculum C API example and acceptance test: destination ratchets.
 *
 * Exercises lev_destination_enable_ratchets and lev_destination_ratchet_public:
 * enabling forward secrecy on an inbound destination, the outbound rejection,
 * and reading the current ratchet public key after registration.
 *
 * Links the real libleviculum.so. Returns 0 on success. Compiled and run by
 * the Rust harness in tests/ffi_c_tests.rs.
 */

#include <stdio.h>
#include <stdlib.h>
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

int main(void) {
    printf("leviculum ratchet C acceptance test\n");

    char dir[] = "/tmp/leviculum-c-ratchet-XXXXXX";
    CHECK(mkdtemp(dir) != NULL);

    lev_identity_t *id = lev_identity_generate();
    CHECK(id != NULL);

    lev_builder_t *b = lev_builder_new();
    CHECK(b != NULL);
    CHECK(lev_builder_storage_path(b, dir) == LEV_OK);
    CHECK(lev_builder_identity(b, id) == LEV_OK);
    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);
    CHECK(node != NULL);
    CHECK(lev_start(node) == LEV_OK);

    const char *aspects[] = {"ratchet"};

    /* Outbound destinations cannot ratchet. */
    lev_destination_t *out =
        lev_destination_new(id, LEV_DIRECTION_OUT, LEV_DEST_SINGLE, "app",
                            aspects, 1);
    CHECK(out != NULL);
    CHECK(lev_destination_enable_ratchets(out, 1700000000000ULL) ==
          LEV_ERR_INVALID_ARG);
    lev_destination_free(out);

    /* Inbound destination: enable ratchets before registering. */
    lev_destination_t *in =
        lev_destination_new(id, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "app",
                            aspects, 1);
    CHECK(in != NULL);
    CHECK(lev_destination_enable_ratchets(in, 1700000000000ULL) == LEV_OK);

    uint8_t dh[LEV_ADDR_LEN];
    size_t dhl = sizeof(dh);
    CHECK(lev_destination_hash(in, dh, sizeof(dh), &dhl) == LEV_OK);
    CHECK(lev_register_destination(node, in) == LEV_OK);
    lev_destination_free(in);

    /* The current ratchet public key is readable (32 bytes, not all zero). */
    size_t need = 0;
    CHECK(lev_destination_ratchet_public(node, dh, NULL, 0, &need) ==
          LEV_ERR_BUFFER_TOO_SMALL);
    CHECK(need == 32);
    uint8_t key[32];
    size_t kl = sizeof(key);
    CHECK(lev_destination_ratchet_public(node, dh, key, sizeof(key), &kl) ==
          LEV_OK);
    CHECK(kl == 32);
    int nonzero = 0;
    for (size_t i = 0; i < kl; i++) {
        if (key[i] != 0) {
            nonzero = 1;
        }
    }
    CHECK(nonzero);

    CHECK(lev_stop(node) == LEV_OK);
    lev_free(node);
    lev_identity_free(id);

    if (failures == 0) {
        printf("OK\n");
        return 0;
    }
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
}
