/*
 * Leviculum C API example: a minimal Reticulum daemon in C.
 *
 * Loads an RNS-style config directory (the same layout rnsd/lnsd use), brings
 * up the stack, and runs until SIGINT or SIGTERM. If the config enables a
 * shared instance it also serves the rnstatus/rnpath/rnprobe RPC, so standard
 * Reticulum tools drive this daemon over `docker exec` or a local socket. This
 * is the C node used as the `c-api` type in the reticulum-integ mesh.
 *
 * Usage: c-lnsd [--config <dir>] [-v|-vv|-q|-qq]   (config default: ./.reticulum)
 *
 * Build (static, self-contained), see the `build-c-lnsd` Just recipe:
 *   cargo build-ffi --release
 *   cc lnsd.c target/<triple>/release/libleviculum.a -lpthread -ldl -lm \
 *      -I leviculum-ffi -o c-lnsd
 */

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "leviculum.h"

static volatile sig_atomic_t running = 1;

static void on_signal(int sig) {
    (void)sig;
    running = 0;
}

static void log_sink(int level, const char *msg, void *user) {
    (void)level;
    (void)user;
    fprintf(stderr, "%s\n", msg);
}

int main(int argc, char **argv) {
    const char *config_dir = "./.reticulum";
    int level = LEV_LOG_INFO;
    for (int i = 1; i < argc; i++) {
        if ((strcmp(argv[i], "--config") == 0 || strcmp(argv[i], "-c") == 0) &&
            i < argc - 1) {
            config_dir = argv[++i];
        } else if (strcmp(argv[i], "-v") == 0) {
            level = LEV_LOG_DEBUG;
        } else if (strcmp(argv[i], "-vv") == 0) {
            level = LEV_LOG_TRACE;
        } else if (strcmp(argv[i], "-q") == 0) {
            level = LEV_LOG_WARN;
        } else if (strcmp(argv[i], "-qq") == 0) {
            level = LEV_LOG_ERROR;
        }
    }

    char config_path[1024];
    snprintf(config_path, sizeof(config_path), "%s/config", config_dir);
    /* Match rnsd/lnsd: state lives under <config>/storage, not <config>. */
    char storage_path[1024];
    snprintf(storage_path, sizeof(storage_path), "%s/storage", config_dir);

    lev_init();
    lev_log_set_callback(log_sink, NULL);
    lev_log_set_level(level);

    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);

    lev_builder_t *b = lev_builder_new();
    if (!b) {
        fprintf(stderr, "c-lnsd: out of memory\n");
        return 1;
    }
    int rc = lev_builder_storage_path(b, storage_path);
    if (rc == LEV_OK) {
        rc = lev_builder_config_file(b, config_path);
    }
    if (rc != LEV_OK) {
        fprintf(stderr, "c-lnsd: config %s: %s\n", config_path,
                lev_strerror(rc));
        lev_builder_free(b);
        return 1;
    }

    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);
    if (!node) {
        const char *err = lev_last_error();
        fprintf(stderr, "c-lnsd: build failed: %s\n", err ? err : "unknown");
        return 1;
    }

    rc = lev_start(node);
    if (rc != LEV_OK) {
        fprintf(stderr, "c-lnsd: start failed: %s\n", lev_strerror(rc));
        lev_free(node);
        return 1;
    }

    fprintf(stderr, "c-lnsd: running, config %s\n", config_path);

    /* Drain events so the queue never blocks the engine, sleeping otherwise. */
    struct timespec idle = {.tv_sec = 0, .tv_nsec = 100 * 1000 * 1000};
    while (running) {
        lev_event_t *ev = NULL;
        if (lev_wait_event(node, &ev, 100) == LEV_OK && ev) {
            lev_event_free(ev);
        } else {
            nanosleep(&idle, NULL);
        }
    }

    fprintf(stderr, "c-lnsd: shutting down\n");
    lev_stop(node);
    lev_free(node);
    return 0;
}
