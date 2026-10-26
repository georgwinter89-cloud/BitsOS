/* fs-memfd: memfd_create and file seals (FS-10) as programs see them, for the ABI diff (CachyOS is the oracle;
 * user/abi-tests/c/memfd.c is the same file) and the ktests fs.memfd.*. Prints results, errno names and yes/no
 * relations only (no addresses, pids, devices or inode numbers). Modes (argv[1]):
 *   basic          one process: names in /proc/self/fd and /proc/self/maps, seals against write, truncate and
 *                  shared mappings (EBUSY, EPERM, read-only mappings of a sealed file, mprotect EACCES), private
 *                  copies, reopening through /proc/self/fd, /dev/shm files' seals, fallocate (posix_fallocate,
 *                  hole punch seen through a shared mapping, seals, hostile sizes), vm.memfd_noexec
 *   wl-fork        the Wayland wl_shm path with the fd inherited by fork: the client makes and draws a buffer, the
 *                  compositor (the child) maps it read-only and sees both frames
 *   wl-socket      the same with the fd passed over a socketpair (SCM_RIGHTS), as Wayland does it
 *   wl-client      the client half alone (ktest fs.memfd.wl_shm: the kernel passes its fd 3 to the compositor as
 *                  that one's stdin, as SCM_RIGHTS would, and steers it through stdin: "next", then "end")
 *   wl-compositor  the compositor half alone, the buffer on fd 0
 * The client's and the compositor's lines are the same in every wl mode; the ktest compares each half with the
 * wl-fork transcript recorded on CachyOS (host/tests/fs/memfd-linux.txt). Raw syscalls: musl's wrappers pass the
 * arguments through. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#define F_GET_SEALS 1034
#endif
#ifndef F_SEAL_SEAL
#define F_SEAL_SEAL 0x1
#define F_SEAL_SHRINK 0x2
#define F_SEAL_GROW 0x4
#define F_SEAL_WRITE 0x8
#endif
#ifndef F_SEAL_FUTURE_WRITE
#define F_SEAL_FUTURE_WRITE 0x10
#endif
#ifndef F_SEAL_EXEC
#define F_SEAL_EXEC 0x20
#endif
#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x1
#define MFD_ALLOW_SEALING 0x2
#endif
#ifndef MFD_NOEXEC_SEAL
#define MFD_NOEXEC_SEAL 0x8
#endif
#ifndef MFD_EXEC
#define MFD_EXEC 0x10
#endif

#define PG 4096
/* The buffer: 256x192 ARGB pixels; word 0 is the frame counter the compositor waits on. */
#define W 256
#define H 192
#define BYTES (W * H * 4)

static const char *ename(int e) {
    static char other[24];
    switch (e) {
    case 0: return "0";
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EBADF: return "EBADF";
    case ENOMEM: return "ENOMEM";
    case EACCES: return "EACCES";
    case EFAULT: return "EFAULT";
    case EBUSY: return "EBUSY";
    case EINVAL: return "EINVAL";
    case ENODEV: return "ENODEV";
    case ENXIO: return "ENXIO";
    case ELOOP: return "ELOOP";
    case ENOSPC: return "ENOSPC";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ENOSYS: return "ENOSYS";
    }
    snprintf(other, sizeof other, "errno%d", e);
    return other;
}

/* Every line goes out at once (fork must not copy buffered lines; the halves' lines are compared one by one). */
/* fallocate modes (linux/falloc.h), raw: libc headers differ in which they define. */
#define FL_KEEP_SIZE 1
#define FL_PUNCH_HOLE 2
static long falloc(int fd, int mode, long off, long len) { return syscall(SYS_fallocate, fd, mode, off, len); }

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
#include <stdarg.h>
static void say(const char *fmt, ...) {
    char line[512];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(line, sizeof line, fmt, ap);
    va_end(ap);
    if (n > (int)sizeof line - 2) n = sizeof line - 2;
    line[n++] = '\n';
    ssize_t off = 0;
    while (off < n) {
        ssize_t w = write(1, line + off, n - off);
        if (w <= 0) return;
        off += w;
    }
}

static void res(const char *label, long r) { say("%s: %s", label, r < 0 ? ename(errno) : "0"); }
static void yes(const char *label, int cond) { say("%s: %s", label, cond ? "yes" : "no"); }

static long rmmap(void *addr, unsigned long len, unsigned long prot, unsigned long flags, long fd, unsigned long off) {
    return syscall(SYS_mmap, addr, len, prot, flags, fd, off);
}

/* mmap that prints "label: mapped" or the errno, and gives the mapping back (NULL when refused). */
static void *map(const char *label, unsigned long len, unsigned long prot, unsigned long flags, int fd) {
    errno = 0;
    long r = rmmap(0, len, prot, flags, fd, 0);
    say("%s: %s", label, r == -1 ? ename(errno) : "mapped");
    return r == -1 ? NULL : (void *)r;
}

static int memfd(const char *name, unsigned flags) { return (int)syscall(SYS_memfd_create, name, flags); }

static void link_of(int fd, char *out, size_t n) {
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    ssize_t r = readlink(p, out, n - 1);
    if (r < 0) snprintf(out, n, "-%s", ename(errno));
    else out[r] = 0;
}

/* The perms and name of the /proc/self/maps line whose name contains `what` ("-" when there is none). The name
 * is everything from the first '/' (the columns before it hold none). */
static void maps_line(const char *what, char *perms, char *name) {
    strcpy(perms, "-");
    strcpy(name, "-");
    FILE *f = fopen("/proc/self/maps", "r");
    if (!f) return;
    char line[512];
    while (fgets(line, sizeof line, f)) {
        if (!strstr(line, what)) continue;
        line[strcspn(line, "\n")] = 0;
        /* the second column (no sscanf: the image's C programs link glibc headers against musl) */
        char *sp = strchr(line, ' ');
        if (sp) {
            int k = 0;
            for (sp++; *sp && *sp != ' ' && k < 7; sp++) perms[k++] = *sp;
            perms[k] = 0;
        }
        char *n = strchr(line, '/');
        snprintf(name, 256, "%s", n ? n : "?");
        break;
    }
    fclose(f);
}

static int basic(void) {
    char buf[256], perms[8], name[256];
    int fd = memfd("basic", MFD_ALLOW_SEALING | MFD_CLOEXEC);
    yes("memfd made", fd >= 0);
    if (fd < 0) return 2;
    say("cloexec: %d", fcntl(fd, F_GETFD));
    say("seals at start: %#x", fcntl(fd, F_GET_SEALS));
    link_of(fd, buf, sizeof buf);
    say("fd link: %s", buf);
    char lp[64];
    snprintf(lp, sizeof lp, "/proc/self/fd/%d", fd);
    struct stat st;
    if (lstat(lp, &st) == 0) say("link lstat: mode %o size %ld", st.st_mode, (long)st.st_size);
    if (fstat(fd, &st) == 0) say("fstat: mode %o nlink %ld size %ld", st.st_mode, (long)st.st_nlink, (long)st.st_size);
    res("ftruncate 3 pages", ftruncate(fd, 3 * PG));
    char *a = map("shared rw", 3 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    if (!a) return 2;
    memcpy(a, "hello", 5);
    char got[8] = {0};
    pread(fd, got, 5, 0);
    say("read sees the mapping: %s", got);
    maps_line("memfd:basic", perms, name);
    say("maps: %s %s", perms, name);
    errno = 0;
    say("add WRITE with a writable mapping: %s", fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE) < 0 ? ename(errno) : "0");
    errno = 0;
    say("add FUTURE_WRITE: %s", fcntl(fd, F_ADD_SEALS, F_SEAL_FUTURE_WRITE) < 0 ? ename(errno) : "0");
    say("seals: %#x", fcntl(fd, F_GET_SEALS));
    map("new shared rw", PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    char *b = map("new shared ro", PG, PROT_READ, MAP_SHARED, fd);
    if (b) {
        errno = 0;
        res("mprotect it rw", mprotect(b, PG, PROT_READ | PROT_WRITE));
    }
    a[0] = 'H';
    memset(got, 0, sizeof got);
    pread(fd, got, 5, 0);
    say("the old mapping still writes: %s", got);
    if (b) say("the read-only mapping sees it: %c", b[0]);
    errno = 0;
    say("write(2): %s", write(fd, "x", 1) < 0 ? ename(errno) : "written");
    char *c = map("private rw", PG, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd);
    if (c) {
        c[1] = 'E';
        memset(got, 0, sizeof got);
        pread(fd, got, 5, 0);
        say("private copy stays private: %s %c", got, c[1]);
    }
    res("munmap the writable mapping", munmap(a, 3 * PG));
    errno = 0;
    say("add WRITE now: %s", fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE) < 0 ? ename(errno) : "0");
    char *d = map("shared ro after WRITE", 3 * PG, PROT_READ, MAP_SHARED, fd);
    if (d) say("it reads: %.5s", d);
    map("shared rw after WRITE", PG, PROT_READ | PROT_WRITE, MAP_SHARED, fd);
    errno = 0;
    say("add SHRINK: %s", fcntl(fd, F_ADD_SEALS, F_SEAL_SHRINK) < 0 ? ename(errno) : "0");
    /* Through /proc/self/fd: a new open file of the same memfd. */
    int r = open(lp, O_RDONLY);
    yes("reopened read-only", r >= 0);
    if (r >= 0) {
        say("seals through it: %#x", fcntl(r, F_GET_SEALS));
        errno = 0;
        say("add through it: %s", fcntl(r, F_ADD_SEALS, F_SEAL_GROW) < 0 ? ename(errno) : "0");
        struct stat s2;
        fstat(r, &s2);
        yes("same file", s2.st_ino == st.st_ino && s2.st_dev == st.st_dev);
        memset(got, 0, sizeof got);
        read(r, got, 5);
        say("reads: %s", got);
        char *e = map("its shared ro", PG, PROT_READ, MAP_SHARED, r);
        if (e) munmap(e, PG);
        close(r);
    }
    errno = 0;
    say("open O_RDWR|O_TRUNC through the link: %s", open(lp, O_RDWR | O_TRUNC) < 0 ? ename(errno) : "opened");
    errno = 0;
    say("open O_NOFOLLOW: %s", open(lp, O_RDONLY | O_NOFOLLOW) < 0 ? ename(errno) : "opened");
    say("final seals: %#x", fcntl(fd, F_GET_SEALS));
    if (b) munmap(b, PG);
    if (c) munmap(c, PG);
    if (d) munmap(d, 3 * PG);
    close(fd);
    errno = 0;
    say("link after close: %s", open(lp, O_RDONLY) < 0 ? ename(errno) : "opened");
    /* Other names and flags. */
    int s = memfd("a/b c", 0);
    link_of(s, buf, sizeof buf);
    say("odd name link: %s", buf);
    say("plain seals: %#x cloexec %d", fcntl(s, F_GET_SEALS), fcntl(s, F_GETFD));
    close(s);
    int n = memfd("nx", MFD_NOEXEC_SEAL);
    fstat(n, &st);
    say("noexec: mode %o seals %#x", st.st_mode, fcntl(n, F_GET_SEALS));
    close(n);
    int x = memfd("x", MFD_ALLOW_SEALING | MFD_EXEC);
    fcntl(x, F_ADD_SEALS, F_SEAL_EXEC);
    say("exec seal on 0777 brings: %#x", fcntl(x, F_GET_SEALS));
    close(x);
    /* A /dev/shm file is a tmpfs file: seals exist, F_SEAL_SEAL from the start. */
    char shm[64];
    snprintf(shm, sizeof shm, "/dev/shm/fs-memfd-%d", (int)getpid());
    int t = open(shm, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (t >= 0) {
        unlink(shm);
        say("/dev/shm file seals: %#x", fcntl(t, F_GET_SEALS));
        errno = 0;
        say("add to it: %s", fcntl(t, F_ADD_SEALS, F_SEAL_GROW) < 0 ? ename(errno) : "0");
        close(t);
    }
    /* fallocate: Weston-style clients size a wl_shm pool with posix_fallocate on the memfd. */
    int g = memfd("pool", MFD_ALLOW_SEALING | MFD_CLOEXEC);
    int pe = posix_fallocate(g, 0, 2 * PG);
    say("posix_fallocate 2 pages: %s", pe ? ename(pe) : "0");
    fstat(g, &st);
    say("size after it: %ld", (long)st.st_size);
    char *h = map("pool shared rw", 2 * PG, PROT_READ | PROT_WRITE, MAP_SHARED, g);
    if (h) {
        memset(h, 'p', 2 * PG);
        res("punch the first page", falloc(g, FL_PUNCH_HOLE | FL_KEEP_SIZE, 0, PG));
        say("the mapping sees the hole: %d %c", h[0], h[PG]);
        res("punch 100 bytes", falloc(g, FL_PUNCH_HOLE | FL_KEEP_SIZE, PG + 100, 100));
        say("around them: %c %d %d %c", h[PG + 99], h[PG + 100], h[PG + 199], h[PG + 200]);
        munmap(h, 2 * PG);
    }
    fcntl(g, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK);
    res("fallocate inside under GROW", falloc(g, 0, 0, PG));
    res("fallocate past the end under GROW", falloc(g, 0, PG, 2 * PG));
    pe = posix_fallocate(g, PG, 2 * PG);
    say("posix_fallocate past the end under GROW: %s", pe ? ename(pe) : "0");
    res("punch under SHRINK", falloc(g, FL_PUNCH_HOLE | FL_KEEP_SIZE, 0, PG));
    fcntl(g, F_ADD_SEALS, F_SEAL_WRITE);
    res("punch under WRITE", falloc(g, FL_PUNCH_HOLE | FL_KEEP_SIZE, 0, PG));
    fstat(g, &st);
    say("pool size stays: %ld", (long)st.st_size);
    close(g);
    /* Hostile sizes on a /dev/shm file: refused before anything is taken (the mount's size, V2's pool). */
    t = open(shm, O_CREAT | O_EXCL | O_RDWR, 0600);
    if (t >= 0) {
        unlink(shm);
        res("fallocate 1 TiB", falloc(t, 0, 0, 1L << 40));
        res("fallocate 1 TiB KEEP_SIZE", falloc(t, FL_KEEP_SIZE, 0, 1L << 40));
        fstat(t, &st);
        say("its size stays: %ld", (long)st.st_size);
        close(t);
    }
    int k = open("/proc/sys/vm/memfd_noexec", O_RDONLY);
    if (k >= 0) {
        char v[16] = {0};
        ssize_t got_n = read(k, v, sizeof v - 1);
        if (got_n > 0) v[strcspn(v, "\n")] = 0;
        say("vm.memfd_noexec: %s", v);
        close(k);
    } else say("vm.memfd_noexec: %s", ename(errno));
    say("done");
    return 0;
}

/* ---- the wl_shm path ----------------------------------------------------------------------------------- */

static uint32_t pixel(int x, int y, int frame) {
    uint32_t v = (uint32_t)((x * 7 + y * 13 + frame * 101) & 0xff);
    return 0xff000000u | v << 16 | (v ^ 0x5a) << 8 | (uint32_t)((x + y + frame) & 0xff);
}

static uint32_t sum_frame(volatile uint32_t *px) {
    uint32_t h = 0;
    for (int i = 1; i < W * H; i++) h = h * 31 + px[i];
    return h;
}

static void draw(uint32_t *px, int frame) {
    for (int y = 0; y < H; y++)
        for (int x = 0; x < W; x++)
            if (x || y) px[y * W + x] = pixel(x, y, frame);
}

static uint32_t *client_buf;
static int client_fd = -1;

/* The client up to the hand-over: the memfd, its size, frame 1, the seals a wl_shm client sets. */
static int client_start(void) {
    client_fd = memfd("wl_shm", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    yes("client: memfd made", client_fd >= 0);
    if (client_fd < 0) return -1;
    /* The fd number differs between the modes (the socket comes first in wl-socket): not printed. The ktest
     * finds the memfd in the client's fd table by its name. */
    res("client: ftruncate", ftruncate(client_fd, BYTES));
    client_buf = map("client: shared rw", BYTES, PROT_READ | PROT_WRITE, MAP_SHARED, client_fd);
    if (!client_buf) return -1;
    draw(client_buf, 1);
    __atomic_store_n(&client_buf[0], 1, __ATOMIC_RELEASE);
    say("client: frame 1 sum %08x", sum_frame(client_buf));
    errno = 0;
    say("client: seal shrink|grow: %s", fcntl(client_fd, F_ADD_SEALS, F_SEAL_SHRINK | F_SEAL_GROW) < 0 ? ename(errno) : "0");
    return client_fd;
}

/* After the compositor saw frame 1: frame 2 (its sum first, then the counter the compositor waits on). */
static void client_frame2(void) {
    draw(client_buf, 2);
    say("client: frame 2 sum %08x", sum_frame(client_buf));
    __atomic_store_n(&client_buf[0], 2, __ATOMIC_RELEASE);
}

static void client_end(void) {
    res("client: munmap", munmap(client_buf, BYTES));
    res("client: close", close(client_fd));
    say("client: done");
}

/* The compositor on the fd it was given: `seen1` tells the client frame 1 arrived. */
static int compositor(int fd, void (*seen1)(void)) {
    struct stat st;
    if (fstat(fd, &st) != 0) {
        say("compositor: fstat %s", ename(errno));
        return 2;
    }
    say("compositor: size %ld seals %#x", (long)st.st_size, fcntl(fd, F_GET_SEALS));
    say("compositor: access %s", (fcntl(fd, F_GETFL) & O_ACCMODE) == O_RDWR ? "O_RDWR" : "other");
    volatile uint32_t *px = map("compositor: shared ro", st.st_size, PROT_READ, MAP_SHARED, fd);
    if (!px) return 2;
    say("compositor: frame %u sum %08x", px[0], sum_frame(px));
    errno = 0;
    say("compositor: shrink %s", ftruncate(fd, PG) < 0 ? ename(errno) : "done");
    errno = 0;
    say("compositor: grow %s", ftruncate(fd, st.st_size + PG) < 0 ? ename(errno) : "done");
    say("compositor: seen 1");
    seen1();
    int waited = 0;
    struct timespec ms = {0, 1000000};
    while (__atomic_load_n(&px[0], __ATOMIC_ACQUIRE) != 2 && waited < 10000) {
        nanosleep(&ms, NULL);
        waited++;
    }
    if (px[0] != 2) {
        say("compositor: frame 2 never came");
        return 3;
    }
    say("compositor: frame %u sum %08x", px[0], sum_frame(px));
    res("compositor: munmap", munmap((void *)px, st.st_size));
    say("compositor: done");
    return 0;
}

static int sync_fd = -1;
static void seen_by_pipe(void) {
    char c = '1';
    write(sync_fd, &c, 1);
}
static void seen_nothing(void) {}

static int wl_fork(void) {
    if (client_start() < 0) return 2;
    int p[2];
    if (pipe(p) != 0) return 2;
    pid_t pid = fork();
    if (pid < 0) {
        say("fork: %s", ename(errno));
        return 2;
    }
    if (pid == 0) {
        /* The child inherited the memfd (MFD_CLOEXEC matters only for exec). It drops the client's mapping
         * and works only with the fd, as a compositor does. */
        close(p[0]);
        munmap(client_buf, BYTES);
        sync_fd = p[1];
        _exit(compositor(client_fd, seen_by_pipe));
    }
    close(p[1]);
    char c;
    if (read(p[0], &c, 1) != 1) say("client: compositor ended early");
    client_frame2();
    int status = 0;
    waitpid(pid, &status, 0);
    client_end();
    return WIFEXITED(status) ? WEXITSTATUS(status) : 4;
}

static int send_fd(int sock, int fd) {
    char byte = 'F';
    struct iovec iov = {&byte, 1};
    char ctl[CMSG_SPACE(sizeof(int))];
    memset(ctl, 0, sizeof ctl);
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = ctl;
    m.msg_controllen = sizeof ctl;
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof(int));
    return sendmsg(sock, &m, 0) == 1 ? 0 : -1;
}

static int recv_fd(int sock) {
    char byte;
    struct iovec iov = {&byte, 1};
    char ctl[CMSG_SPACE(sizeof(int))];
    struct msghdr m = {0};
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = ctl;
    m.msg_controllen = sizeof ctl;
    if (recvmsg(sock, &m, MSG_CMSG_CLOEXEC) != 1) return -1;
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    if (!c || c->cmsg_type != SCM_RIGHTS) return -1;
    int fd;
    memcpy(&fd, CMSG_DATA(c), sizeof(int));
    return fd;
}

static int wl_socket(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv) != 0) {
        say("socketpair: %s", ename(errno));
        return 2;
    }
    pid_t pid = fork();
    if (pid < 0) {
        say("fork: %s", ename(errno));
        return 2;
    }
    if (pid == 0) {
        /* The compositor: it has nothing of the client's but the socket. */
        close(sv[0]);
        int fd = recv_fd(sv[1]);
        if (fd < 0) _exit(5);
        sync_fd = sv[1];
        _exit(compositor(fd, seen_by_pipe));
    }
    close(sv[1]);
    if (client_start() < 0) return 2;
    if (send_fd(sv[0], client_fd) != 0) say("client: sendmsg %s", ename(errno));
    char c;
    if (read(sv[0], &c, 1) != 1) say("client: compositor ended early");
    client_frame2();
    int status = 0;
    waitpid(pid, &status, 0);
    client_end();
    return WIFEXITED(status) ? WEXITSTATUS(status) : 4;
}

/* One line from stdin ("" at the end). */
static void stdin_line(char *out, size_t n) {
    size_t k = 0;
    char c;
    while (k + 1 < n && read(0, &c, 1) == 1 && c != '\n') out[k++] = c;
    out[k] = 0;
}

static int wl_client(void) {
    if (client_start() < 0) return 2;
    char l[32];
    stdin_line(l, sizeof l);
    if (strcmp(l, "next")) say("client: expected next, got '%s'", l);
    client_frame2();
    stdin_line(l, sizeof l);
    client_end();
    return 0;
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "basic";
    if (!strcmp(m, "basic")) return basic();
    if (!strcmp(m, "wl-fork")) return wl_fork();
    if (!strcmp(m, "wl-socket")) return wl_socket();
    if (!strcmp(m, "wl-client")) return wl_client();
    if (!strcmp(m, "wl-compositor")) return compositor(0, seen_nothing);
    say("unknown mode %s", m);
    return 2;
}
