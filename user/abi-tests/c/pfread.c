/* abi-pfread (PF-1): the file read path under N threads, for the PF chain (Linux 4c0f369's test shape):
 * pread of 16 bytes from one cached page, 64 KiB and 1 MiB reads, each with one shared fd, one fd per thread
 * on one file, and one file per thread; N = 1, 2, 4, 8, 16; on any directory (tmpfs /tmp, ext4 /data).
 *
 * Without an argument (and with `check [dir]`) it runs a short, deterministic correctness pass that the ABI
 * diff compares with CachyOS: every case with 2 threads and a few reads, the bytes checked, the files removed.
 * `bench` measures and prints timings, cycles and rates: it breaks the README's "no times, no counts" rule on
 * purpose and must never get a line in cases.txt.
 *
 *   abi-pfread bench [ms=250] [threads=1,2,4,8,16] [sizes=16,65536,1048576]
 *                    [layouts=shared-fd,fd-per-thread,file-per-thread] [dir[:label] ...]   (default /tmp:tmpfs)
 *   abi-pfread clean [dir ...]   removes pfread-* files a killed bench left behind (default /tmp)
 *
 * Cycles are rdtsc ticks around each thread's whole loop divided by its reads (TSC rate, not core clock;
 * tsc_mhz on each line converts them); reads/s is all reads over the wall time from the first thread's start
 * to the last thread's end (CLOCK_MONOTONIC). Only open/pread64/pwrite64/close/unlink/clone/futex/
 * clock_gettime/nanosleep/mmap (plus rt_sigaction, unchecked, for the cleanup; getdents64 in `clean`), so
 * the same file runs on CachyOS and on every V2 generation. */
#include "abi.h"
#include <fcntl.h>
#include <limits.h>
#include <linux/futex.h>
#include <dirent.h>
#include <pthread.h>
#include <signal.h>

#pragma clang attribute push(__attribute__((target("arch=x86-64-v3"))), apply_to = function)

#define FILE_SZ (1l << 20)
#define CHUNK (64l << 10)

enum layout { SHARED_FD, FD_PER_THREAD, FILE_PER_THREAD, LAYOUTS };
static const char *const layout_name[LAYOUTS] = {"shared-fd", "fd-per-thread", "file-per-thread"};

struct worker {
    pthread_t t;
    int id;           /* pattern id of the file it reads */
    int fd;           /* the shared fd, or -1: open path itself */
    const char *path;
    long size;
    off_t off;
    unsigned long limit; /* 0: read until stop */
    char *buf;
    unsigned long reads;
    uint64_t tsc, ns0, ns1;
    long got;         /* the failing pread's result */
    int err;          /* errno of the failure, or -1 for a short read */
    const char *what;
    long bad;         /* wrong bytes in the last read */
};

static int go, ready, stop;

static unsigned char pat(int id, long i) {
    return (unsigned char)((i >> 4) * 29 + (i & 15) * 7 + id * 101 + (i >> 12));
}

static long verify(const char *b, long len, off_t off, int id) {
    long bad = 0;
    for (long i = 0; i < len; i++)
        bad += (unsigned char)b[i] != pat(id, off + i);
    return bad;
}

static void futex_wake_all(int *a) {
    syscall(SYS_futex, a, FUTEX_WAKE_PRIVATE, INT_MAX, 0, 0, 0);
}

static void *worker(void *arg) {
    struct worker *w = arg;
    int fd = w->fd, own = 0;
    if (fd < 0) {
        fd = open(w->path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) {
            w->err = errno;
            w->what = "open";
        } else {
            own = 1;
        }
    }
    __atomic_add_fetch(&ready, 1, __ATOMIC_SEQ_CST);
    while (!__atomic_load_n(&go, __ATOMIC_ACQUIRE))
        syscall(SYS_futex, &go, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0);
    if (w->err || __atomic_load_n(&stop, __ATOMIC_RELAXED))
        goto out;
    unsigned long n = 0;
    w->ns0 = ns_of(CLOCK_MONOTONIC);
    uint64_t c0 = __builtin_ia32_rdtsc();
    while (!__atomic_load_n(&stop, __ATOMIC_RELAXED)) {
        ssize_t r = pread(fd, w->buf, (size_t)w->size, w->off);
        if (r != w->size) {
            w->err = r < 0 ? errno : -1;
            w->got = (long)r;
            w->what = "pread";
            break;
        }
        if (++n == w->limit)
            break;
    }
    w->tsc = __builtin_ia32_rdtsc() - c0;
    w->ns1 = ns_of(CLOCK_MONOTONIC);
    w->reads = n;
    if (!w->err && n)
        w->bad = verify(w->buf, w->size, w->off, w->id);
out:
    if (own)
        close(fd);
    return 0;
}

struct fsdir {
    const char *dir, *label;
    char **paths; /* [0] the shared file, [1 + t] thread t's own file */
    int nfiles;
};

static int make_file(const char *path, int id, char *buf) {
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
    if (fd < 0) {
        fprintf(stderr, "pfread: open %s: %s\n", path, ename(errno));
        return -1;
    }
    for (long o = 0; o < FILE_SZ; o += CHUNK) {
        for (long i = 0; i < CHUNK; i++)
            buf[i] = (char)pat(id, o + i);
        if (pwrite(fd, buf, CHUNK, o) != CHUNK) {
            fprintf(stderr, "pfread: pwrite %s: %s\n", path, ename(errno));
            close(fd);
            return -1;
        }
    }
    /* Warm: two full passes, checked, so no page is filled or read ahead inside a measured loop. */
    for (int pass = 0; pass < 2; pass++)
        for (long o = 0; o < FILE_SZ; o += CHUNK)
            if (pread(fd, buf, CHUNK, o) != CHUNK || verify(buf, CHUNK, o, id)) {
                fprintf(stderr, "pfread: warm read of %s failed or wrong\n", path);
                close(fd);
                return -1;
            }
    close(fd);
    return 0;
}

static void remove_files(struct fsdir *d) {
    for (int i = 0; i < d->nfiles; i++)
        unlink(d->paths[i]);
    d->nfiles = 0;
}

/* A run killed by SIGTERM/SIGINT/SIGHUP (a timeout, Ctrl-C) still removes its ~17 MiB of files; unlink and
 * _exit are async-signal-safe. SIGKILL cannot be caught: `abi-pfread clean [dir ...]` removes what is left. */
static struct fsdir *volatile live_dir;

static void on_signal(int sig) {
    struct fsdir *d = live_dir;
    if (d)
        for (int i = 0; i < d->nfiles; i++)
            unlink(d->paths[i]);
    _exit(128 + sig);
}

static void catch_signals(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_signal;
    sigemptyset(&sa.sa_mask);
    /* No error check: a generation without rt_sigaction only loses the cleanup, never the measurement. */
    sigaction(SIGTERM, &sa, 0);
    sigaction(SIGINT, &sa, 0);
    sigaction(SIGHUP, &sa, 0);
}

/* Removes every pfread-* file in each dir (left behind by a SIGKILL); prints how many. */
static int clean(int argc, char **argv) {
    static char *def[] = {"/tmp"};
    if (!argc)
        argc = 1, argv = def;
    int rc = 0;
    for (int i = 0; i < argc; i++) {
        DIR *dp = opendir(argv[i]);
        if (!dp) {
            fprintf(stderr, "pfread: opendir %s: %s\n", argv[i], ename(errno));
            rc = 1;
            continue;
        }
        int n = 0;
        char path[PATH_MAX];
        for (struct dirent *e; (e = readdir(dp));)
            if (!strncmp(e->d_name, "pfread-", 7)) {
                snprintf(path, sizeof path, "%s/%s", argv[i], e->d_name);
                if (unlink(path)) {
                    fprintf(stderr, "pfread: unlink %s: %s\n", path, ename(errno));
                    rc = 1;
                } else
                    n++;
            }
        closedir(dp);
        printf("pfread clean %s: removed %d\n", argv[i], n);
    }
    return rc;
}

static int make_files(struct fsdir *d, int maxn) {
    char *buf = malloc(CHUNK);
    if (!buf)
        return -1;
    d->paths = calloc((size_t)maxn + 1, sizeof *d->paths);
    if (!d->paths) {
        free(buf);
        return -1;
    }
    for (int i = 0; i <= maxn; i++) {
        size_t len = strlen(d->dir) + 48;
        d->paths[i] = malloc(len);
        if (!d->paths[i])
            break;
        if (i == 0)
            snprintf(d->paths[i], len, "%s/pfread-%d-s", d->dir, (int)getpid());
        else
            snprintf(d->paths[i], len, "%s/pfread-%d-%d", d->dir, (int)getpid(), i - 1);
        d->nfiles = i + 1;
        if (make_file(d->paths[i], i, buf)) {
            free(buf);
            remove_files(d);
            return -1;
        }
    }
    free(buf);
    return d->nfiles == maxn + 1 ? 0 : (remove_files(d), -1);
}

struct result {
    unsigned long reads;
    uint64_t tsc, wall_ns, thread_ns;
};

/* One case: n threads, joined; 0 on success, a message on stderr and -1 on any failure (never hangs). */
static int run_case(struct fsdir *d, enum layout l, long size, int n, unsigned long limit, long ms,
                    struct result *res) {
    struct worker *w = calloc((size_t)n, sizeof *w);
    if (!w)
        return -1;
    int shared = -1, rc = 0, made = 0;
    if (l == SHARED_FD) {
        shared = open(d->paths[0], O_RDONLY | O_CLOEXEC);
        if (shared < 0) {
            fprintf(stderr, "pfread: open %s: %s\n", d->paths[0], ename(errno));
            free(w);
            return -1;
        }
    }
    go = 0;
    ready = 0;
    stop = 0;
    for (int t = 0; t < n; t++) {
        struct worker *x = &w[t];
        int own = l == FILE_PER_THREAD;
        x->id = own ? t + 1 : 0;
        x->fd = shared;
        x->path = d->paths[own ? t + 1 : 0];
        x->size = size;
        /* FILE_SZ - size + 1 keeps every read inside the file for any size= (a short pread would end the run). */
        x->off = size == 16 ? (off_t)(t * 64 % 4096) : (off_t)(t * size % (FILE_SZ - size + 1));
        x->limit = limit;
        x->buf = aligned_alloc(64, (size_t)(size + 63) / 64 * 64);
        if (!x->buf) {
            fprintf(stderr, "pfread: no memory for %ld bytes\n", size);
            rc = -1;
            break;
        }
        int e = pthread_create(&x->t, 0, worker, x);
        if (e) {
            fprintf(stderr, "pfread: pthread_create %d of %d: %s\n", t + 1, n, ename(e));
            free(x->buf);
            x->buf = 0;
            rc = -1;
            break;
        }
        made++;
    }
    if (rc)
        __atomic_store_n(&stop, 1, __ATOMIC_SEQ_CST);
    /* All threads parked (or failed opening) before the gate opens; bounded at 10 s. */
    for (int i = 0; i < 10000 && __atomic_load_n(&ready, __ATOMIC_SEQ_CST) < made; i++)
        nanosleep(&(struct timespec){0, 1000000}, 0);
    if (__atomic_load_n(&ready, __ATOMIC_SEQ_CST) < made) {
        fprintf(stderr, "pfread: threads did not start within 10 s\n");
        __atomic_store_n(&stop, 1, __ATOMIC_SEQ_CST);
        rc = -1;
    }
    __atomic_store_n(&go, 1, __ATOMIC_RELEASE);
    futex_wake_all(&go);
    if (!rc && !limit) {
        nanosleep(&(struct timespec){ms / 1000, ms % 1000 * 1000000}, 0);
        __atomic_store_n(&stop, 1, __ATOMIC_RELAXED);
    }
    for (int t = 0; t < made; t++)
        pthread_join(w[t].t, 0);
    memset(res, 0, sizeof *res);
    uint64_t first = UINT64_MAX, last = 0;
    for (int t = 0; t < made; t++) {
        struct worker *x = &w[t];
        if (x->err) {
            if (x->err > 0)
                fprintf(stderr, "pfread: %s in thread %d: %s\n", x->what, t, ename(x->err));
            else
                fprintf(stderr, "pfread: short pread in thread %d: %ld of %ld\n", t, x->got, x->size);
            rc = -1;
        } else if (x->bad) {
            fprintf(stderr, "pfread: thread %d read %ld wrong bytes\n", t, x->bad);
            rc = -1;
        } else if (!rc) {
            res->reads += x->reads;
            res->tsc += x->tsc;
            res->thread_ns += x->ns1 - x->ns0;
            first = x->ns0 < first ? x->ns0 : first;
            last = x->ns1 > last ? x->ns1 : last;
        }
        free(x->buf);
    }
    res->wall_ns = last > first ? last - first : 0;
    if (shared >= 0)
        close(shared);
    free(w);
    return rc;
}

static int check(const char *dir) {
    struct fsdir d = {dir, dir, 0, 0};
    static const long sz[] = {16, CHUNK, FILE_SZ};
    if (make_files(&d, 2)) {
        printf("pfread check: files: failed\n");
        return 1;
    }
    int bad = 0;
    for (int l = 0; l < LAYOUTS; l++)
        for (int s = 0; s < 3; s++) {
            struct result r;
            int rc = run_case(&d, (enum layout)l, sz[s], 2, 3, 0, &r);
            printf("pfread check: %s size=%ld n=2: %s\n", layout_name[l], sz[s],
                   rc ? "failed" : r.reads == 6 ? "ok" : "wrong count");
            bad |= rc || r.reads != 6;
        }
    remove_files(&d);
    int gone = 1;
    for (int i = 0; i < 3; i++)
        gone &= open(d.paths[i], O_RDONLY | O_CLOEXEC) < 0 && errno == ENOENT;
    yes("pfread check: files removed", gone);
    return bad || !gone;
}

static int parse_list(const char *s, long **out) {
    int n = 1;
    for (const char *p = s; *p; p++)
        n += *p == ',';
    *out = malloc((size_t)n * sizeof **out);
    if (!*out)
        return 0;
    int k = 0;
    for (const char *p = s; *p;) {
        char *e;
        long v = strtol(p, &e, 10);
        if (e == p || v <= 0)
            return 0;
        (*out)[k++] = v;
        p = *e == ',' ? e + 1 : e;
        if (*e && *e != ',')
            return 0;
    }
    return k;
}

static int usage(void) {
    fprintf(stderr, "usage: abi-pfread [check [dir]]\n"
                    "       abi-pfread bench [ms=N] [threads=1,2,...] [sizes=16,...] [layouts=a,b] [dir[:label] ...]\n"
                    "       abi-pfread clean [dir ...]\n");
    return 2;
}

static int bench(int argc, char **argv) {
    long ms = 250, *thr = 0, *sz = 0;
    int nthr = 0, nsz = 0, lay[LAYOUTS] = {1, 1, 1}, nd = 0;
    struct fsdir *dirs = calloc((size_t)argc + 1, sizeof *dirs);
    if (!dirs)
        return 1;
    for (int i = 0; i < argc; i++) {
        char *a = argv[i];
        if (!strncmp(a, "ms=", 3)) {
            ms = strtol(a + 3, 0, 10);
            if (ms <= 0)
                return usage();
        } else if (!strncmp(a, "threads=", 8)) {
            if (!(nthr = parse_list(a + 8, &thr)))
                return usage();
        } else if (!strncmp(a, "sizes=", 6)) {
            if (!(nsz = parse_list(a + 6, &sz)))
                return usage();
            for (int k = 0; k < nsz; k++)
                if (sz[k] > FILE_SZ)
                    return usage();
        } else if (!strncmp(a, "layouts=", 8)) {
            memset(lay, 0, sizeof lay);
            for (char *p = strtok(a + 8, ","); p; p = strtok(0, ",")) {
                int l = 0;
                while (l < LAYOUTS && strcmp(p, layout_name[l]))
                    l++;
                if (l == LAYOUTS)
                    return usage();
                lay[l] = 1;
            }
        } else {
            char *c = strchr(a, ':');
            if (c)
                *c = 0;
            dirs[nd].dir = a;
            dirs[nd].label = c ? c + 1 : a;
            nd++;
        }
    }
    static long def_thr[] = {1, 2, 4, 8, 16}, def_sz[] = {16, CHUNK, FILE_SZ};
    if (!nthr)
        thr = def_thr, nthr = 5;
    if (!nsz)
        sz = def_sz, nsz = 3;
    if (!nd)
        dirs[nd++] = (struct fsdir){"/tmp", "tmpfs", 0, 0};
    long maxn = 0;
    for (int k = 0; k < nthr; k++)
        maxn = thr[k] > maxn ? thr[k] : maxn;

    /* Rows are kept for the table at the end; the PF1 lines go out as each case ends. */
    int rows = nd * LAYOUTS * nsz * nthr, nrow = 0;
    char (*row)[128] = calloc((size_t)rows, sizeof *row);
    if (!row)
        return 1;
    int rc = 0;
    for (int di = 0; di < nd && !rc; di++) {
        struct fsdir *d = &dirs[di];
        live_dir = d;
        if (make_files(d, (int)maxn)) {
            fprintf(stderr, "pfread: cannot set up %ld files of %ld bytes in %s\n", maxn + 1, FILE_SZ, d->dir);
            rc = 1;
            break;
        }
        for (int l = 0; l < LAYOUTS && !rc; l++)
            for (int s = 0; s < nsz && lay[l] && !rc; s++)
                for (int k = 0; k < nthr && !rc; k++) {
                    struct result r;
                    if (run_case(d, (enum layout)l, sz[s], (int)thr[k], 0, ms, &r)) {
                        rc = 1;
                        break;
                    }
                    unsigned long long cyc = r.reads ? r.tsc / r.reads : 0;
                    unsigned long long rps = r.wall_ns ? r.reads * 1000000000ull / r.wall_ns : 0;
                    double mibs = (double)rps * (double)sz[s] / (1 << 20);
                    unsigned long long mhz = r.thread_ns ? r.tsc * 1000 / r.thread_ns : 0;
                    printf("PF1 fs=%s layout=%s size=%ld n=%ld reads=%lu wall_us=%llu cyc_per_read=%llu "
                           "reads_per_s=%llu mib_per_s=%.1f tsc_mhz=%llu\n",
                           d->label, layout_name[l], sz[s], thr[k], r.reads,
                           (unsigned long long)(r.wall_ns / 1000), cyc, rps, mibs, mhz);
                    fflush(stdout);
                    snprintf(row[nrow++], sizeof row[0], "%-10s %-15s %8ld %3ld %12llu %12llu %10.1f", d->label,
                             layout_name[l], sz[s], thr[k], cyc, rps, mibs);
                }
        remove_files(d);
        live_dir = 0;
    }
    printf("\n%-10s %-15s %8s %3s %12s %12s %10s\n", "fs", "layout", "size", "n", "cyc/read", "reads/s", "MiB/s");
    for (int i = 0; i < nrow; i++)
        printf("%s\n", row[i]);
    return rc;
}

int main(int argc, char **argv) {
    if (argc == 1)
        return check("/tmp");
    if (!strcmp(argv[1], "check") && argc <= 3)
        return check(argc == 3 ? argv[2] : "/tmp");
    if (!strcmp(argv[1], "bench")) {
        catch_signals();
        return bench(argc - 2, argv + 2);
    }
    if (!strcmp(argv[1], "clean"))
        return clean(argc - 2, argv + 2);
    return usage();
}

#pragma clang attribute pop
