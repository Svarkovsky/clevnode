/*
 * Leviculum C API example and acceptance test: run as or with a daemon.
 *
 * Exercises the config-file and shared-instance builder surface added in the
 * roadmap "daemon" phase: lev_builder_config_file, lev_builder_share_instance,
 * lev_builder_connect_shared_instance. Covers argument guards, a real
 * config-file driven node coming up, and a node offering a shared instance
 * that a second node attaches to as a local client.
 *
 * Takes the config-file node's TCP listen port as argv[1]. `0` is the
 * intended form: the kernel assigns the port at bind and nothing dials this
 * server (Codeberg #221). The Rust harness in tests/ffi_c_tests.rs passes
 * exactly this.
 *
 * Links the real libleviculum.so. Returns 0 on success, non-zero on the first
 * failed check. Compiled and run by the Rust harness in tests/ffi_c_tests.rs.
 */

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
    /* NULL path / name is rejected, the builder is left usable. */
    CHECK(lev_builder_config_file(b, NULL) == LEV_ERR_INVALID_ARG);
    CHECK(lev_builder_share_instance(b, NULL) == LEV_ERR_INVALID_ARG);
    CHECK(lev_builder_connect_shared_instance(b, NULL) == LEV_ERR_INVALID_ARG);
    CHECK(lev_builder_share_instance(b, "levtest-guard") == LEV_OK);
    lev_builder_free(b);
}

/* Write a minimal RNS-style config offering a single TCP server, no
 * transport, into dir/config. Returns the full config path (static buffer). */
static const char *write_config(const char *dir, int port) {
    static char path[512];
    snprintf(path, sizeof(path), "%s/config", dir);
    FILE *f = fopen(path, "w");
    if (!f) {
        return NULL;
    }
    fprintf(f,
            "[reticulum]\n"
            "  enable_transport = no\n"
            "\n"
            "[interfaces]\n"
            "  [[Test TCP Server]]\n"
            "    type = TCPServerInterface\n"
            "    enabled = yes\n"
            "    listen_ip = 127.0.0.1\n"
            "    listen_port = %d\n"
            "    mode = gateway\n",
            port);
    fclose(f);
    return path;
}

static void test_config_file(int port) {
    char dir[] = "/tmp/leviculum-c-daemon-cfg-XXXXXX";
    CHECK(mkdtemp(dir) != NULL);
    const char *cfg = write_config(dir, port);
    CHECK(cfg != NULL);

    lev_builder_t *b = lev_builder_new();
    CHECK(b != NULL);
    CHECK(lev_builder_storage_path(b, dir) == LEV_OK);
    CHECK(lev_builder_config_file(b, cfg) == LEV_OK);

    leviculum_t *node = lev_builder_build(b);
    lev_builder_free(b);
    CHECK(node != NULL);
    CHECK(lev_start(node) == LEV_OK);
    CHECK(lev_is_running(node) == 1);
    CHECK(lev_stop(node) == LEV_OK);
    lev_free(node);
}

/* A daemon node offers a shared instance; a client attaches to it by name. */
static void test_shared_instance(void) {
    /* Machine-wide abstract socket namespace, so keep the name unique. */
    char name[64];
    snprintf(name, sizeof(name), "levtest-c-%d", (int)getpid());

    char ddir[] = "/tmp/leviculum-c-daemon-d-XXXXXX";
    char cdir[] = "/tmp/leviculum-c-daemon-c-XXXXXX";
    CHECK(mkdtemp(ddir) != NULL);
    CHECK(mkdtemp(cdir) != NULL);

    lev_builder_t *db = lev_builder_new();
    CHECK(db != NULL);
    CHECK(lev_builder_storage_path(db, ddir) == LEV_OK);
    CHECK(lev_builder_share_instance(db, name) == LEV_OK);
    leviculum_t *daemon = lev_builder_build(db);
    lev_builder_free(db);
    CHECK(daemon != NULL);
    CHECK(lev_start(daemon) == LEV_OK);

    /* Let the local IPC server bind before the client connects. */
    usleep(400 * 1000);

    lev_builder_t *cb = lev_builder_new();
    CHECK(cb != NULL);
    CHECK(lev_builder_storage_path(cb, cdir) == LEV_OK);
    CHECK(lev_builder_connect_shared_instance(cb, name) == LEV_OK);
    leviculum_t *client = lev_builder_build(cb);
    lev_builder_free(cb);
    CHECK(client != NULL);
    CHECK(lev_start(client) == LEV_OK);
    CHECK(lev_is_running(client) == 1);

    lev_stop(client);
    lev_free(client);
    lev_stop(daemon);
    lev_free(daemon);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr,
                "usage: %s <port>\n"
                "  TCP port for the config-file node's TCPServerInterface;\n"
                "  0 is the intended form (the kernel assigns the port at\n"
                "  bind, nothing dials this server; Codeberg #221). No\n"
                "  default on purpose: a compiled-in literal is what made\n"
                "  ports collide across the suite (Codeberg #206).\n",
                argv[0]);
        return 2;
    }
    if (strspn(argv[1], "0123456789") != strlen(argv[1]) || strlen(argv[1]) == 0) {
        fprintf(stderr, "not a port: %s\n", argv[1]);
        return 2;
    }
    int port = atoi(argv[1]);
    if (port < 0 || port > 65535) {
        fprintf(stderr, "not a port: %s\n", argv[1]);
        return 2;
    }

    printf("leviculum daemon C acceptance test\n");
    test_argument_guards();
    test_config_file(port);
    test_shared_instance();

    if (failures == 0) {
        printf("OK\n");
        return 0;
    }
    fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
}
