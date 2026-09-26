/*
 * Leviculum C API example and acceptance test: delivery proof strategies.
 *
 * Exercises lev_destination_set_proof_strategy and the lev_send_proof guards.
 * The full App-strategy flow (a PACKET_PROOF_REQUESTED event answered by
 * lev_send_proof) is a two-node exchange covered by the Rust integration
 * tests; here we validate the strategy setter and argument handling from C.
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
    printf("leviculum proof C acceptance test\n");

    lev_identity_t *id = lev_identity_generate();
    CHECK(id != NULL);
    const char *aspects[] = {"proof"};

    lev_destination_t *dest = lev_destination_new(
        id, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "app", aspects, 1);
    CHECK(dest != NULL);

    /* Each strategy is accepted; a bogus value is rejected. */
    CHECK(lev_destination_set_proof_strategy(dest, LEV_PROOF_NONE) == LEV_OK);
    CHECK(lev_destination_set_proof_strategy(dest, LEV_PROOF_APP) == LEV_OK);
    CHECK(lev_destination_set_proof_strategy(dest, LEV_PROOF_ALL) == LEV_OK);
    CHECK(lev_destination_set_proof_strategy(dest, 99) == LEV_ERR_INVALID_ARG);
    CHECK(lev_destination_set_proof_strategy(NULL, LEV_PROOF_APP) ==
          LEV_ERR_NULL_PTR);

    lev_destination_free(dest);
    lev_identity_free(id);

    /* lev_send_proof rejects NULL arguments. */
    uint8_t hash[LEV_ADDR_LEN] = {0};
    uint8_t phash[32] = {0};
    CHECK(lev_send_proof(NULL, hash, phash, 1000) == LEV_ERR_NULL_PTR);

    if (failures == 0) {
        printf("OK\n");
        return 0;
    }
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
}
