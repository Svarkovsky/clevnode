/*
 * Leviculum C API example and acceptance test: identity cryptography.
 *
 * Exercises lev_identity_sign / _verify / _encrypt / _decrypt: the sign and
 * verify round-trip, a public-only identity verifying but not signing, and the
 * encrypt-to-public-key / decrypt-with-private-key round-trip. All keys and
 * payloads cross the boundary read(2) style.
 *
 * Links the real libleviculum.so. Returns 0 on success. Compiled and run by
 * the Rust harness in tests/ffi_c_tests.rs.
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

static void test_sign_verify(void) {
    lev_identity_t *id = lev_identity_generate();
    CHECK(id != NULL);

    const uint8_t msg[] = "sign me";
    size_t mlen = sizeof(msg) - 1;

    /* read(2) size query for the signature. */
    size_t need = 0;
    CHECK(lev_identity_sign(id, msg, mlen, NULL, 0, &need) ==
          LEV_ERR_BUFFER_TOO_SMALL);
    CHECK(need == 64);

    uint8_t sig[64];
    size_t slen = sizeof(sig);
    CHECK(lev_identity_sign(id, msg, mlen, sig, sizeof(sig), &slen) == LEV_OK);
    CHECK(slen == 64);

    CHECK(lev_identity_verify(id, msg, mlen, sig, slen) == 1);

    /* Tampered message does not verify. */
    const uint8_t bad[] = "sign ME";
    CHECK(lev_identity_verify(id, bad, sizeof(bad) - 1, sig, slen) == 0);

    /* A public-only identity verifies but cannot sign. */
    uint8_t pub[LEV_IDENTITY_KEY_LEN];
    size_t plen = sizeof(pub);
    CHECK(lev_identity_public_key(id, pub, sizeof(pub), &plen) == LEV_OK);
    lev_identity_t *pub_only = lev_identity_from_public_key(pub, plen);
    CHECK(pub_only != NULL);
    CHECK(lev_identity_verify(pub_only, msg, mlen, sig, slen) == 1);
    size_t n = 0;
    CHECK(lev_identity_sign(pub_only, msg, mlen, NULL, 0, &n) == LEV_ERR_CRYPTO);

    lev_identity_free(pub_only);
    lev_identity_free(id);
}

static void test_encrypt_decrypt(void) {
    lev_identity_t *id = lev_identity_generate();
    CHECK(id != NULL);

    const uint8_t plain[] = "secret payload";
    size_t plen = sizeof(plain) - 1;

    /* Encrypt to the public key (length first, then the bytes). */
    size_t clen = 0;
    CHECK(lev_identity_encrypt(id, plain, plen, NULL, 0, &clen) ==
          LEV_ERR_BUFFER_TOO_SMALL);
    CHECK(clen >= 96);
    uint8_t ct[256];
    CHECK(clen <= sizeof(ct));
    CHECK(lev_identity_encrypt(id, plain, plen, ct, sizeof(ct), &clen) == LEV_OK);

    /* Decrypt with the private key reproduces the plaintext. */
    uint8_t out[256];
    size_t olen = sizeof(out);
    CHECK(lev_identity_decrypt(id, ct, clen, out, sizeof(out), &olen) == LEV_OK);
    CHECK(olen == plen && memcmp(out, plain, plen) == 0);

    /* Garbage does not decrypt. */
    uint8_t junk[100] = {0};
    size_t jl = sizeof(out);
    CHECK(lev_identity_decrypt(id, junk, sizeof(junk), out, sizeof(out), &jl) ==
          LEV_ERR_CRYPTO);

    lev_identity_free(id);
}

int main(void) {
    printf("leviculum crypto C acceptance test\n");
    test_sign_verify();
    test_encrypt_decrypt();

    if (failures == 0) {
        printf("OK\n");
        return 0;
    }
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
}
