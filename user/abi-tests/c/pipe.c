/* fs-pipe: pipes, FIFOs and eventfd (FS-3) as programs see them, for the ABI diff (CachyOS is the oracle;
 * user/abi-tests/c/pipe.c is the same file) and the ktests fs.pipe.* (two programs joined by a kernel pipe,
 * SIGPIPE, a kill while blocked, a FIFO open that waits). Prints results, errno names and yes/no relations only.
 * Modes (argv[1]):
 *   self          one process: a pipe through poll, FIONREAD, a full pipe, O_NONBLOCK, EOF, EPIPE (SIGPIPE
 *                 ignored), a FIFO in /tmp, eventfd; the line-by-line cases are in host/tests/fs/cases.txt
 *   sigpipe       a write to a pipe without a reader: the default SIGPIPE ends the program (signal 13)
 *   sigpipe-ign   the same with SIGPIPE ignored: EPIPE
 *   sigpipe-block the same with SIGPIPE blocked: EPIPE, and SIGPIPE is pending afterwards
 *   produce N     N bytes (byte i = i*7 % 251) to stdout in writes of 1..9000 bytes; exit 0 when all went
 *   produce-forever  4096-byte writes to stdout until something ends the program
 *   consume       stdin to its end: "got <bytes> sum <checksum>" (the checksum of produce's pattern check)
 *   block-read    one read of stdin (waits for data that never comes, until killed)
 *   fifo-read P   opens the FIFO P for reading (waits for a writer), reads to the end: "fifo got <n>: <text>"
 *   eventfd       eventfd2 with EFD_SEMAPHORE/EFD_NONBLOCK/EFD_CLOEXEC: counts, limits, EAGAIN, EINVAL, poll bits
 *   atomic        PIPE_BUF in one process: writes up to 4096 bytes go in whole or not at all (EAGAIN), larger
 *                 ones in part; page merging decides what fits
 *   records ID N  N records of 16..4096 bytes (one write each, so never torn) from writer ID to stdout
 *   check-records K N  reads stdin to its end and checks K writers' records: whole, in order, N each
 * Raw syscalls are not needed: musl's wrappers pass the arguments through. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

/* EFD_SEMAPHORE without <sys/eventfd.h> (the program also builds against the host's headers). */
#define EFD_SEMAPHORE_ 1
#ifndef F_GETPIPE_SZ
#define F_SETPIPE_SZ 1031
#define F_GETPIPE_SZ 1032
#endif

static const char *ename(int e) {
    static char other[24];
    switch (e) {
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EINTR: return "EINTR";
    case ENXIO: return "ENXIO";
    case EBADF: return "EBADF";
    case EAGAIN: return "EAGAIN";
    case EACCES: return "EACCES";
    case EFAULT: return "EFAULT";
    case EBUSY: return "EBUSY";
    case EEXIST: return "EEXIST";
    case EINVAL: return "EINVAL";
    case EMFILE: return "EMFILE";
    case ENOTTY: return "ENOTTY";
    case ESPIPE: return "ESPIPE";
    case EPIPE: return "EPIPE";
    case ENOSYS: return "ENOSYS";
    }
    snprintf(other, sizeof other, "errno %d", e);
    return other;
}

static void show(const char *label, long r) {
    if (r < 0) printf("%s: -%s\n", label, ename(errno));
    else printf("%s: %ld\n", label, r);
}

static void yes(const char *label, int c) {
    printf("%s: %s\n", label, c ? "yes" : "no");
}

static int revents(int fd, short ev) {
    struct pollfd p = {fd, ev, 0};
    if (poll(&p, 1, 0) < 0) return -errno;
    return p.revents;
}

static long nread(int fd) {
    int n = -1;
    if (ioctl(fd, FIONREAD, &n) < 0) return -errno;
    return n;
}

static int mode_self(void) {
    int p[2];
    show("pipe2", pipe2(p, O_NONBLOCK));
    struct stat st;
    fstat(p[0], &st);
    yes("fifo type", S_ISFIFO(st.st_mode));
    printf("perm: %o\n", st.st_mode & 07777);
    yes("owner is me", st.st_uid == getuid());
    yes("same inode both ends", ({ struct stat s2; fstat(p[1], &s2); s2.st_ino == st.st_ino && s2.st_dev == st.st_dev; }));
    printf("revents read end empty: %#x\n", revents(p[0], POLLIN | POLLOUT));
    printf("revents write end empty: %#x\n", revents(p[1], POLLIN | POLLOUT));
    long total = 0, r;
    static char page[4096];
    memset(page, 'p', sizeof page);
    while ((r = write(p[1], page, 1000)) == 1000) total += r;
    printf("filled with 1000-byte writes: %ld, then %s\n", total, r < 0 ? ename(errno) : "short");
    printf("revents write end full: %#x\n", revents(p[1], POLLOUT));
    printf("fionread: %ld\n", nread(p[0]));
    show("read 4096", read(p[0], page, sizeof page));
    printf("revents write end after a page: %#x\n", revents(p[1], POLLOUT));
    close(p[1]);
    printf("revents read end, writer gone: %#x\n", revents(p[0], POLLIN));
    while ((r = read(p[0], page, sizeof page)) > 0) total -= r;
    show("read at the end", r);
    printf("bytes left over: %ld\n", total - 4096);
    close(p[0]);

    /* EPIPE with SIGPIPE ignored; POLLERR on the write end */
    signal(SIGPIPE, SIG_IGN);
    show("pipe", pipe(p));
    close(p[0]);
    printf("revents write end, reader gone: %#x\n", revents(p[1], POLLOUT));
    show("write without reader", write(p[1], "x", 1));
    close(p[1]);

    /* a FIFO in /tmp */
    char path[64];
    snprintf(path, sizeof path, "/tmp/fs-pipe-%d", (int)getpid());
    show("mkfifo", mkfifo(path, 0600));
    show("mkfifo again", mkfifo(path, 0600));
    show("open write nonblock, no reader", open(path, O_WRONLY | O_NONBLOCK));
    int r1 = open(path, O_RDONLY | O_NONBLOCK);
    yes("open read nonblock", r1 >= 0);
    printf("revents before a writer: %#x\n", revents(r1, POLLIN));
    int w1 = open(path, O_WRONLY | O_NONBLOCK);
    yes("open write nonblock", w1 >= 0);
    show("write", write(w1, "through the fifo", 16));
    char b[64] = {0};
    show("read", read(r1, b, sizeof b));
    printf("text: %s\n", b);
    close(w1);
    printf("revents after the writer left: %#x\n", revents(r1, POLLIN));
    close(r1);
    show("unlink", unlink(path));

    /* eventfd */
    int e = (int)syscall(SYS_eventfd2, 2, O_NONBLOCK);
    yes("eventfd", e >= 0);
    uint64_t v = 0;
    show("read", read(e, &v, 8));
    printf("value: %llu\n", (unsigned long long)v);
    show("read empty", read(e, &v, 8));
    v = 40;
    show("write", write(e, &v, 8));
    v = 2;
    show("write", write(e, &v, 8));
    printf("revents: %#x\n", revents(e, POLLIN | POLLOUT));
    show("read", read(e, &v, 8));
    printf("value: %llu\n", (unsigned long long)v);
    fstat(e, &st);
    printf("eventfd mode: %o\n", st.st_mode);
    close(e);
    return 0;
}

static int mode_sigpipe(int how) {
    if (how == 1) signal(SIGPIPE, SIG_IGN);
    if (how == 2) {
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGPIPE);
        sigprocmask(SIG_BLOCK, &s, 0);
    }
    int p[2];
    if (pipe(p) < 0) return 1;
    close(p[0]);
    printf("writing\n");
    fflush(stdout);
    long r = write(p[1], "x", 1);
    show("write", r);
    if (how == 2) {
        sigset_t pend;
        sigpending(&pend);
        yes("SIGPIPE pending", sigismember(&pend, SIGPIPE));
    }
    return 0;
}

static int mode_eventfd(void) {
    int e = (int)syscall(SYS_eventfd2, 0, EFD_SEMAPHORE_ | O_NONBLOCK | O_CLOEXEC);
    yes("eventfd2 semaphore", e >= 0);
    printf("F_GETFD: %d\n", fcntl(e, F_GETFD));
    printf("F_GETFL: %#o\n", fcntl(e, F_GETFL));
    uint64_t v = 0;
    show("read empty", read(e, &v, 8));
    v = 3;
    show("write 3", write(e, &v, 8));
    for (int i = 0; i < 4; i++) {
        v = 0;
        long r = read(e, &v, 8);
        if (r < 0) show("read", r);
        else printf("read: %ld value %llu\n", r, (unsigned long long)v);
    }
    v = 0xfffffffffffffffeULL;
    show("write max-1", write(e, &v, 8));
    v = 1;
    show("write 1 more", write(e, &v, 8));
    printf("revents at max-1: %#x\n", revents(e, POLLIN | POLLOUT));
    v = 0xffffffffffffffffULL;
    show("write max", write(e, &v, 8));
    char small[4];
    show("read 4 bytes", read(e, small, 4));
    show("write 4 bytes", write(e, small, 4));
    show("lseek", lseek(e, 0, SEEK_SET));
    show("pread", pread(e, &v, 8, 0));
    printf("fionread: %ld\n", nread(e));
    show("F_GETPIPE_SZ", fcntl(e, F_GETPIPE_SZ));
    close(e);
    show("eventfd2 bad flag", syscall(SYS_eventfd2, 0, 2));
    /* a counter (no semaphore): one read takes all */
    e = (int)syscall(SYS_eventfd2, 5, O_NONBLOCK);
    v = 7;
    show("write 7", write(e, &v, 8));
    v = 0;
    show("read", read(e, &v, 8));
    printf("value: %llu\n", (unsigned long long)v);
    printf("revents empty: %#x\n", revents(e, POLLIN | POLLOUT));
    close(e);
    return 0;
}

static int mode_atomic(void) {
    int p[2];
    show("pipe2", pipe2(p, O_NONBLOCK));
    static char buf[16384];
    memset(buf, 'a', sizeof buf);
    show("write 3000", write(p[1], buf, 3000));
    show("write 4096", write(p[1], buf, 4096));
    printf("fionread: %ld\n", nread(p[0]));
    long total = 0, r;
    while ((r = write(p[1], buf, 4096)) == 4096) total += r;
    printf("then 4096-byte writes: %ld, then %s\n", total, r < 0 ? ename(errno) : "short");
    printf("fionread full: %ld\n", nread(p[0]));
    show("write 1000 when full", write(p[1], buf, 1000));
    show("write 8192 when full", write(p[1], buf, 8192));
    show("read 4096", read(p[0], buf, 4096));
    show("write 8192, one page free", write(p[1], buf, 8192));
    show("write 100", write(p[1], buf, 100));
    show("read 10000", read(p[0], buf, 10000));
    show("write 4096", write(p[1], buf, 4096));
    show("write 4096", write(p[1], buf, 4096));
    show("write 4097", write(p[1], buf, 4097));
    printf("fionread: %ld\n", nread(p[0]));
    close(p[0]);
    close(p[1]);
    return 0;
}

/* A record: 0xa5, writer id, length (16 bit), sequence (32 bit), then bytes from (id, seq, position). */
static long rec_len(long id, long seq) { return 16 + (seq * 977 + id * 131) % 4081; }
static unsigned char rec_byte(long id, long seq, long i) { return (unsigned char)(id * 31 + seq * 7 + i); }

static int mode_records(long id, long n) {
    static unsigned char buf[4096];
    for (long seq = 0; seq < n; seq++) {
        long len = rec_len(id, seq);
        buf[0] = 0xa5;
        buf[1] = (unsigned char)id;
        buf[2] = (unsigned char)len;
        buf[3] = (unsigned char)(len >> 8);
        for (int k = 0; k < 4; k++) buf[4 + k] = (unsigned char)(seq >> (8 * k));
        for (long i = 8; i < len; i++) buf[i] = rec_byte(id, seq, i);
        long r = write(1, buf, len);
        if (r != len) { fprintf(stderr, "records %ld: write of %ld gave %ld (%s)\n", id, len, r, r < 0 ? ename(errno) : "short"); return 1; }
    }
    return 0;
}

static int mode_check_records(long k, long n) {
    static unsigned char buf[3 * 4096];
    static long next[256];
    long have = 0, at = 0, recs = 0, r;
    if (k < 1 || k > 256) return 2;
    for (;;) {
        r = read(0, buf + have, sizeof buf - have);
        if (r < 0) { printf("check: read: %s\n", ename(errno)); return 1; }
        have += r;
        long off = 0;
        while (have - off >= 4) {
            unsigned char *b = buf + off;
            long len = b[2] | (long)b[3] << 8;
            if (b[0] != 0xa5 || b[1] >= k || len < 16 || len > 4096) { printf("torn at byte %ld: header %02x %02x len %ld\n", at, b[0], b[1], len); return 1; }
            if (have - off < len) break;
            long id = b[1], seq = 0;
            for (int q = 0; q < 4; q++) seq |= (long)b[4 + q] << (8 * q);
            if (seq != next[id] || len != rec_len(id, seq)) { printf("writer %ld: record %ld where %ld was due (len %ld)\n", id, seq, next[id], len); return 1; }
            for (long i = 8; i < len; i++)
                if (b[i] != rec_byte(id, seq, i)) { printf("torn at byte %ld: writer %ld record %ld byte %ld\n", at + i, id, seq, i); return 1; }
            next[id]++;
            recs++;
            off += len;
            at += len;
        }
        memmove(buf, buf + off, have - off);
        have -= off;
        if (r == 0) break;
    }
    if (have) { printf("%ld bytes left after the last record\n", have); return 1; }
    for (long id = 0; id < k; id++)
        if (next[id] != n) { printf("writer %ld: %ld records, want %ld\n", id, next[id], n); return 1; }
    printf("records %ld from %ld writers whole\n", recs, k);
    return 0;
}

/* A decimal number (no atol: built against the host's headers by xtask, which map strtol to glibc names). */
static long number(const char *s) {
    long v = 0;
    while (*s >= '0' && *s <= '9') v = v * 10 + (*s++ - '0');
    return v;
}

static unsigned char pat(long i) { return (unsigned char)(i * 7 % 251); }

static int mode_produce(long n) {
    static unsigned char buf[9000];
    long done = 0, step = 1;
    while (done < n) {
        long k = step;
        if (k > n - done) k = n - done;
        for (long i = 0; i < k; i++) buf[i] = pat(done + i);
        long r = write(1, buf, k);
        if (r <= 0) { fprintf(stderr, "produce: write: %s after %ld\n", r < 0 ? ename(errno) : "0", done); return 1; }
        done += r;
        step = step * 3 % 9001 + 1;
    }
    return 0;
}

static int mode_forever(void) {
    static char buf[4096];
    for (;;) {
        long r = write(1, buf, sizeof buf);
        if (r < 0) { fprintf(stderr, "forever: write: %s\n", ename(errno)); return 3; }
    }
}

static int mode_consume(void) {
    static unsigned char buf[5000];
    long total = 0, r;
    uint64_t sum = 0;
    int bad = 0;
    while ((r = read(0, buf, sizeof buf)) > 0) {
        for (long i = 0; i < r; i++) {
            if (buf[i] != pat(total + i)) bad = 1;
            sum = sum * 31 + buf[i];
        }
        total += r;
    }
    if (r < 0) { printf("consume: read: %s\n", ename(errno)); return 1; }
    printf("got %ld sum %llu pattern %s\n", total, (unsigned long long)sum, bad ? "broken" : "ok");
    return 0;
}

static int mode_block_read(void) {
    char b[16];
    long r = read(0, b, sizeof b);
    show("read", r);
    return 0;
}

static int mode_fifo_read(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { printf("open: %s\n", ename(errno)); return 1; }
    char b[256];
    long n = 0, r;
    while ((r = read(fd, b + n, sizeof b - 1 - n)) > 0) n += r;
    b[n] = 0;
    printf("fifo got %ld: %s\n", n, b);
    return 0;
}

/* A pipe end opened again through /proc/self/fd/N (fifo_open on the pipe's inode): any access mode, the same
 * pipe; an eventfd there answers ENXIO. */
static int mode_reopen(void) {
    int p[2];
    char path[64], b[16];
    if (pipe(p) != 0) {
        printf("pipe: %s\n", ename(errno));
        return 1;
    }
    write(p[1], "abc", 3);
    snprintf(path, sizeof path, "/proc/self/fd/%d", p[0]);
    int r = open(path, O_RDONLY);
    printf("read end again: %s\n", r >= 0 ? "opened" : ename(errno));
    if (r >= 0) {
        long n = read(r, b, sizeof b);
        printf("read through it: %ld %.*s\n", n, n > 0 ? (int)n : 0, b);
        struct stat a, c;
        printf("same pipe: %d\n", fstat(r, &a) == 0 && fstat(p[0], &c) == 0 && a.st_ino == c.st_ino && S_ISFIFO(a.st_mode));
        printf("flags: %s\n", (fcntl(r, F_GETFL) & O_ACCMODE) == O_RDONLY ? "read-only" : "other");
        close(r);
    }
    int w = open(path, O_WRONLY);
    printf("write end through the read end's link: %s\n", w >= 0 ? "opened" : ename(errno));
    if (w >= 0) {
        write(w, "xy", 2);
        long n = read(p[0], b, sizeof b);
        printf("read back: %ld %.*s\n", n, n > 0 ? (int)n : 0, b);
        close(w);
    }
    int e = (int)syscall(SYS_eventfd2, 0, 0);
    snprintf(path, sizeof path, "/proc/self/fd/%d", e);
    int x = open(path, O_RDWR);
    printf("eventfd again: %s\n", x >= 0 ? "opened" : ename(errno));
    close(e);
    close(p[0]);
    close(p[1]);
    return 0;
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "self";
    if (!strcmp(m, "self")) return mode_self();
    if (!strcmp(m, "sigpipe")) return mode_sigpipe(0);
    if (!strcmp(m, "sigpipe-ign")) return mode_sigpipe(1);
    if (!strcmp(m, "sigpipe-block")) return mode_sigpipe(2);
    if (!strcmp(m, "produce")) return mode_produce(argc > 2 ? number(argv[2]) : 100000);
    if (!strcmp(m, "produce-forever")) return mode_forever();
    if (!strcmp(m, "consume")) return mode_consume();
    if (!strcmp(m, "block-read")) return mode_block_read();
    if (!strcmp(m, "fifo-read") && argc > 2) return mode_fifo_read(argv[2]);
    if (!strcmp(m, "eventfd")) return mode_eventfd();
    if (!strcmp(m, "atomic")) return mode_atomic();
    if (!strcmp(m, "reopen")) return mode_reopen();
    if (!strcmp(m, "records") && argc > 3) return mode_records(number(argv[2]), number(argv[3]));
    if (!strcmp(m, "check-records") && argc > 3) return mode_check_records(number(argv[2]), number(argv[3]));
    fprintf(stderr, "fs-pipe: unknown mode %s\n", m);
    return 2;
}
