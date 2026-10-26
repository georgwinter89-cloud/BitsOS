/* fs-memfd-lazy: tmpfs and memfd pages charged when they are allocated, not at ftruncate (FS-14), as programs see
 * it, for the ABI diff (CachyOS is the oracle; user/abi-tests/c/memfd-lazy.c is the same file) and the ktests
 * fs.memfd.lazy*. Linux: memfds and tmpfs files are VM_NORESERVE (linux-6.6/mm/memfd.c:363, mm/shmem.c:1153
 * shmem_setattr writes i_size only; shmem_inode_acct_block :210 charges each new page). Prints results, errno
 * names and st_blocks (512-byte units) only. Modes (argv[1]):
 *   pools   foot 1.28's buffer pools: 4 x (memfd_create, ftruncate 512 MiB, mmap MAP_SHARED, touch one page)
 *   blocks  st_blocks counts allocated pages only: ftruncate, writes, a far write, read faults, hole punch,
 *           fallocate KEEP_SIZE past the end and a truncate to the same size dropping those pages
 *   limit   Linux without root: a user and mount namespace with a 256 KiB tmpfs at a fresh directory under /tmp,
 *           then `at`'s probes (V2 has no unshare/mount: the setup line differs there, a known case)
 *   at DIR  the probes on DIR, which must be an empty tmpfs of exactly 64 pages (the ktest makes it in the kernel):
 *           a growing ftruncate past the size, a short write at the size then ENOSPC, fallocate all or nothing,
 *           a fault past the size (SIGBUS in a child), a hole punch giving pages back */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define PG 4096L
#define POOL (512L << 20)
#define FL_KEEP_SIZE 1
#define FL_PUNCH_HOLE 3 /* PUNCH_HOLE always with KEEP_SIZE */

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "0";
    case EPERM: return "EPERM";
    case ENOMEM: return "ENOMEM";
    case ENOSPC: return "ENOSPC";
    case EINVAL: return "EINVAL";
    case EFBIG: return "EFBIG";
    case ENOSYS: return "ENOSYS";
    case EACCES: return "EACCES";
    case ENOENT: return "ENOENT";
    }
    snprintf(b, sizeof b, "errno%d", e);
    return b;
}

/* Every line goes out at once (a forked child must not copy buffered lines). */
static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...) {
    char line[256];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(line, sizeof line - 1, fmt, ap);
    va_end(ap);
    if (n < 0) return;
    if (n > (int)sizeof line - 2) n = sizeof line - 2;
    line[n++] = '\n';
    if (write(1, line, n) < 0) _exit(3);
}

static const char *r(long v) { return v < 0 ? en(errno) : "0"; }
static long blocks(int fd) {
    struct stat st;
    return fstat(fd, &st) ? -1 : (long)st.st_blocks;
}
static long size(int fd) {
    struct stat st;
    return fstat(fd, &st) ? -1 : (long)st.st_size;
}
static long falloc(int fd, int mode, long off, long len) { return syscall(SYS_fallocate, fd, mode, off, len); }
/* One step: its answer first, then what fstat says (C leaves the order of a call's arguments open). */
#define STEP(fd, label, answer)                                                     \
    do {                                                                            \
        const char *a_ = (answer);                                                  \
        say("%s: %s, st_blocks %ld, size %ld", label, a_, blocks(fd), size(fd));    \
    } while (0)
#define ONLY(label, answer)                                                         \
    do {                                                                            \
        const char *a_ = (answer);                                                  \
        say("%s: %s", label, a_);                                                   \
    } while (0)

/* A write's answer: bytes written or the errno name. */
static const char *wr(int fd, long len, long at) {
    static char b[32];
    char *buf = malloc(len);
    if (!buf) return "no buffer";
    memset(buf, 'w', len);
    long n = pwrite(fd, buf, len, at);
    int e = errno;
    free(buf);
    if (n < 0) return en(e);
    snprintf(b, sizeof b, "%ld pages", n / PG);
    if (n % PG) snprintf(b, sizeof b, "%ld bytes", n);
    return b;
}

static int pools(void) {
    for (int i = 0; i < 4; i++) {
        int fd = memfd_create("lazy-pool", MFD_CLOEXEC | MFD_ALLOW_SEALING);
        if (fd < 0) {
            say("pool %d: memfd_create %s", i, en(errno));
            return 1;
        }
        say("pool %d", i);
        STEP(fd, "ftruncate 512 MiB", r(ftruncate(fd, POOL)));
        char *p = mmap(NULL, POOL, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (p == MAP_FAILED) {
            say("mmap %s", en(errno));
            return 1;
        }
        p[0] = 1;
        STEP(fd, "one page touched", "0");
        /* foot gives the tail back with a hole punch after it shrank its buffers. */
        p[PG] = 1;
        STEP(fd, "punch from page 1 on", r(falloc(fd, FL_PUNCH_HOLE, PG, POOL - PG)));
    }
    say("pools ok");
    return 0;
}

static int blocks_mode(void) {
    int fd = memfd_create("lazy-blocks", MFD_CLOEXEC);
    if (fd < 0) {
        say("memfd_create %s", en(errno));
        return 1;
    }
    STEP(fd, "ftruncate 1 MiB", r(ftruncate(fd, 256 * PG)));
    STEP(fd, "write 4 pages", wr(fd, 4 * PG, 0));
    STEP(fd, "write 1 byte at page 100", wr(fd, 1, 100 * PG));
    char b[16];
    STEP(fd, "read a hole", pread(fd, b, sizeof b, 50 * PG) == sizeof b ? "16 bytes" : en(errno));
    char *p = mmap(NULL, 256 * PG, PROT_READ, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) {
        say("mmap %s", en(errno));
        return 1;
    }
    volatile char c = p[60 * PG];
    (void)c;
    STEP(fd, "read fault in a hole (shared)", "0");
    munmap(p, 256 * PG);
    STEP(fd, "punch pages 0-1", r(falloc(fd, FL_PUNCH_HOLE, 0, 2 * PG)));
    STEP(fd, "punch inside page 2", r(falloc(fd, FL_PUNCH_HOLE, 2 * PG + 10, 100)));
    STEP(fd, "fallocate 4 pages at page 300 KEEP_SIZE", r(falloc(fd, FL_KEEP_SIZE, 300 * PG, 4 * PG)));
    STEP(fd, "ftruncate to the same size", r(ftruncate(fd, 256 * PG)));
    STEP(fd, "fallocate pages 10-19", r(falloc(fd, 0, 10 * PG, 10 * PG)));
    STEP(fd, "ftruncate 1 page", r(ftruncate(fd, PG)));
    close(fd);
    return 0;
}

static int at(const char *dir) {
    char path[256];
    snprintf(path, sizeof path, "%s/lazy", dir);
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (fd < 0) {
        say("open %s", en(errno));
        return 1;
    }
    unlink(path);
    STEP(fd, "ftruncate 1 GiB on a 64-page tmpfs", r(ftruncate(fd, 1L << 30)));
    STEP(fd, "ftruncate 1 MiB", r(ftruncate(fd, 256 * PG)));
    STEP(fd, "write 80 pages", wr(fd, 80 * PG, 0));
    ONLY("write 1 byte at page 70", wr(fd, 1, 70 * PG));
    ONLY("fallocate 1 page at page 70", r(falloc(fd, 0, 70 * PG, PG)));
    STEP(fd, "fallocate 65 pages", r(falloc(fd, FL_KEEP_SIZE, 100 * PG, 65 * PG)));
    pid_t pid = fork();
    if (pid < 0) {
        say("fork %s", en(errno));
    } else if (pid == 0) {
        struct rlimit no_core = {0, 0};
        setrlimit(RLIMIT_CORE, &no_core);
        char *p = mmap(NULL, 256 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        if (p == MAP_FAILED) _exit(2);
        p[100 * PG] = 1;
        _exit(0);
    } else {
        int st = 0;
        waitpid(pid, &st, 0);
        if (WIFSIGNALED(st)) say("fault past the size: signal %s", WTERMSIG(st) == SIGBUS ? "SIGBUS" : "other");
        else say("fault past the size: exit %d", WEXITSTATUS(st));
    }
    STEP(fd, "punch pages 0-7", r(falloc(fd, FL_PUNCH_HOLE, 0, 8 * PG)));
    STEP(fd, "write 1 byte at page 70", wr(fd, 1, 70 * PG));
    STEP(fd, "fallocate 8 pages at page 80", r(falloc(fd, 0, 80 * PG, 8 * PG)));
    STEP(fd, "fallocate 7 pages at page 80", r(falloc(fd, 0, 80 * PG, 7 * PG)));
    STEP(fd, "ftruncate 0", r(ftruncate(fd, 0)));
    STEP(fd, "write 64 pages again", wr(fd, 64 * PG, 0));
    close(fd);
    return 0;
}

static int limit(void) {
    uid_t uid = getuid();
    gid_t gid = getgid();
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS) < 0) {
        say("setup: %s", en(errno));
        return 1;
    }
    char m[64];
    int f = open("/proc/self/setgroups", O_WRONLY);
    if (f >= 0) {
        if (write(f, "deny", 4) < 0) {}
        close(f);
    }
    snprintf(m, sizeof m, "0 %u 1", uid);
    f = open("/proc/self/uid_map", O_WRONLY);
    if (f < 0 || write(f, m, strlen(m)) < 0) {
        say("setup: uid_map %s", en(errno));
        return 1;
    }
    close(f);
    snprintf(m, sizeof m, "0 %u 1", gid);
    f = open("/proc/self/gid_map", O_WRONLY);
    if (f >= 0) {
        if (write(f, m, strlen(m)) < 0) {}
        close(f);
    }
    char dir[] = "/tmp/fs14-XXXXXX";
    if (!mkdtemp(dir)) {
        say("setup: mkdtemp %s", en(errno));
        return 1;
    }
    if (mount("tmpfs", dir, "tmpfs", 0, "size=256k,huge=never") < 0) {
        say("setup: mount %s", en(errno));
        rmdir(dir);
        return 1;
    }
    say("setup: 64-page tmpfs");
    int rc = at(dir);
    umount(dir);
    rmdir(dir);
    return rc;
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "pools";
    if (!strcmp(m, "pools")) return pools();
    if (!strcmp(m, "blocks")) return blocks_mode();
    if (!strcmp(m, "limit")) return limit();
    if (!strcmp(m, "at") && argc > 2) return at(argv[2]);
    say("unknown mode %s", m);
    return 2;
}
