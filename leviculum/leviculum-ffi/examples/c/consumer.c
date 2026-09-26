/*
 * Leviculum C API: a third-party consumer built against the INSTALLED library.
 *
 * Unlike the phase_*.c examples (which link the build-tree static archive
 * directly), this one is compiled and linked the way an external application
 * would after `make install`, purely through pkg-config:
 *
 *   cc consumer.c $(pkg-config --cflags --libs leviculum) -o consumer
 *   ./consumer
 *
 * It exercises the install end to end: the header is found via the pkg-config
 * -I, the symbol resolves against libleviculum.so.0 (the SONAME), and the
 * shared library actually loads and runs. `scripts/verify-packaging.sh` drives
 * this against a staged install. Kept dependency-light (no network) so it is a
 * pure "the installed library works" smoke test.
 */

#include <stdio.h>

#include "leviculum.h"

int main(void) {
    printf("leviculum %s (0x%06x)\n", lev_version_string(), lev_version_number());

    /* Exercise a real handle through the shared object: allocate and free a
     * builder. This runs the library's one-time init and the allocator across
     * the FFI boundary, not just a constant string accessor. */
    lev_builder_t *b = lev_builder_new();
    if (!b) {
        fprintf(stderr, "lev_builder_new failed\n");
        return 1;
    }
    lev_builder_free(b);

    printf("builder create/free: ok\n");
    return 0;
}
