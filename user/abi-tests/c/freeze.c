/* SC-7c: the programs of the freeze suite (host/tests/freeze/, B4 "the desktop never freezes"). V2 only, not an
 * ABI-diff case (it prints times). The scenario starts them through MCP `run`:
 *   probe SECS LIMIT_MS [PERIOD_US]  the program in front (MCP sched_focus makes it Interactive): one thread sleeps
 *                                    to absolute deadlines PERIOD_US apart (default 2000) and measures how late it
 *                                    wakes; one thread reads the MCP keyboard (EVIOCSCLOCKID CLOCK_MONOTONIC, as
 *                                    libinput) and measures each key event (press, release, repeat) from its event
 *                                    time to the read. Exit 0 when no wake and no key event was later than LIMIT_MS
 *                                    and no SYN_DROPPED came. "missed" counts the periods a stall swallowed (the
 *                                    lateness of the stall itself is one sample).
 *   hold [nopi-ok]                   a hung program: holds a PI futex (FUTEX_LOCK_PI) forever, one thread blocked in
 *                                    FUTEX_LOCK_PI on it (priority passing to a sleeping owner), two in FUTEX_WAIT
 *                                    on a word nobody wakes; it ends only by a signal. LOCK_PI failing (Linux never
 *                                    answers ENOSYS there) is exit 1; only with "nopi-ok" (the suite's run before
 *                                    the SC-7b integration) does ENOSYS give plain waits and "pi=no".
 *   hog PATH SECS                    a disk hog: 256 KiB writes over a 4 MiB file with an fsync after every MiB
 *                                    and a read back of every fourth chunk, for SECS seconds; then the file goes.
 *                                    (MCP programs run as uid 1000 and cannot write the stick's root-owned
 *                                    \BITSOS2\; the scenario then runs it on /tmp, page cache only, and the
 *                                    device load is FS-12's in-kernel hog.)
 * Without arguments (the ABI diff's default case, both sides): the usage line, exit 2; it never opens a device. */
#include "abi.h"
#include <fcntl.h>
#include <linux/futex.h>
#include <linux/input.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <sys/ioctl.h>

static uint64_t mono_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}

/* Growing sample list (R1: no fixed count). */
struct samples {
    uint64_t *v;
    size_t n, cap;
    int oom;
};

static void add(struct samples *s, uint64_t x) {
    if (s->n == s->cap) {
        size_t cap = s->cap ? s->cap * 2 : 1024;
        uint64_t *v = realloc(s->v, cap * sizeof *v);
        if (!v) {
            s->oom = 1;
            return;
        }
        s->v = v;
        s->cap = cap;
    }
    s->v[s->n++] = x;
}

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

/* "max_us=.. p50_us=.. p99_us=.." of the samples (ns). */
static void stats(struct samples *s, char *out, size_t len) {
    if (s->n == 0) {
        snprintf(out, len, "max_us=0 p50_us=0 p99_us=0");
        return;
    }
    qsort(s->v, s->n, sizeof *s->v, cmp_u64);
    snprintf(out, len, "max_us=%llu p50_us=%llu p99_us=%llu", (unsigned long long)(s->v[s->n - 1] / 1000),
             (unsigned long long)(s->v[s->n / 2] / 1000), (unsigned long long)(s->v[(s->n * 99) / 100] / 1000));
}

static uint64_t end_ns, limit_ns, period_ns;
static volatile int stop_all;

static unsigned long missed;

static void *deadlines(void *arg) {
    struct samples *s = arg;
    uint64_t target = mono_ns();
    while (!stop_all) {
        target += period_ns;
        uint64_t now = mono_ns();
        /* After a long stall: the next deadline from now, so the stall is one lateness sample; the periods it
         * swallowed are counted in `missed`, so a run of misses stays visible. */
        if (target + period_ns < now) {
            missed += (now - target) / period_ns;
            target = now + period_ns;
        }
        if (target >= end_ns)
            break;
        struct timespec ts = { (time_t)(target / 1000000000ull), (long)(target % 1000000000ull) };
        while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &ts, NULL) == EINTR) {
        }
        now = mono_ns();
        add(s, now > target ? now - target : 0);
    }
    return NULL;
}

struct reader {
    int fd;
    struct samples lat;
    unsigned long presses, dropped, errors;
};

static void *input(void *arg) {
    struct reader *r = arg;
    struct input_event ev[64];
    while (!stop_all && mono_ns() < end_ns) {
        struct pollfd p = { r->fd, POLLIN, 0 };
        int n = poll(&p, 1, 100);
        if (n <= 0)
            continue;
        ssize_t got = read(r->fd, ev, sizeof ev);
        uint64_t now = mono_ns();
        if (got < 0) {
            if (errno == EAGAIN || errno == EINTR)
                continue;
            /* The node went away (ENODEV, POLLERR/POLLHUP stays set): count it and stop, never spin on poll. */
            r->errors++;
            break;
        }
        if (got == 0 || (p.revents & (POLLERR | POLLHUP | POLLNVAL))) {
            r->errors++;
            break;
        }
        for (size_t i = 0; i < (size_t)got / sizeof ev[0]; i++) {
            if (ev[i].type == EV_SYN && ev[i].code == SYN_DROPPED)
                r->dropped++;
            if (ev[i].type != EV_KEY)
                continue;
            r->presses += ev[i].value == 1;
            uint64_t t = (uint64_t)ev[i].input_event_sec * 1000000000ull + (uint64_t)ev[i].input_event_usec * 1000ull;
            /* The event time is microseconds, so up to 1 us after now is the same moment. */
            add(&r->lat, now + 1000 > t ? now + 1000 - t : 0);
        }
    }
    return NULL;
}

static int find_keyboard(void) {
    for (int i = 0;; i++) {
        char path[32], name[256] = { 0 };
        snprintf(path, sizeof path, "/dev/input/event%d", i);
        int fd = open(path, O_RDONLY | O_NONBLOCK);
        if (fd < 0) {
            if (errno == ENOENT)
                return -1;
            continue;
        }
        if (ioctl(fd, EVIOCGNAME(sizeof name - 1), name) >= 0 && strcmp(name, "BitsOS MCP keyboard") == 0)
            return fd;
        close(fd);
    }
}

static unsigned long over(const struct samples *s) {
    unsigned long n = 0;
    for (size_t i = 0; i < s->n; i++)
        n += s->v[i] > limit_ns;
    return n;
}

static int probe(int secs, int limit_ms, int period_us) {
    struct reader r = { .fd = find_keyboard() };
    if (r.fd < 0) {
        printf("freeze probe: no MCP keyboard node\n");
        return 1;
    }
    int clk = CLOCK_MONOTONIC;
    if (ioctl(r.fd, EVIOCSCLOCKID, &clk) < 0) {
        printf("freeze probe: EVIOCSCLOCKID %s\n", ename(errno));
        return 1;
    }
    struct input_event drain[64];
    while (read(r.fd, drain, sizeof drain) > 0) {
    }
    limit_ns = (uint64_t)limit_ms * 1000000ull;
    period_ns = (uint64_t)period_us * 1000ull;
    end_ns = mono_ns() + (uint64_t)secs * 1000000000ull;
    struct samples wake = { 0 };
    pthread_t a, b;
    if (pthread_create(&a, NULL, deadlines, &wake) || pthread_create(&b, NULL, input, &r)) {
        printf("freeze probe: pthread_create failed\n");
        return 1;
    }
    /* Both threads exist before the scenario reads this line and grants the focus to the whole process. */
    printf("freeze probe ready pid=%d\n", getpid());
    fflush(stdout);
    pthread_join(a, NULL);
    pthread_join(b, NULL);
    char ws[96], is[96];
    unsigned long late = over(&wake), in_late = over(&r.lat);
    stats(&wake, ws, sizeof ws);
    stats(&r.lat, is, sizeof is);
    printf("freeze probe deadlines=%zu late=%lu %s missed=%lu limit_ms=%d period_us=%d\n", wake.n, late, ws, missed,
           limit_ms, period_us);
    printf("freeze probe input presses=%lu key_events=%zu late=%lu %s dropped=%lu errors=%lu\n", r.presses, r.lat.n,
           in_late, is, r.dropped, r.errors);
    int ok = !late && !in_late && !r.dropped && !r.errors && !wake.oom && !r.lat.oom && wake.n > 0;
    printf("freeze probe %s\n", ok ? "green" : "red");
    return ok ? 0 : 1;
}

/* ---- hold */

static uint32_t pi_word, plain_word;

static long futex(uint32_t *u, int op, uint32_t val) {
    return syscall(SYS_futex, u, op, val, NULL, NULL, 0);
}

static void *pi_waiter(void *arg) {
    (void)arg;
    long r = futex(&pi_word, FUTEX_LOCK_PI_PRIVATE, 0);
    printf("freeze hold: lock_pi returned %ld %s\n", r, r < 0 ? ename(errno) : "");
    fflush(stdout);
    return NULL;
}

static void *plain_waiter(void *arg) {
    (void)arg;
    for (;;) {
        long r = futex(&plain_word, FUTEX_WAIT_PRIVATE, 0);
        if (r < 0 && errno != EINTR && errno != EAGAIN) {
            printf("freeze hold: wait %s\n", ename(errno));
            fflush(stdout);
            return NULL;
        }
    }
}

static int hold(int nopi_ok) {
    pthread_t a, b, c;
    if (futex(&pi_word, FUTEX_LOCK_PI_PRIVATE, 0) < 0) {
        /* Only the suite's pre-integration run accepts a kernel without the PI operations (ENOSYS): it hangs in
         * plain waits and says so. */
        int e = errno;
        printf("freeze hold: lock_pi %s\n", ename(e));
        if (e != ENOSYS || !nopi_ok)
            return 1;
        if (pthread_create(&a, NULL, plain_waiter, NULL) || pthread_create(&b, NULL, plain_waiter, NULL)) {
            printf("freeze hold: pthread_create failed\n");
            return 1;
        }
        printf("freeze hold ready pid=%d pi=no\n", getpid());
        fflush(stdout);
        for (;;)
            pause();
    }
    if (pthread_create(&a, NULL, pi_waiter, NULL) || pthread_create(&b, NULL, plain_waiter, NULL) ||
        pthread_create(&c, NULL, plain_waiter, NULL)) {
        printf("freeze hold: pthread_create failed\n");
        return 1;
    }
    /* The PI waiter is in the kernel once the word carries FUTEX_WAITERS. */
    for (int i = 0; i < 5000 && !(__atomic_load_n(&pi_word, __ATOMIC_ACQUIRE) & FUTEX_WAITERS); i++)
        usleep(1000);
    uint32_t w = __atomic_load_n(&pi_word, __ATOMIC_ACQUIRE);
    printf("freeze hold ready pid=%d pi=yes owner=%s waiters=%s\n", getpid(),
           (w & FUTEX_TID_MASK) == (uint32_t)syscall(SYS_gettid) ? "me" : "other", w & FUTEX_WAITERS ? "yes" : "no");
    fflush(stdout);
    for (;;)
        pause();
}

/* ---- hog */

static int hog(const char *path, int secs) {
    const size_t chunk = 256 * 1024, chunks = 16;
    unsigned char *buf = malloc(chunk), *back = malloc(chunk);
    if (!buf || !back) {
        printf("freeze hog: no memory\n");
        return 1;
    }
    unlink(path);
    int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644);
    if (fd < 0) {
        printf("freeze hog: open %s %s\n", path, ename(errno));
        return 1;
    }
    printf("freeze hog ready pid=%d\n", getpid());
    fflush(stdout);
    uint64_t end = mono_ns() + (uint64_t)secs * 1000000000ull, fsync_max = 0, bytes = 0;
    unsigned long writes = 0, fsyncs = 0, bad = 0;
    int err = 0;
    for (unsigned long i = 0; mono_ns() < end; i++) {
        size_t c = i % chunks;
        memset(buf, (int)(i & 0xff), chunk);
        if (pwrite(fd, buf, chunk, (off_t)(c * chunk)) != (ssize_t)chunk) {
            err = errno;
            break;
        }
        writes++;
        bytes += chunk;
        if (c % 4 == 3) {
            uint64_t t = mono_ns();
            if (fsync(fd) < 0) {
                err = errno;
                break;
            }
            t = mono_ns() - t;
            fsync_max = t > fsync_max ? t : fsync_max;
            fsyncs++;
            if (pread(fd, back, chunk, (off_t)(c * chunk)) != (ssize_t)chunk || memcmp(back, buf, chunk) != 0)
                bad++;
        }
    }
    close(fd);
    unlink(path);
    printf("freeze hog writes=%lu mib=%llu fsyncs=%lu fsync_max_ms=%llu bad_reads=%lu error=%s\n", writes,
           (unsigned long long)(bytes >> 20), fsyncs, (unsigned long long)(fsync_max / 1000000), bad, err ? ename(err) : "none");
    return err || bad || fsyncs == 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc >= 4 && strcmp(argv[1], "probe") == 0)
        return probe(atoi(argv[2]), atoi(argv[3]), argc >= 5 && atoi(argv[4]) >= 100 ? atoi(argv[4]) : 2000);
    if (argc >= 2 && strcmp(argv[1], "hold") == 0)
        return hold(argc >= 3 && strcmp(argv[2], "nopi-ok") == 0);
    if (argc >= 4 && strcmp(argv[1], "hog") == 0)
        return hog(argv[2], atoi(argv[3]));
    printf("usage: abi-freeze probe SECS LIMIT_MS [PERIOD_US] | hold [nopi-ok] | hog PATH SECS\n");
    return 2;
}
