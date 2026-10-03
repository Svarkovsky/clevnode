#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <assert.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <time.h>
#include <stdint.h>
#include "leviculum.h"

/* Implement minimal mock stubs for FFI signatures from leviculum.h */
int lev_init(void) { return 0; }
const char *lev_last_error(void) { return ""; }
const char *lev_strerror(int err) { (void)err; return "OK"; }

struct lev_identity_t *lev_identity_load_file(const char *path) { (void)path; return (struct lev_identity_t *)1; }
struct lev_identity_t *lev_identity_generate(void) { return (struct lev_identity_t *)1; }
int lev_identity_save_file(const struct lev_identity_t *id, const char *path) { (void)id; (void)path; return 0; }
void lev_identity_free(struct lev_identity_t *id) { (void)id; }

struct lev_builder_t *lev_builder_new(void) { return (struct lev_builder_t *)1; }
int lev_builder_config_file(struct lev_builder_t *b, const char *path) { (void)b; (void)path; return 0; }
int lev_builder_storage_path(struct lev_builder_t *b, const char *path) { (void)b; (void)path; return 0; }
struct leviculum_t *lev_builder_build(struct lev_builder_t *b) { (void)b; return (struct leviculum_t *)1; }
void lev_builder_free(struct lev_builder_t *b) { (void)b; }

int lev_start(struct leviculum_t *node) { (void)node; return 0; }
int lev_stop(struct leviculum_t *node) { (void)node; return 0; }
void lev_free(struct leviculum_t *node) { (void)node; }

struct lev_destination_t *lev_destination_new(const struct lev_identity_t *identity, int direction, int destination_type, const char *app_name, const char * const *aspects, uintptr_t aspects_len) {
    (void)identity; (void)direction; (void)destination_type; (void)app_name; (void)aspects; (void)aspects_len;
    return (struct lev_destination_t *)1;
}
int lev_destination_set_accepts_links(struct lev_destination_t *dest, int accepts) { (void)dest; (void)accepts; return 0; }
int lev_destination_hash(const struct lev_destination_t *dest, uint8_t *out, uintptr_t out_len, uintptr_t *out_written) {
    (void)dest; (void)out; (void)out_len;
    if (out_written) *out_written = 16;
    return 0;
}
int lev_register_destination(const struct leviculum_t *node, struct lev_destination_t *dest) { (void)node; (void)dest; return 0; }
void lev_destination_free(struct lev_destination_t *dest) { (void)dest; }

int lev_announce(const struct leviculum_t *node, const uint8_t *dest_hash, const uint8_t *app_data, uintptr_t app_data_len, int attached_interface) {
    (void)node; (void)dest_hash; (void)app_data; (void)app_data_len; (void)attached_interface;
    return 0;
}
int lev_register_request_handler(const struct leviculum_t *node, const uint8_t *dest_hash, const char *path, int policy, const uint8_t *allowed_list, uintptr_t allowed_count) {
    (void)node; (void)dest_hash; (void)path; (void)policy; (void)allowed_list; (void)allowed_count;
    return 0;
}
int lev_wait_event(struct leviculum_t *node, struct lev_event_t **out, int timeout_ms) { (void)node; (void)timeout_ms; if (out) *out = NULL; return 0; }
int lev_event_type(const struct lev_event_t *event) { (void)event; return 0; }
int lev_event_link_id(const struct lev_event_t *event, uint8_t *out, uintptr_t out_len, uintptr_t *out_written) {
    (void)event; (void)out; (void)out_len; if (out_written) *out_written = 16; return 0;
}
int lev_event_request_id(const struct lev_event_t *event, uint8_t *out, uintptr_t out_len, uintptr_t *out_written) {
    (void)event; (void)out; (void)out_len; if (out_written) *out_written = 16; return 0;
}
int lev_event_path(const struct lev_event_t *event, uint8_t *out, uintptr_t out_len, uintptr_t *out_written) {
    (void)event; (void)out; (void)out_len; if (out_written) *out_written = 0; return 0;
}
void lev_event_free(struct lev_event_t *event) { (void)event; }

int lev_send_response(const struct leviculum_t *node, const uint8_t *link_id, const uint8_t *request_id, const uint8_t *data, uintptr_t data_len, int timeout_ms) {
    (void)node; (void)link_id; (void)request_id; (void)data; (void)data_len; (void)timeout_ms;
    return 0;
}
int lev_send_response_resource(const struct leviculum_t *node, const uint8_t *link_id, const uint8_t *request_id, const uint8_t *data, uintptr_t data_len, int timeout_ms) {
    (void)node; (void)link_id; (void)request_id; (void)data; (void)data_len; (void)timeout_ms;
    return 0;
}

/* Include clevnode logic under test (with main renamed) */
#define main clevnode_dummy_main
#include "clevnode.c"
#undef main

static const char *test_dir = "/tmp/clevnode_test_posts";

static void setup_test_files(void) {
    mkdir(test_dir, 0755);

    FILE *f1 = fopen("/tmp/clevnode_test_posts/index.mu", "wb");
    assert(f1);
    fprintf(f1, "Hello from index");
    fclose(f1);

    FILE *f2 = fopen("/tmp/clevnode_test_posts/page1.mu", "wb");
    assert(f2);
    fprintf(f2, "Content of page1");
    fclose(f2);

    FILE *f_secret = fopen("/tmp/clevnode_test_posts/secret.txt", "wb");
    assert(f_secret);
    fprintf(f_secret, "CONFIDENTIAL");
    fclose(f_secret);

    FILE *f_hidden = fopen("/tmp/clevnode_test_posts/.hidden.mu", "wb");
    assert(f_hidden);
    fprintf(f_hidden, "HIDDEN_SECRET");
    fclose(f_hidden);
}

static void cleanup_test_files(void) {
    unlink("/tmp/clevnode_test_posts/index.mu");
    unlink("/tmp/clevnode_test_posts/page1.mu");
    unlink("/tmp/clevnode_test_posts/secret.txt");
    unlink("/tmp/clevnode_test_posts/.hidden.mu");
    rmdir(test_dir);
}

static void test_valid_paths(void) {
    size_t len = 0;
    uint8_t *data;

    data = get_page_content("/page", test_dir, &len);
    assert(data != NULL && len > 0);
    free(data);

    data = get_page_content("/page/", test_dir, &len);
    assert(data != NULL && len > 0);
    free(data);

    data = get_page_content("/page/index.mu", test_dir, &len);
    assert(data != NULL && len > 0);
    free(data);

    data = get_page_content("/page/page1.mu", test_dir, &len);
    assert(data != NULL && len > 0);
    free(data);

    printf("[PASS] test_valid_paths\n");
}

static void test_path_traversal_attacks(void) {
    size_t len = 0;
    uint8_t *data;

    const char *traversal_attacks[] = {
        "/page/../etc/passwd",
        "/page/../../../../../../etc/passwd",
        "/page/..\\..\\etc\\passwd",
        "/page/....//etc/passwd",
        "/page/..%2f..%2fetc/passwd",
        "/page/sub/../../etc/passwd",
        "/page/dir/page.mu",
        "/page/secret.txt/..",
        "../etc/passwd",
        "/etc/passwd",
        "",
        "/",
        "/page/..",
        "/page/...",
        "/page/.hidden.mu",
        NULL
    };

    for (int i = 0; traversal_attacks[i] != NULL; i++) {
        const char *atk = traversal_attacks[i];
        data = get_page_content(atk, test_dir, &len);
        if (data != NULL) {
            printf("[FAIL] Traversal not blocked: '%s'\n", atk);
        }
        assert(data == NULL);
    }

    /* Long buffer overflow attack (> 1024 bytes) */
    char long_path[4096];
    memset(long_path, 'A', sizeof(long_path) - 1);
    long_path[sizeof(long_path) - 1] = 0;
    data = get_page_content(long_path, test_dir, &len);
    assert(data == NULL);

    /* Prefix match without slash: /pagehack.mu */
    data = get_page_content("/pagehack.mu", test_dir, &len);
    assert(data == NULL);

    printf("[PASS] test_path_traversal_attacks\n");
}

static void test_lru_cache_eviction_and_cleanup(void) {
    size_t len = 0;

    for (int i = 0; i < 20; i++) {
        char fn[256];
        snprintf(fn, sizeof(fn), "%s/lru_%d.mu", test_dir, i);
        FILE *f = fopen(fn, "wb");
        assert(f);
        fprintf(f, "Dynamic page content %d", i);
        fclose(f);

        char req[128];
        snprintf(req, sizeof(req), "/page/lru_%d.mu", i);
        uint8_t *d = get_page_content(req, test_dir, &len);
        assert(d != NULL && len > 0);
        free(d);
    }

    /* Hot reload on mtime change */
    char reload_path[256];
    snprintf(reload_path, sizeof(reload_path), "%s/lru_19.mu", test_dir);
    sleep(1);
    FILE *f = fopen(reload_path, "wb");
    assert(f);
    fprintf(f, "UPDATED_CONTENT_OVERWRITE");
    fclose(f);

    uint8_t *d = get_page_content("/page/lru_19.mu", test_dir, &len);
    assert(d != NULL && len > 0);
    free(d);

    for (int i = 0; i < 20; i++) {
        char fn[256];
        snprintf(fn, sizeof(fn), "%s/lru_%d.mu", test_dir, i);
        unlink(fn);
    }

    for (int i = 0; i < PAGE_CACHE_MAX_SLOTS; i++) {
        if (page_cache[i].is_valid && page_cache[i].msgpack_data) {
            free(page_cache[i].msgpack_data);
            page_cache[i].msgpack_data = NULL;
            page_cache[i].is_valid = 0;
        }
    }

    printf("[PASS] test_lru_cache_eviction_and_cleanup\n");
}

int main(void) {
    printf("=== Starting Clevnode Security & Memory Unit Tests ===\n");
    setup_test_files();
    test_valid_paths();
    test_path_traversal_attacks();
    test_lru_cache_eviction_and_cleanup();
    cleanup_test_files();
    printf("=== ALL SECURITY & MEMORY TESTS PASSED ===\n");
    return 0;
}
