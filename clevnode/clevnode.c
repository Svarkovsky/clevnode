/*
 * clevnode: Monolithic C Node and NomadNet Page Server for Reticulum
 * Version 0.1.1
 *
 * A lightweight C implementation of a monolithic Reticulum node
 * and NomadNet page server based on the Leviculum C API by Lew Palm.
 *
 * Copyright (c) 2026 Ivan Svarkovsky <ivansvarkovsky@gmail.com>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * Built upon the Leviculum C API (leviculum-ffi) by Lew Palm.
 * https://codeberg.org/Lew_Palm/leviculum
 */

/**
 * @file clevnode.c
 * @brief Monolithic Reticulum node and NomadNet page server daemon.
 * @author Ivan Svarkovsky <ivansvarkovsky@gmail.com>
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <time.h>
#include <pthread.h>
#include <limits.h>
#include <libgen.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <dirent.h>
#include "leviculum.h"

/** @brief Default directory path for Reticulum configuration and state. */
#define DEFAULT_CONFIG_DIR "/tmp/mnt/sda1/home/lblogd/.reticulum"

/** @brief Default directory path containing Micron (.mu) blog posts. */
#define DEFAULT_POSTS_DIR "/tmp/mnt/sda1/home/lblogd/posts"

/** @brief Default file path for storing the destination identity keypair. */
#define DEFAULT_IDENTITY_FILE "/tmp/mnt/sda1/home/lblogd/identities/lblogd"

/** @brief Resolved configuration directory path. */
static char resolved_config_dir[PATH_MAX];

/** @brief Resolved posts directory path. */
static char resolved_posts_dir[PATH_MAX];

/** @brief Resolved identity file path. */
static char resolved_identity_file[PATH_MAX];

/** @brief Global flag indicating whether the event loop should continue running. */
static volatile sig_atomic_t running = 1;

/**
 * @brief Resolves executable-relative paths for configuration, posts, and identity.
 *
 * Inspects /proc/self/exe to determine the base directory of the running binary.
 * If procfs inspection fails, relative local directory names are used as fallbacks.
 */
static void resolve_paths(void) {
    char exe_path[PATH_MAX];
    ssize_t len = readlink("/proc/self/exe", exe_path, sizeof(exe_path) - 1);
    if (len != -1) {
        exe_path[len] = 0;
        char *dir = dirname(exe_path);
        snprintf(resolved_config_dir, sizeof(resolved_config_dir), "%s/.reticulum", dir);
        snprintf(resolved_posts_dir, sizeof(resolved_posts_dir), "%s/posts", dir);
        snprintf(resolved_identity_file, sizeof(resolved_identity_file), "%s/identities/lblogd", dir);
    } else {
        strncpy(resolved_config_dir, ".reticulum", sizeof(resolved_config_dir));
        strncpy(resolved_posts_dir, "posts", sizeof(resolved_posts_dir));
        strncpy(resolved_identity_file, "identities/lblogd", sizeof(resolved_identity_file));
    }
}

/**
 * @brief Signal handler that captures the signal and sender PID, requesting daemon shutdown.
 *
 * @param sig Signal number received.
 * @param info Pointer to siginfo_t containing signal sender information.
 * @param ucontext Pointer to thread context (unused).
 */
static void handle_signal(int sig, siginfo_t *info, void *ucontext) {
    (void)ucontext;
    pid_t sender_pid = info ? info->si_pid : -1;
    uid_t sender_uid = info ? info->si_uid : (uid_t)0;

    const char *sig_name = "UNKNOWN";
    if (sig == SIGTERM) sig_name = "SIGTERM";
    else if (sig == SIGINT) sig_name = "SIGINT";
    else if (sig == SIGHUP) sig_name = "SIGHUP";

    char msg[256];
    int len = snprintf(msg, sizeof(msg),
        "\n[clevnode] >>> SIGNAL RECEIVED: %d (%s) from PID %d (UID %d). Shutting down... <<<\n",
        sig, sig_name, (int)sender_pid, (int)sender_uid);

    if (len > 0) {
        write(STDOUT_FILENO, msg, (size_t)len);
    }
    running = 0;
}

/**
 * @brief Node in a queue representing a pending request response for a link.
 */
struct pending_response {
    uint8_t request_id[LEV_ADDR_LEN];  /**< Reticulum request identifier. */
    uint8_t *raw_page;                 /**< Payload buffer wrapped in MessagePack. */
    size_t raw_len;                    /**< Size of the raw_page buffer in bytes. */
    struct pending_response *next;     /**< Pointer to the next queued response. */
};

/**
 * @brief Transmission queue and state descriptor for an active Reticulum link.
 */
struct link_state {
    uint8_t link_id[LEV_ADDR_LEN];       /**< Reticulum Link identifier. */
    struct pending_response *queue_head; /**< Head of the pending response queue. */
    struct pending_response *queue_tail; /**< Tail of the pending response queue. */
    int resource_in_flight;              /**< Indicator if a resource is actively transmitting. */
    struct link_state *next;             /**< Next tracked link state. */
};

/** @brief Head of the linked list containing active link states. */
static struct link_state *links = NULL;

/** @brief Mutex synchronizing access to the active links list and queues. */
static pthread_mutex_t links_mutex = PTHREAD_MUTEX_INITIALIZER;

/**
 * @brief Counts the number of queued pending responses for a given link.
 *
 * @param ls Pointer to the link state structure.
 * @return Number of queued responses.
 */
static int count_link_queue(const struct link_state *ls) {
    int count = 0;
    const struct pending_response *p = ls ? ls->queue_head : NULL;
    while (p) {
        count++;
        p = p->next;
    }
    return count;
}

/**
 * @brief Retrieves an existing link state entry or allocates a new one.
 *
 * @param link_id Reticulum Link identifier.
 * @return Pointer to the link_state structure, or NULL on allocation error.
 */
static struct link_state *find_or_create_link(const uint8_t *link_id) {
    struct link_state *ls = links;
    while (ls) {
        if (memcmp(ls->link_id, link_id, LEV_ADDR_LEN) == 0) return ls;
        ls = ls->next;
    }
    ls = calloc(1, sizeof(*ls));
    if (!ls) return NULL;
    memcpy(ls->link_id, link_id, LEV_ADDR_LEN);
    ls->next = links;
    links = ls;
    return ls;
}

/**
 * @brief Appends a pending response to the link's outgoing transmission queue.
 *
 * @param ls Pointer to the target link state.
 * @param request_id Reticulum request identifier.
 * @param raw_page Allocated buffer containing MessagePack-wrapped response data.
 * @param raw_len Length of raw_page in bytes.
 */
static void enqueue_response(struct link_state *ls,
                             const uint8_t *request_id,
                             uint8_t *raw_page,
                             size_t raw_len) {
    struct pending_response *pr = malloc(sizeof(*pr));
    if (!pr) { free(raw_page); return; }
    memcpy(pr->request_id, request_id, LEV_ADDR_LEN);
    pr->raw_page = raw_page;
    pr->raw_len = raw_len;
    pr->next = NULL;

    if (ls->queue_tail) ls->queue_tail->next = pr;
    else ls->queue_head = pr;
    ls->queue_tail = pr;
}

/**
 * @brief Extracts the oldest pending response from a link's queue.
 *
 * @param ls Pointer to the link state.
 * @return Pointer to the extracted pending_response, or NULL if the queue is empty.
 */
static struct pending_response *dequeue_response(struct link_state *ls) {
    struct pending_response *pr = ls->queue_head;
    if (!pr) return NULL;
    ls->queue_head = pr->next;
    if (!ls->queue_head) ls->queue_tail = NULL;
    pr->next = NULL;
    return pr;
}

/**
 * @brief Serializes a raw binary buffer into MessagePack binary format (bin8/bin16/bin32).
 *
 * @param raw Pointer to source binary payload.
 * @param len Size of source binary payload in bytes.
 * @param out_len Pointer to store the size of the resulting serialized buffer.
 * @return Dynamically allocated buffer containing MessagePack data, or NULL on error.
 */
static uint8_t *msgpack_wrap_bin(const uint8_t *raw, size_t len, size_t *out_len) {
    uint8_t *buf = NULL;
    if (len <= 255) {
        buf = malloc(len + 2);
        if (!buf) return NULL;
        buf[0] = 0xC4;
        buf[1] = (uint8_t)len;
        memcpy(buf + 2, raw, len);
        *out_len = len + 2;
    } else if (len <= 65535) {
        buf = malloc(len + 3);
        if (!buf) return NULL;
        buf[0] = 0xC5;
        buf[1] = (uint8_t)(len >> 8);
        buf[2] = (uint8_t)(len & 0xFF);
        memcpy(buf + 3, raw, len);
        *out_len = len + 3;
    } else {
        buf = malloc(len + 5);
        if (!buf) return NULL;
        buf[0] = 0xC6;
        buf[1] = (uint8_t)((len >> 24) & 0xFF);
        buf[2] = (uint8_t)((len >> 16) & 0xFF);
        buf[3] = (uint8_t)((len >> 8) & 0xFF);
        buf[4] = (uint8_t)(len & 0xFF);
        memcpy(buf + 5, raw, len);
        *out_len = len + 5;
    }
    return buf;
}

/**
 * @brief Initiates a chunked multi-packet Resource response transmission via Leviculum C API.
 *
 * @param node Pointer to the active Leviculum node instance.
 * @param link_id Target Reticulum link identifier.
 * @param request_id Request identifier being answered.
 * @param response_data Data buffer to transfer.
 * @param response_len Length of data buffer in bytes.
 * @return LEV_OK on success, or a negative error code on failure.
 */
static int send_response_resource(const struct leviculum_t *node,
                                  const uint8_t *link_id,
                                  const uint8_t *request_id,
                                  const uint8_t *response_data,
                                  size_t response_len) {
    return lev_send_response_resource(node, link_id, request_id, response_data, response_len, 15000);
}

/**
 * @brief Work item representing a transmission task dispatched to the worker thread.
 */
struct task {
    leviculum_t *node;                    /**< Pointer to active node. */
    uint8_t link_id[LEV_ADDR_LEN];        /**< Destination link identifier. */
    uint8_t request_id[LEV_ADDR_LEN];     /**< Request identifier. */
    uint8_t *raw_page;                    /**< Allocated MessagePack payload. */
    size_t raw_len;                       /**< Payload size in bytes. */
    struct task *next;                    /**< Pointer to next task in worker queue. */
};

/** @brief Head pointer of the worker task queue. */
static struct task *task_queue_head = NULL;

/** @brief Tail pointer of the worker task queue. */
static struct task *task_queue_tail = NULL;

/** @brief Mutex protecting the worker task queue. */
static pthread_mutex_t task_queue_mutex = PTHREAD_MUTEX_INITIALIZER;

/** @brief Condition variable used to signal available worker tasks. */
static pthread_cond_t task_queue_cond = PTHREAD_COND_INITIALIZER;

/**
 * @brief Submits a transmission task to the background worker thread queue.
 *
 * @param node Active Leviculum node handle.
 * @param link_id Target link identifier.
 * @param request_id Associated request identifier.
 * @param raw_page Dynamically allocated payload buffer.
 * @param raw_len Size of payload buffer.
 */
static void queue_task(leviculum_t *node, const uint8_t *link_id, const uint8_t *request_id, uint8_t *raw_page, size_t raw_len) {
    struct task *t = malloc(sizeof(*t));
    if (!t) {
        free(raw_page);
        return;
    }
    t->node = node;
    memcpy(t->link_id, link_id, LEV_ADDR_LEN);
    memcpy(t->request_id, request_id, LEV_ADDR_LEN);
    t->raw_page = raw_page;
    t->raw_len = raw_len;
    t->next = NULL;

    pthread_mutex_lock(&task_queue_mutex);
    if (task_queue_tail) {
        task_queue_tail->next = t;
    } else {
        task_queue_head = t;
    }
    task_queue_tail = t;
    pthread_cond_signal(&task_queue_cond);
    pthread_mutex_unlock(&task_queue_mutex);
}

/**
 * @brief Pops the next transmission task from the worker queue, blocking if empty.
 *
 * @return Pointer to task structure, or NULL if the daemon is shutting down.
 */
static struct task *dequeue_task(void) {
    pthread_mutex_lock(&task_queue_mutex);
    while (task_queue_head == NULL && running) {
        pthread_cond_wait(&task_queue_cond, &task_queue_mutex);
    }
    if (task_queue_head == NULL && !running) {
        pthread_mutex_unlock(&task_queue_mutex);
        return NULL;
    }
    struct task *t = task_queue_head;
    task_queue_head = t->next;
    if (!task_queue_head) {
        task_queue_tail = NULL;
    }
    pthread_mutex_unlock(&task_queue_mutex);
    return t;
}

/**
 * @brief Thread routine that asynchronously executes outgoing page responses.
 *
 * @param arg Thread startup argument (unused).
 * @return Always returns NULL.
 */
void *worker_thread_func(void *arg) {
    (void)arg;
    struct task *t;
    while ((t = dequeue_task()) != NULL) {
        int rc = lev_send_response(t->node, t->link_id, t->request_id, t->raw_page, t->raw_len, 5000);
        if (rc == LEV_OK) {
            printf("[clevnode] [worker] Sent raw Micron response (%zu bytes)\n", t->raw_len);

            pthread_mutex_lock(&links_mutex);
            struct link_state *ls = find_or_create_link(t->link_id);
            struct pending_response *pr = NULL;
            if (ls) {
                ls->resource_in_flight = 0;
                pr = dequeue_response(ls);
                if (pr) {
                    ls->resource_in_flight = 1;
                }
            }
            pthread_mutex_unlock(&links_mutex);

            if (pr) {
                queue_task(t->node, t->link_id, pr->request_id, pr->raw_page, pr->raw_len);
                free(pr);
            }
        } else {
            printf("[clevnode] [worker] Too large (%d), sending as resource...\n", rc);
            int res_rc = send_response_resource(t->node, t->link_id, t->request_id, t->raw_page, t->raw_len);
            if (res_rc == LEV_OK) {
                printf("[clevnode] [worker] Resource sent OK\n");
            } else {
                fprintf(stderr, "[clevnode] [worker] Resource failed immediately: %s (%d)\n", lev_strerror(res_rc), res_rc);

                pthread_mutex_lock(&links_mutex);
                struct link_state *ls = find_or_create_link(t->link_id);
                struct pending_response *pr = NULL;
                if (ls) {
                    ls->resource_in_flight = 0;
                    pr = dequeue_response(ls);
                    if (pr) {
                        ls->resource_in_flight = 1;
                    }
                }
                pthread_mutex_unlock(&links_mutex);

                if (pr) {
                    queue_task(t->node, t->link_id, pr->request_id, pr->raw_page, pr->raw_len);
                    free(pr);
                }
            }
        }

        free(t->raw_page);
        free(t);
    }
    return NULL;
}

/**
 * @brief Serializes a string buffer into MessagePack string format (fixstr/str8/str16/str32).
 *
 * @param raw Pointer to UTF-8 encoded text.
 * @param len Byte length of the string.
 * @param out_len Pointer to store the output serialized buffer size.
 * @return Allocated buffer with serialized MessagePack string, or NULL on error.
 */
static uint8_t *msgpack_wrap_string(const uint8_t *raw, size_t len, size_t *out_len) {
    uint8_t *buf = NULL;
    if (len <= 31) {
        buf = malloc(len + 1);
        if (!buf) return NULL;
        buf[0] = (uint8_t)(0xA0 | len);
        memcpy(buf + 1, raw, len);
        *out_len = len + 1;
    } else if (len <= 255) {
        buf = malloc(len + 2);
        if (!buf) return NULL;
        buf[0] = 0xD9;
        buf[1] = (uint8_t)len;
        memcpy(buf + 2, raw, len);
        *out_len = len + 2;
    } else if (len <= 65535) {
        buf = malloc(len + 3);
        if (!buf) return NULL;
        buf[0] = 0xDA;
        buf[1] = (uint8_t)(len >> 8);
        buf[2] = (uint8_t)(len & 0xFF);
        memcpy(buf + 3, raw, len);
        *out_len = len + 3;
    } else {
        buf = malloc(len + 5);
        if (!buf) return NULL;
        buf[0] = 0xDB;
        buf[1] = (uint8_t)((len >> 24) & 0xFF);
        buf[2] = (uint8_t)((len >> 16) & 0xFF);
        buf[3] = (uint8_t)((len >> 8) & 0xFF);
        buf[4] = (uint8_t)(len & 0xFF);
        memcpy(buf + 5, raw, len);
        *out_len = len + 5;
    }
    return buf;
}

#define PAGE_CACHE_MAX_SLOTS 16

struct page_cache_slot {
    char req_path[128];
    char filepath[512];
    uint8_t *msgpack_data;
    size_t data_len;
    time_t mtime;
    time_t last_used;
    int is_valid;
};

static struct page_cache_slot page_cache[PAGE_CACHE_MAX_SLOTS];

static uint8_t *get_page_content(const char *req_path, const char *posts_dir, size_t *out_len);

/**
 * @brief Scans posts_dir and dynamically registers all .mu page files with the Reticulum core.
 */
static void sync_page_registration(leviculum_t *node, const uint8_t *dest_hash, const char *posts_dir) {
    if (!node || !dest_hash || !posts_dir) return;

    /* Base handlers for index */
    lev_register_request_handler(node, dest_hash, "/page/index.mu", LEV_REQUEST_POLICY_ALLOW_ALL, NULL, 0);
    lev_register_request_handler(node, dest_hash, "/page/", LEV_REQUEST_POLICY_ALLOW_ALL, NULL, 0);
    lev_register_request_handler(node, dest_hash, "/page", LEV_REQUEST_POLICY_ALLOW_ALL, NULL, 0);

    DIR *d = opendir(posts_dir);
    if (!d) return;

    struct dirent *entry;
    while ((entry = readdir(d)) != NULL) {
        if (entry->d_name[0] == '.') continue;
        size_t nlen = strlen(entry->d_name);
        if (nlen > 3 && strcmp(entry->d_name + nlen - 3, ".mu") == 0) {
            char req_path[320];
            snprintf(req_path, sizeof(req_path), "/page/%s", entry->d_name);
            lev_register_request_handler(node, dest_hash, req_path, LEV_REQUEST_POLICY_ALLOW_ALL, NULL, 0);
        }
    }
    closedir(d);
}

/**
 * @brief Retrieves page content from cache, or reloads it on-demand if modified on disk.
 */
static uint8_t *get_page_content(const char *req_path, const char *posts_dir, size_t *out_len) {
    *out_len = 0;
    if (!req_path || !posts_dir) return NULL;

    const char *subpath = NULL;
    if (strcmp(req_path, "/page") == 0 || strcmp(req_path, "/page/") == 0) {
        subpath = "index.mu";
    } else if (strncmp(req_path, "/page/", 6) == 0) {
        subpath = req_path + 6;
    } else {
        return NULL;
    }

    /* Path traversal & security protection */
    if (strstr(subpath, "..") != NULL || strchr(subpath, '/') != NULL || strchr(subpath, '\\') != NULL) {
        fprintf(stderr, "[clevnode] Security: rejected traversal path: %s\n", req_path);
        return NULL;
    }
    /* Reject hidden files (e.g. .hidden.mu, .env, .git) */
    if (subpath[0] == '.') {
        fprintf(stderr, "[clevnode] Security: rejected hidden file path: %s\n", req_path);
        return NULL;
    }
    size_t sub_len = strlen(subpath);
    if (sub_len == 0 || sub_len > 120) {
        return NULL;
    }
    /* Must end with .mu */
    if (sub_len < 3 || strcmp(subpath + sub_len - 3, ".mu") != 0) {
        fprintf(stderr, "[clevnode] Security: rejected non-.mu extension: %s\n", req_path);
        return NULL;
    }

    char filepath[512];
    int n = snprintf(filepath, sizeof(filepath), "%s/%s", posts_dir, subpath);
    if (n < 0 || (size_t)n >= sizeof(filepath)) {
        return NULL;
    }

    struct stat st;
    if (stat(filepath, &st) != 0 || !S_ISREG(st.st_mode)) {
        return NULL; /* 404 */
    }

    time_t now = time(NULL);
    int found_idx = -1;
    int oldest_idx = 0;
    time_t oldest_time = now + 1000;

    for (int i = 0; i < PAGE_CACHE_MAX_SLOTS; i++) {
        if (!page_cache[i].is_valid) {
            oldest_idx = i;
            oldest_time = 0;
            continue;
        }
        if (strcmp(page_cache[i].filepath, filepath) == 0) {
            found_idx = i;
            break;
        }
        if (page_cache[i].last_used < oldest_time) {
            oldest_time = page_cache[i].last_used;
            oldest_idx = i;
        }
    }

    if (found_idx >= 0) {
        struct page_cache_slot *slot = &page_cache[found_idx];
        slot->last_used = now;

        if (slot->mtime == st.st_mtime && slot->msgpack_data != NULL) {
            uint8_t *copy = malloc(slot->data_len);
            if (copy) {
                memcpy(copy, slot->msgpack_data, slot->data_len);
                *out_len = slot->data_len;
            }
            return copy;
        }

        printf("[clevnode] Reloading modified page from disk: %s\n", filepath);
        free(slot->msgpack_data);
        slot->msgpack_data = NULL;
        slot->data_len = 0;
        slot->is_valid = 0;
        oldest_idx = found_idx;
    }

    FILE *f = fopen(filepath, "rb");
    if (!f) return NULL;

    fseek(f, 0, SEEK_END);
    long fsize = ftell(f);
    fseek(f, 0, SEEK_SET);

    if (fsize < 0 || fsize > 10 * 1024 * 1024) {
        fclose(f);
        return NULL;
    }

    uint8_t *raw_buf = malloc(fsize > 0 ? fsize : 1);
    if (!raw_buf) {
        fclose(f);
        return NULL;
    }
    size_t rb = fread(raw_buf, 1, fsize, f);
    fclose(f);

    size_t mp_len = 0;
    uint8_t *mp_data = msgpack_wrap_bin(raw_buf, rb, &mp_len);
    free(raw_buf);
    if (!mp_data) return NULL;

    struct page_cache_slot *slot = &page_cache[oldest_idx];
    if (slot->is_valid && slot->msgpack_data) {
        free(slot->msgpack_data);
    }
    strncpy(slot->req_path, req_path, sizeof(slot->req_path) - 1);
    slot->req_path[sizeof(slot->req_path) - 1] = '\0';
    strncpy(slot->filepath, filepath, sizeof(slot->filepath) - 1);
    slot->filepath[sizeof(slot->filepath) - 1] = '\0';
    slot->msgpack_data = mp_data;
    slot->data_len = mp_len;
    slot->mtime = st.st_mtime;
    slot->last_used = now;
    slot->is_valid = 1;

    printf("[clevnode] Cached %s (%zu bytes wrapped in MsgPack, slot %d)\n", filepath, mp_len, oldest_idx);

    uint8_t *copy = malloc(mp_len);
    if (copy) {
        memcpy(copy, mp_data, mp_len);
        *out_len = mp_len;
    }
    return copy;
}

/** @brief Human-readable node name used in network announcements. */
static char node_name[128] = "Clevnode";

/**
 * @brief Parses node_name from the [reticulum] section of an INI configuration file.
 *
 * @param config_path Path to the configuration file.
 */
static void load_node_name_from_config(const char *config_path) {
    FILE *f = fopen(config_path, "r");
    if (!f) return;

    char line[256];
    int in_reticulum_sec = 0;

    while (fgets(line, sizeof(line), f)) {
        char *p = line;
        while (*p == ' ' || *p == '\t') p++;

        if (*p == '[') {
            if (strncmp(p, "[reticulum]", 11) == 0) in_reticulum_sec = 1;
            else if (*p != '[') in_reticulum_sec = 0;
            continue;
        }

        char *val = NULL;
        if (in_reticulum_sec && strncmp(p, "node_name", 9) == 0) {
            val = p + 9;
        }

        if (val) {
            while (*val == ' ' || *val == '\t') val++;
            if (*val == '=') {
                val++;
                while (*val == ' ' || *val == '\t') val++;

                if (*val == '"' || *val == '\'') val++;

                char *end = val + strlen(val) - 1;
                while (end > val && (*end == '\n' || *end == '\r' || *end == ' ' || *end == '\t' || *end == '"' || *end == '\'')) {
                    *end = '\0';
                    end--;
                }

                if (strlen(val) > 0) {
                    snprintf(node_name, sizeof(node_name), "%s", val);
                    break;
                }
            }
        }
    }
    fclose(f);
}

/**
 * @brief Main entry point of the clevnode daemon.
 *
 * @param argc Argument count.
 * @param argv Argument vector.
 * @return 0 on successful termination, non-zero on failure.
 */
int main(int argc, char **argv) {
    if (argc > 1 && (strcmp(argv[1], "--help") == 0 || strcmp(argv[1], "-h") == 0)) {
        printf("Usage: clevnode [CONFIG_DIR] [POSTS_DIR] [IDENTITY_FILE]\n");
        printf("   or: clevnode --version | -v\n");
        printf("   or: clevnode --help | -h\n");
        return 0;
    }
    if (argc > 1 && (strcmp(argv[1], "--version") == 0 || strcmp(argv[1], "-v") == 0)) {
        printf("clevnode v0.1.1\n");
        return 0;
    }

    setsid();

    setvbuf(stdout, NULL, _IONBF, 0);
    setvbuf(stderr, NULL, _IONBF, 0);

    resolve_paths();
    const char *config_dir = resolved_config_dir;
    const char *posts_dir = resolved_posts_dir;
    const char *identity_file = resolved_identity_file;

    if (argc > 1) config_dir = argv[1];
    if (argc > 2) posts_dir = argv[2];
    if (argc > 3) identity_file = argv[3];

    printf("[clevnode] ===================================================\n");
    printf("[clevnode] clevnode v0.1.1 (Reticulum Node & NomadNet Server)\n");
    printf("[clevnode] Author: Ivan Svarkovsky <ivansvarkovsky@gmail.com>\n");
    printf("[clevnode] Engine: Leviculum C-API by Lew Palm <lp@lew-palm.de>\n");
    printf("[clevnode] License: GNU AGPLv3+\n");
    printf("[clevnode] ===================================================\n");
    printf("\n");
    printf("[clevnode] Config: %s  Posts: %s  Identity: %s\n", config_dir, posts_dir, identity_file);
    
    printf("[clevnode] Starting monolithic C-Leviculum Node...\n");
    printf("[clevnode] Config: %s  Posts: %s  Identity: %s\n", config_dir, posts_dir, identity_file);

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = handle_signal;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGTERM, &sa, NULL);
    signal(SIGINT, SIG_IGN);
    signal(SIGPIPE, SIG_IGN);

    if (lev_init() != LEV_OK) {
        fprintf(stderr, "[clevnode] lev_init failed\n");
        return 1;
    }

    int ret = 0;
    int worker_started = 0;
    pthread_t worker_tid;
    lev_identity_t *site_id = NULL;
    lev_builder_t *b = NULL;
    leviculum_t *node = NULL;
    lev_destination_t *dest = NULL;

    site_id = lev_identity_load_file(identity_file);
    if (!site_id) {
        printf("[clevnode] No identity, generating new...\n");
        site_id = lev_identity_generate();
        if (!site_id) {
            fprintf(stderr, "[clevnode] identity gen failed\n");
            ret = 1;
            goto cleanup;
        }
        lev_identity_save_file(site_id, identity_file);
    } else {
        printf("[clevnode] Identity loaded\n");
    }

    printf("[clevnode] Page cache initialized for directory: %s\n", posts_dir);

    b = lev_builder_new();
    if (!b) {
        fprintf(stderr, "[clevnode] builder failed\n");
        ret = 1;
        goto cleanup;
    }

    char config_path[4096 + 32], storage_path[4096 + 32];
    snprintf(config_path, sizeof(config_path), "%s/config", config_dir);
    snprintf(storage_path, sizeof(storage_path), "%s/storage", config_dir);

    load_node_name_from_config(config_path);
    if (argc > 4) {
        snprintf(node_name, sizeof(node_name), "%s", argv[4]);
    }
    printf("[clevnode] Node Name: %s\n", node_name);
    lev_builder_config_file(b, config_path);
    lev_builder_storage_path(b, storage_path);

    printf("[clevnode] Building Reticulum node from config...\n");
    node = lev_builder_build(b);
    lev_builder_free(b);
    b = NULL;

    if (!node) {
        fprintf(stderr, "[clevnode] build failed\n");
        ret = 1;
        goto cleanup;
    }
    printf("[clevnode] Reticulum node built successfully\n");

    printf("[clevnode] Starting Reticulum transport...\n");
    int start_rc = lev_start(node);
    if (start_rc != LEV_OK) {
        fprintf(stderr, "[clevnode] start failed: %s (%d)\n", lev_strerror(start_rc), start_rc);
        const char *err_detail = lev_last_error();
        if (err_detail) {
            fprintf(stderr, "[clevnode] Error detail: %s\n", err_detail);
        }
        ret = 1;
        goto cleanup;
    }
    printf("[clevnode] Transport started\n");

    const char *aspects[] = {"node"};
    dest = lev_destination_new(site_id, LEV_DIRECTION_IN, LEV_DEST_SINGLE, "nomadnetwork", aspects, 1);
    if (!dest) {
        fprintf(stderr, "[clevnode] dest new failed\n");
        ret = 1;
        goto cleanup;
    }
    lev_destination_set_accepts_links(dest, 1);

    uint8_t dest_hash[LEV_ADDR_LEN];
    size_t dhl = sizeof(dest_hash);
    if (lev_destination_hash(dest, dest_hash, sizeof(dest_hash), &dhl) != LEV_OK) {
        fprintf(stderr, "[clevnode] hash failed\n");
        ret = 1;
        goto cleanup;
    }

    printf("[clevnode] ===================================================\n");
    printf("[clevnode] Destination Hash: ");
    for (size_t i = 0; i < dhl; i++) printf("%02x", dest_hash[i]);
    printf("\n");
    printf("[clevnode] ===================================================\n");

    if (lev_register_destination(node, dest) != LEV_OK) {
        fprintf(stderr, "[clevnode] register dest failed\n");
        ret = 1;
        goto cleanup;
    }
    printf("[clevnode] Destination registered\n");

    sync_page_registration(node, dest_hash, posts_dir);
    size_t warm_len = 0;
    uint8_t *warm_p = get_page_content("/page/index.mu", posts_dir, &warm_len);
    if (warm_p) free(warm_p);

    pthread_attr_t worker_attr;
    pthread_attr_init(&worker_attr);
    pthread_attr_setstacksize(&worker_attr, 128 * 1024);
    if (pthread_create(&worker_tid, &worker_attr, worker_thread_func, NULL) != 0) {
        pthread_attr_destroy(&worker_attr);
        fprintf(stderr, "[clevnode] Failed to start worker thread\n");
        ret = 1;
        goto cleanup;
    }
    pthread_attr_destroy(&worker_attr);
    worker_started = 1;

    printf("[clevnode] Sending initial announcement as \"%s\"...\n", node_name);
    lev_announce(node, dest_hash, (const uint8_t *)node_name, strlen(node_name), 5000);

    printf("[clevnode] Event loop running\n");

    time_t last_announce = time(NULL);
    time_t last_page_sync = time(NULL);

    while (running) {
        time_t now = time(NULL);
        if (now - last_announce >= 1800) {
            printf("[clevnode] Re-announcing as \"%s\"...\n", node_name);
            lev_announce(node, dest_hash, (const uint8_t *)node_name, strlen(node_name), 5000);
            last_announce = now;
        }

        if (now - last_page_sync >= 30) {
            sync_page_registration(node, dest_hash, posts_dir);
            last_page_sync = now;
        }

        lev_event_t *ev = NULL;
        int res = lev_wait_event(node, &ev, 500);
        if (res != LEV_OK || !ev) {
            if (ev) lev_event_free(ev);
            continue;
        }

        int ev_type = lev_event_type(ev);

        if (ev_type == LEV_EVENT_LINK_ESTABLISHED) {
            printf("[clevnode] Link established\n");
        } else if (ev_type == LEV_EVENT_LINK_CLOSED) {
            uint8_t lid[LEV_ADDR_LEN];
            size_t all = sizeof(lid);
            if (lev_event_link_id(ev, lid, sizeof(lid), &all) == LEV_OK) {
                pthread_mutex_lock(&links_mutex);
                struct link_state **pp = &links;
                while (*pp) {
                    if (memcmp((*pp)->link_id, lid, LEV_ADDR_LEN) == 0) {
                        struct link_state *to_remove = *pp;
                        *pp = to_remove->next;
                        struct pending_response *p = to_remove->queue_head;
                        while (p) {
                            struct pending_response *nx = p->next;
                            free(p->raw_page);
                            free(p);
                            p = nx;
                        }
                        free(to_remove);
                        break;
                    }
                    pp = &(*pp)->next;
                }
                pthread_mutex_unlock(&links_mutex);
                printf("[clevnode] Link closed, queue cleared\n");
            }
        } else if (ev_type == LEV_EVENT_REQUEST_RECEIVED) {
            uint8_t path[256];
            size_t pl = sizeof(path) - 1;
            if (lev_event_path(ev, path, sizeof(path) - 1, &pl) == LEV_OK) {
                path[pl] = '\0';
                printf("[clevnode] Request: %s\n", (char *)path);

                size_t raw_len = 0;
                uint8_t *raw_page = get_page_content((const char *)path, posts_dir, &raw_len);

                if (raw_page && raw_len > 0) {

                        uint8_t lid[LEV_ADDR_LEN], rid[LEV_ADDR_LEN];
                        size_t all = sizeof(lid), gil = sizeof(rid);
                        if (lev_event_link_id(ev, lid, sizeof(lid), &all) == LEV_OK &&
                            lev_event_request_id(ev, rid, sizeof(rid), &gil) == LEV_OK) {

                            pthread_mutex_lock(&links_mutex);
                            struct link_state *ls = find_or_create_link(lid);
                            if (!ls) {
                                pthread_mutex_unlock(&links_mutex);
                                free(raw_page);
                            } else if (!ls->resource_in_flight && ls->queue_head == NULL) {
                                ls->resource_in_flight = 1;
                                pthread_mutex_unlock(&links_mutex);

                                queue_task(node, lid, rid, raw_page, raw_len);
                            } else {
                                enqueue_response(ls, rid, raw_page, raw_len);
                                printf("[clevnode] Queued response (queue len=%d, in_flight=%d)\n",
                                       count_link_queue(ls),
                                       ls->resource_in_flight);
                                pthread_mutex_unlock(&links_mutex);
                            }
                        } else {
                            free(raw_page);
                        }
                } else {
                    const char *err_msg = "File not found";
                    size_t mp_len = 0;
                    uint8_t *mp_data = msgpack_wrap_string((const uint8_t *)err_msg, strlen(err_msg), &mp_len);
                    uint8_t lid[LEV_ADDR_LEN], rid[LEV_ADDR_LEN];
                    size_t all = sizeof(lid), gil = sizeof(rid);
                    if (lev_event_link_id(ev, lid, sizeof(lid), &all) == LEV_OK &&
                        lev_event_request_id(ev, rid, sizeof(rid), &gil) == LEV_OK) {
                        lev_send_response(node, lid, rid, mp_data, mp_len, 5000);
                    }
                    free(mp_data);
                }
            }
        } else if (ev_type == LEV_EVENT_RESOURCE_COMPLETED) {
            uint8_t lid[LEV_ADDR_LEN];
            size_t all = sizeof(lid);
            if (lev_event_link_id(ev, lid, sizeof(lid), &all) == LEV_OK) {
                struct pending_response *pr = NULL;
                pthread_mutex_lock(&links_mutex);
                struct link_state *ls = links;
                while (ls) {
                    if (memcmp(ls->link_id, lid, LEV_ADDR_LEN) == 0) {
                        ls->resource_in_flight = 0;
                        pr = dequeue_response(ls);
                        if (pr) {
                            ls->resource_in_flight = 1;
                        }
                        break;
                    }
                    ls = ls->next;
                }
                pthread_mutex_unlock(&links_mutex);

                if (pr) {
                    queue_task(node, lid, pr->request_id, pr->raw_page, pr->raw_len);
                    free(pr);
                }
                printf("[clevnode] Resource completed on link\n");
            }
        } else if (ev_type == LEV_EVENT_RESOURCE_FAILED) {
            uint8_t lid[LEV_ADDR_LEN];
            size_t all = sizeof(lid);
            if (lev_event_link_id(ev, lid, sizeof(lid), &all) == LEV_OK) {
                struct pending_response *pr = NULL;
                pthread_mutex_lock(&links_mutex);
                struct link_state *ls = links;
                while (ls) {
                    if (memcmp(ls->link_id, lid, LEV_ADDR_LEN) == 0) {
                        ls->resource_in_flight = 0;
                        pr = dequeue_response(ls);
                        if (pr) {
                            ls->resource_in_flight = 1;
                        }
                        break;
                    }
                    ls = ls->next;
                }
                pthread_mutex_unlock(&links_mutex);

                if (pr) {
                    queue_task(node, lid, pr->request_id, pr->raw_page, pr->raw_len);
                    free(pr);
                }
                printf("[clevnode] Resource failed on link\n");
            }
        }

        lev_event_free(ev);
    }

cleanup:
    printf("[clevnode] Stopping...\n");

    if (worker_started) {
        pthread_cond_broadcast(&task_queue_cond);
        pthread_join(worker_tid, NULL);
    }

    pthread_mutex_lock(&task_queue_mutex);
    struct task *t = task_queue_head;
    while (t) {
        struct task *next_t = t->next;
        free(t->raw_page);
        free(t);
        t = next_t;
    }
    task_queue_head = task_queue_tail = NULL;
    pthread_mutex_unlock(&task_queue_mutex);

    pthread_mutex_lock(&links_mutex);
    struct link_state *ls = links;
    while (ls) {
        struct link_state *next_ls = ls->next;
        struct pending_response *pr = ls->queue_head;
        while (pr) {
            struct pending_response *next_pr = pr->next;
            free(pr->raw_page);
            free(pr);
            pr = next_pr;
        }
        free(ls);
        ls = next_ls;
    }
    links = NULL;
    pthread_mutex_unlock(&links_mutex);

    if (dest) {
        lev_destination_free(dest);
    }
    if (b) {
        lev_builder_free(b);
    }
    if (node) {
        lev_stop(node);
        lev_free(node);
    }
    if (site_id) {
        lev_identity_free(site_id);
    }
    /* Free all link states */
    pthread_mutex_lock(&links_mutex);
    struct link_state *curr_ls = links;
    while (curr_ls) {
        struct link_state *next_ls = curr_ls->next;
        struct pending_response *curr_p = curr_ls->queue_head;
        while (curr_p) {
            struct pending_response *next_p = curr_p->next;
            if (curr_p->raw_page) free(curr_p->raw_page);
            free(curr_p);
            curr_p = next_p;
        }
        free(curr_ls);
        curr_ls = next_ls;
    }
    links = NULL;
    pthread_mutex_unlock(&links_mutex);

    for (int i = 0; i < PAGE_CACHE_MAX_SLOTS; i++) {
        if (page_cache[i].is_valid && page_cache[i].msgpack_data) {
            free(page_cache[i].msgpack_data);
            page_cache[i].msgpack_data = NULL;
            page_cache[i].is_valid = 0;
        }
    }

    printf("[clevnode] Stopped OK\n");
    return ret;
}
