/*
 * Leviculum C API example and acceptance test, phase a.
 *
 * Exercises the foundation surface: version, error strings, identity
 * lifecycle and key round-trip, the read(2) buffer protocol, and the node
 * instance lifecycle (build, start, stop, free) with no interfaces.
 *
 * Links the real libleviculum.so. Returns 0 on success, non-zero on the
 * first failed check. Compiled and run by the Rust harness in
 * tests/ffi_c_tests.rs.
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

static void test_version(void) {
    const char *vs = lev_version_string();
    CHECK(vs != NULL);
    CHECK(strlen(vs) > 0);
    uint32_t vn = lev_version_number();
    /* major.minor.patch packed; at least one component is non-zero. */
    CHECK(vn != 0);
    printf("  version: %s (0x%06x)\n", vs, vn);
}

static void test_error_strings(void) {
    CHECK(strcmp(lev_strerror(LEV_OK), "success") == 0);
    CHECK(strcmp(lev_strerror(LEV_ERR_NULL_PTR), "null pointer") == 0);
    CHECK(strcmp(lev_strerror(-9999), "unknown error") == 0);
    /* No failure recorded yet on this thread, or a stale one; either way the
     * pointer contract holds (NULL or a valid string). */
}

static void test_identity(void) {
    lev_identity_t *id = lev_identity_generate();
    CHECK(id != NULL);
    CHECK(lev_identity_has_private_keys(id) == 1);

    /* read(2) size query: NULL buffer returns the required length. */
    size_t need = 0;
    int rc = lev_identity_hash(id, NULL, 0, &need);
    CHECK(rc == LEV_ERR_BUFFER_TOO_SMALL);
    CHECK(need == LEV_ADDR_LEN);

    uint8_t hash[LEV_ADDR_LEN];
    size_t got = sizeof(hash);
    rc = lev_identity_hash(id, hash, sizeof(hash), &got);
    CHECK(rc == LEV_OK);
    CHECK(got == LEV_ADDR_LEN);

    /* Private key round-trip: rebuilding from the private key reproduces the
     * same identity hash. */
    uint8_t prv[LEV_IDENTITY_KEY_LEN];
    size_t prv_len = sizeof(prv);
    rc = lev_identity_private_key(id, prv, sizeof(prv), &prv_len);
    CHECK(rc == LEV_OK);
    CHECK(prv_len == LEV_IDENTITY_KEY_LEN);

    lev_identity_t *id2 = lev_identity_from_private_key(prv, prv_len);
    CHECK(id2 != NULL);
    uint8_t hash2[LEV_ADDR_LEN];
    size_t got2 = sizeof(hash2);
    CHECK(lev_identity_hash(id2, hash2, sizeof(hash2), &got2) == LEV_OK);
    CHECK(memcmp(hash, hash2, LEV_ADDR_LEN) == 0);

    /* Public-only identity has no private keys. */
    uint8_t pub[LEV_IDENTITY_KEY_LEN];
    size_t pub_len = sizeof(pub);
    CHECK(lev_identity_public_key(id, pub, sizeof(pub), &pub_len) == LEV_OK);
    lev_identity_t *pub_only = lev_identity_from_public_key(pub, pub_len);
    CHECK(pub_only != NULL);
    CHECK(lev_identity_has_private_keys(pub_only) == 0);
    CHECK(lev_identity_private_key(pub_only, prv, sizeof(prv), &prv_len) ==
          LEV_ERR_CRYPTO);

    /* Wrong key length is rejected. */
    CHECK(lev_identity_from_private_key(prv, 10) == NULL);

    lev_identity_free(pub_only);
    lev_identity_free(id2);
    lev_identity_free(id);
    lev_identity_free(NULL); /* no-op */
}

static void test_identity_file(void) {
    lev_identity_t *id = lev_identity_generate();
    CHECK(id != NULL);
    const char *path = "/tmp/leviculum-c-identity-file";
    CHECK(lev_identity_save_file(id, path) == LEV_OK);

    lev_identity_t *loaded = lev_identity_load_file(path);
    CHECK(loaded != NULL);
    CHECK(lev_identity_has_private_keys(loaded) == 1);

    uint8_t h1[LEV_ADDR_LEN], h2[LEV_ADDR_LEN];
    size_t l1 = sizeof(h1), l2 = sizeof(h2);
    CHECK(lev_identity_hash(id, h1, sizeof(h1), &l1) == LEV_OK);
    CHECK(lev_identity_hash(loaded, h2, sizeof(h2), &l2) == LEV_OK);
    CHECK(memcmp(h1, h2, LEV_ADDR_LEN) == 0);

    lev_identity_free(loaded);
    lev_identity_free(id);

    /* Missing file yields NULL. */
    CHECK(lev_identity_load_file("/tmp/leviculum-no-such-identity-xyz") == NULL);
}

static void test_null_guards(void) {
    uint8_t buf[LEV_ADDR_LEN];
    size_t len = sizeof(buf);
    CHECK(lev_identity_hash(NULL, buf, sizeof(buf), &len) == LEV_ERR_NULL_PTR);
    CHECK(lev_identity_has_private_keys(NULL) == 0);
    CHECK(lev_is_running(NULL) == 0);
    CHECK(lev_start(NULL) == LEV_ERR_NULL_PTR);
}

static void test_hex(void) {
    const uint8_t bytes[] = {0x00, 0x1f, 0xab, 0xff};
    uint8_t hex[8];
    size_t hl = sizeof(hex);
    CHECK(lev_hex_encode(bytes, 4, hex, sizeof(hex), &hl) == LEV_OK);
    CHECK(hl == 8 && memcmp(hex, "001fabff", 8) == 0);

    uint8_t back[4];
    size_t bl = sizeof(back);
    CHECK(lev_hex_decode(hex, 8, back, sizeof(back), &bl) == LEV_OK);
    CHECK(bl == 4 && memcmp(back, bytes, 4) == 0);

    /* Size query and error cases. */
    size_t need = 0;
    CHECK(lev_hex_encode(bytes, 4, NULL, 0, &need) == LEV_ERR_BUFFER_TOO_SMALL);
    CHECK(need == 8);
    CHECK(lev_hex_decode((const uint8_t *)"zz", 2, back, sizeof(back), &bl) ==
          LEV_ERR_INVALID_ARG);
    CHECK(lev_hex_decode((const uint8_t *)"abc", 3, back, sizeof(back), &bl) ==
          LEV_ERR_INVALID_ARG);
}

static volatile int log_count = 0;
static int log_last_level = 0;

static void log_cb(int level, const char *msg, void *user) {
    (void)msg;
    int *cnt = (int *)user;
    (*cnt)++;
    log_last_level = level;
}

static void test_logging(void) {
    CHECK(lev_init() == LEV_OK);
    CHECK(lev_init() == LEV_OK); /* idempotent */

    CHECK(lev_log_set_callback(log_cb, (void *)&log_count) == LEV_OK);
    CHECK(lev_log_set_level(LEV_LOG_INFO) == LEV_OK);
    CHECK(lev_log_set_level(9999) == LEV_ERR_INVALID_ARG);

    /* Building and starting a node logs at info on the calling thread, so the
     * sink must fire at least once. */
    lev_builder_t *b = lev_builder_new();
    CHECK(b != NULL);
    CHECK(lev_builder_storage_path(b, "/tmp/leviculum-c-phase-a-log") == LEV_OK);
    CHECK(lev_builder_enable_transport(b, 0) == LEV_OK);
    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);
    CHECK(node != NULL);
    CHECK(lev_start(node) == LEV_OK);
    CHECK(lev_stop(node) == LEV_OK);
    lev_free(node);

    CHECK(log_count > 0);
    CHECK(log_last_level >= LEV_LOG_ERROR && log_last_level <= LEV_LOG_INFO);

    /* Detach the sink and silence so later tests do not accumulate. */
    CHECK(lev_log_set_callback(NULL, NULL) == LEV_OK);
    CHECK(lev_log_set_level(LEV_LOG_OFF) == LEV_OK);
}

static void test_node_lifecycle(void) {
    lev_builder_t *b = lev_builder_new();
    CHECK(b != NULL);
    CHECK(lev_builder_storage_path(b, "/tmp/leviculum-c-phase-a") == LEV_OK);
    CHECK(lev_builder_enable_transport(b, 0) == LEV_OK);

    leviculum_t *node = lev_builder_build(b);
    CHECK(node != NULL);

    /* Builder is emptied by build; a second build fails but the handle is
     * still ours to free. */
    CHECK(lev_builder_build(b) == NULL);
    lev_builder_free(b);

    CHECK(lev_is_running(node) == 0);
    CHECK(lev_start(node) == LEV_OK);
    CHECK(lev_is_running(node) == 1);

    /* Event fd is valid; with no peer the queue drains cleanly. */
    CHECK(lev_event_fd(node) >= 0);
    lev_event_t *ev = NULL;
    CHECK(lev_next_event(node, &ev) == LEV_OK);
    if (ev) {
        lev_event_free(ev);
    }
    ev = NULL;
    CHECK(lev_wait_event(node, &ev, 20) == LEV_OK); /* OK with NULL on timeout */
    if (ev) {
        lev_event_free(ev);
    }
    CHECK(lev_event_fd(NULL) < 0);

    uint8_t self_hash[LEV_ADDR_LEN];
    size_t hl = sizeof(self_hash);
    CHECK(lev_identity_hash_self(node, self_hash, sizeof(self_hash), &hl) ==
          LEV_OK);
    CHECK(hl == LEV_ADDR_LEN);

    CHECK(lev_stop(node) == LEV_OK);
    CHECK(lev_is_running(node) == 0);

    /* Restart: stop then start must bring the node back up (the engine
     * rebuilds its runtime on start). */
    CHECK(lev_start(node) == LEV_OK);
    CHECK(lev_is_running(node) == 1);
    CHECK(lev_stop(node) == LEV_OK);
    CHECK(lev_is_running(node) == 0);

    lev_free(node);
    lev_free(NULL); /* no-op */
}

int main(void) {
    printf("leviculum phase a C acceptance test\n");
    test_version();
    test_error_strings();
    test_identity();
    test_identity_file();
    test_null_guards();
    test_hex();
    test_logging();
    test_node_lifecycle();

    if (failures == 0) {
        printf("OK\n");
        return 0;
    }
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
}
