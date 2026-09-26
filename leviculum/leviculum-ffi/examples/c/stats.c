/*
 * Leviculum C API example and acceptance test: read-only diagnostics.
 *
 * Exercises lev_transport_stats: reading the transport counters and the
 * path-table size from a running node, the NULL-out-pointer skip, and the
 * NULL-node guard. The basis for an rnstatus-style tool in C.
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
    printf("leviculum stats C acceptance test\n");

    /* NULL node is rejected. */
    uint64_t scratch = 0;
    CHECK(lev_transport_stats(NULL, &scratch, NULL, NULL, NULL, NULL, NULL) ==
          LEV_ERR_NULL_PTR);

    char dir[] = "/tmp/leviculum-c-stats-XXXXXX";
    CHECK(mkdtemp(dir) != NULL);

    lev_builder_t *b = lev_builder_new();
    CHECK(b != NULL);
    CHECK(lev_builder_storage_path(b, dir) == LEV_OK);
    CHECK(lev_builder_enable_transport(b, 0) == LEV_OK);
    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);
    CHECK(node != NULL);
    CHECK(lev_start(node) == LEV_OK);

    /* Read every counter. A fresh node has no paths and no drops. */
    uint64_t sent = 9, received = 9, forwarded = 9, announces = 9, dropped = 9,
             paths = 9;
    CHECK(lev_transport_stats(node, &sent, &received, &forwarded, &announces,
                              &dropped, &paths) == LEV_OK);
    CHECK(paths == 0);
    CHECK(dropped == 0);
    printf("  sent=%llu received=%llu forwarded=%llu announces=%llu paths=%llu\n",
           (unsigned long long)sent, (unsigned long long)received,
           (unsigned long long)forwarded, (unsigned long long)announces,
           (unsigned long long)paths);

    /* All out-pointers NULL is a valid no-op. */
    CHECK(lev_transport_stats(node, NULL, NULL, NULL, NULL, NULL, NULL) ==
          LEV_OK);

    /* Path table: take a frozen snapshot, read entries by index, free it. A
     * bare node has no paths, so the snapshot is empty. */
    CHECK(lev_path_table_snapshot(NULL) == NULL);
    lev_path_table_t *table = lev_path_table_snapshot(node);
    CHECK(table != NULL);
    int n = lev_path_table_count(table);
    CHECK(n == 0);
    for (int i = 0; i < n; i++) {
        uint8_t dest[LEV_ADDR_LEN], next_hop[LEV_ADDR_LEN];
        uint8_t hops = 0;
        int has_next = 0;
        uint64_t iface = 0, expires = 0;
        CHECK(lev_path_table_entry(table, i, dest, &hops, next_hop, &has_next,
                                   &iface, &expires) == LEV_OK);
    }
    /* Out-of-range index is rejected. */
    CHECK(lev_path_table_entry(table, 0, NULL, NULL, NULL, NULL, NULL, NULL) ==
          LEV_ERR_INVALID_ARG);
    lev_path_table_free(table);
    lev_path_table_free(NULL); /* no-op */

    /* Interface stats: same snapshot pattern. The name is read(2) style, the
     * rest are scalars. A bare node has no interfaces. */
    CHECK(lev_interface_stats_snapshot(NULL) == NULL);
    lev_interface_stats_t *ifaces = lev_interface_stats_snapshot(node);
    CHECK(ifaces != NULL);
    int ic = lev_interface_stats_count(ifaces);
    CHECK(ic == 0);
    for (int i = 0; i < ic; i++) {
        size_t need = 0;
        lev_interface_stats_name(ifaces, i, NULL, 0, &need);
        char name[256];
        size_t nl = sizeof(name);
        CHECK(lev_interface_stats_name(ifaces, i, (uint8_t *)name, sizeof(name),
                                       &nl) == LEV_OK);
        int online = 0, is_local = 0;
        uint64_t rx = 0, tx = 0;
        CHECK(lev_interface_stats_entry(ifaces, i, &online, &is_local, &rx,
                                        &tx) == LEV_OK);
    }
    CHECK(lev_interface_stats_entry(ifaces, 0, NULL, NULL, NULL, NULL) ==
          LEV_ERR_INVALID_ARG);
    lev_interface_stats_free(ifaces);
    lev_interface_stats_free(NULL); /* no-op */

    CHECK(lev_stop(node) == LEV_OK);
    lev_free(node);

    if (failures == 0) {
        printf("OK\n");
        return 0;
    }
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
}
