/* abi-pfcalls (PF-7): what a file call takes from its thread at the moment it runs, against CachyOS. The kernel no
 * longer looks at the credentials, the cwd and the fd the old way per syscall, so each is changed here and the very
 * next call must see it:
 *   ids   setfsuid as a plain user (refused or the same), a file closed to its owner, then a new user namespace;
 *   idsns the same, and in the namespace the owner, now its root, reads the closed file (CAP_DAC_OVERRIDE);
 *   cwd   chdir/fchdir followed by relative opens; another thread's chdir with CLONE_FS shared, then after its
 *         unshare(CLONE_FS); the umask with it;
 *   fds   read, readv, write, writev and getdents on closed, replaced, O_PATH, negative and huge numbers, on the
 *         wrong kind of open file, and on sockets (read and write there take another road in the kernel). */
#include "abi.h"
#include <dirent.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/wait.h>

#pragma clang attribute push(__attribute__((target("arch=x86-64-v3"))), apply_to = function)

static char base[64];
static int ns_root_reads; /* mode idsns: see expected/pf7.known */

static void path_of(char *out, size_t n, const char *name) {
    snprintf(out, n, "%s/%s", base, name);
}

static void put(const char *name, const char *text) {
    char p[128];
    path_of(p, sizeof p, name);
    int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0 || write(fd, text, strlen(text)) != (ssize_t)strlen(text))
        printf("put %s failed: %s\n", name, ename(errno));
    close(fd);
}

/* "label = <the file's first byte>" or "label = -<ERRNO>" for an open by `name` from the cwd. */
static void read_rel(const char *label, const char *name) {
    int fd = open(name, O_RDONLY);
    if (fd < 0) {
        printf("%s = -%s\n", label, ename(errno));
        return;
    }
    char c = 0;
    long n = read(fd, &c, 1);
    if (n == 1)
        printf("%s = %c\n", label, c);
    else
        printf("%s = read %ld %s\n", label, n, ename(errno));
    close(fd);
}

static void cleanup(void) {
    static const char *const names[] = {"a/f", "b/f", "f", "closed", "w", "a", "b"};
    char p[128];
    if (chdir("/") != 0)
        return;
    for (size_t i = 0; i < sizeof names / sizeof *names; i++) {
        path_of(p, sizeof p, names[i]);
        if (unlink(p) != 0)
            rmdir(p);
    }
    rmdir(base);
}

static void setup(void) {
    char p[128];
    snprintf(base, sizeof base, "/tmp/pfcalls-%d", (int)getpid());
    if (mkdir(base, 0755) != 0)
        printf("mkdir: %s\n", ename(errno));
    path_of(p, sizeof p, "a");
    mkdir(p, 0755);
    path_of(p, sizeof p, "b");
    mkdir(p, 0755);
    put("a/f", "A");
    put("b/f", "B");
    put("f", "F");
}

static void ids(void) {
    char closed[128];
    path_of(closed, sizeof closed, "closed");
    put("closed", "C");
    uid_t uid = getuid();
    if (chdir(base) != 0)
        printf("chdir: %s\n", ename(errno));
    yes("the program is no root (the checks below ask a plain user)", uid != 0);
    read_rel("plain", "f");
    yes("setfsuid(own) answers the old one", syscall(SYS_setfsuid, uid) == (long)uid);
    read_rel("after setfsuid(own)", "f");
    yes("setfsuid(another) answers the old one", syscall(SYS_setfsuid, uid + 1) == (long)uid);
    yes("and changed nothing", syscall(SYS_setfsuid, -1) == (long)uid);
    read_rel("after a refused setfsuid", "f");
    SYS("chmod 000", SYS_chmod, closed, 0);
    read_rel("a file closed to its owner", "closed");
    SYS("setresuid to the same", SYS_setresuid, uid, uid, uid);
    read_rel("closed, after setresuid", "closed");
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        gid_t gid = getgid();
        char m[64];
        int n;
        SYS("unshare(CLONE_NEWUSER)", SYS_unshare, CLONE_NEWUSER);
        read_rel("closed, in the namespace, not mapped", "closed");
        int fd = open("/proc/self/setgroups", O_WRONLY);
        if (fd >= 0) {
            if (write(fd, "deny", 4) != 4)
                printf("setgroups: %s\n", ename(errno));
            close(fd);
        }
        n = snprintf(m, sizeof m, "0 %u 1", (unsigned)uid);
        fd = open("/proc/self/uid_map", O_WRONLY);
        if (fd < 0 || write(fd, m, n) != n)
            printf("uid_map: %s\n", ename(errno));
        close(fd);
        n = snprintf(m, sizeof m, "0 %u 1", (unsigned)gid);
        fd = open("/proc/self/gid_map", O_WRONLY);
        if (fd < 0 || write(fd, m, n) != n)
            printf("gid_map: %s\n", ename(errno));
        close(fd);
        if (ns_root_reads)
            read_rel("closed, mapped: the namespace's root may", "closed");
        yes("geteuid in the namespace is 0", geteuid() == 0);
        yes("setfsuid(0) answers 0", syscall(SYS_setfsuid, 0) == 0);
        read_rel("plain, in the namespace", "f");
        fflush(stdout);
        _exit(0);
    }
    int st = -1;
    waitpid(c, &st, 0);
    yes("the child ended well", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    read_rel("closed again in the parent", "closed");
}

static int to_worker[2], from_worker[2];

static void step(void) {
    char c = 0;
    if (write(to_worker[1], &c, 1) != 1 || read(from_worker[0], &c, 1) != 1)
        printf("the worker is gone\n");
}

static void *cwd_worker(void *arg) {
    char *b = arg;
    char c;
    if (read(to_worker[0], &c, 1) != 1) /* one: chdir with the fs shared */
        return 0;
    if (chdir(b) != 0)
        printf("worker chdir: %s\n", ename(errno));
    umask(027);
    if (write(from_worker[1], &c, 1) != 1 || read(to_worker[0], &c, 1) != 1) /* two: alone */
        return 0;
    if (syscall(SYS_unshare, CLONE_FS) != 0)
        printf("worker unshare: %s\n", ename(errno));
    if (chdir("../a") != 0)
        printf("worker chdir: %s\n", ename(errno));
    umask(0);
    read_rel("worker, after its own chdir", "f");
    fflush(stdout);
    if (write(from_worker[1], &c, 1) != 1)
        return 0;
    return 0;
}

static void cwd(void) {
    char a[128], b[128];
    path_of(a, sizeof a, "a");
    path_of(b, sizeof b, "b");
    SYS("chdir a", SYS_chdir, a);
    read_rel("in a", "f");
    SYS("chdir ../b", SYS_chdir, "../b");
    read_rel("in b", "f");
    int fd = open(a, O_RDONLY | O_DIRECTORY);
    SYS("fchdir a", SYS_fchdir, fd);
    read_rel("in a again", "f");
    SYS("chdir nowhere", SYS_chdir, "nowhere");
    read_rel("a refused chdir moves nothing", "f");
    close(fd);
    read_rel("the closed fd keeps nothing from it", "f");
    if (pipe(to_worker) != 0 || pipe(from_worker) != 0)
        printf("pipe: %s\n", ename(errno));
    umask(022);
    fflush(stdout);
    pthread_t t;
    if (pthread_create(&t, 0, cwd_worker, b) != 0)
        printf("thread failed\n");
    step();
    read_rel("after the other thread's chdir", "f");
    mode_t m = umask(022);
    printf("its umask here = %o\n", (unsigned)m);
    umask(m);
    fflush(stdout);
    step();
    read_rel("after its chdir alone", "f");
    m = umask(m);
    printf("umask after its unshare = %o\n", (unsigned)m);
    pthread_join(t, 0);
    read_rel("the thread gone", "f");
    if (chdir(base) != 0)
        printf("chdir: %s\n", ename(errno));
}

static void all_ways(const char *what, long fd) {
    char b[16];
    char name[64];
    struct iovec v = {b, sizeof b};
#define WAY(l, ...)                                     \
    snprintf(name, sizeof name, "%s: " l, what);        \
    SYS(name, __VA_ARGS__)
    WAY("read", SYS_read, fd, b, sizeof b);
    WAY("readv", SYS_readv, fd, &v, 1);
    WAY("pread", SYS_pread64, fd, b, sizeof b, 0);
    WAY("write", SYS_write, fd, "x", 1);
    WAY("writev", SYS_writev, fd, &v, 1);
    WAY("getdents", SYS_getdents64, fd, b, sizeof b);
#undef WAY
}

static void fds(void) {
    char f[128], g[128], w[128];
    path_of(f, sizeof f, "f");
    path_of(g, sizeof g, "a/f");
    path_of(w, sizeof w, "w");
    all_ways("negative", -1);
    all_ways("huge", 1l << 40);
    all_ways("above the table", 4000);
    int fd = open(f, O_RDONLY);
    char c = 0;
    show("read, open", read(fd, &c, 1));
    printf("got %c\n", c);
    close(fd);
    all_ways("closed", fd);
    /* The same number on another file: the next call goes there. */
    int a = open(f, O_RDONLY), b = open(g, O_RDONLY);
    show("read a", read(a, &c, 1));
    printf("got %c\n", c);
    show("dup2 over it", dup2(b, a) == a);
    show("lseek", lseek(a, 0, SEEK_SET));
    show("read, replaced", read(a, &c, 1));
    printf("got %c\n", c);
    close(a);
    close(b);
    int p = open(f, O_PATH);
    all_ways("O_PATH", p);
    close(p);
    int d = open(base, O_RDONLY | O_DIRECTORY);
    SYS("read a directory", SYS_read, d, &c, 1);
    SYS("write to a directory", SYS_write, d, "x", 1);
    close(d);
    int wo = open(w, O_CREAT | O_WRONLY, 0644);
    SYS("read a write-only file", SYS_read, wo, &c, 1);
    SYS("pread a write-only file", SYS_pread64, wo, &c, 1, 0);
    SYS("write to it", SYS_write, wo, "x", 1);
    close(wo);
    int ro = open(f, O_RDONLY);
    SYS("write to a read-only file", SYS_write, ro, "x", 1);
    struct iovec v = {"x", 1};
    SYS("writev to a read-only file", SYS_writev, ro, &v, 1);
    close(ro);
    /* Sockets: read, write, readv and writev are one recvmsg/sendmsg each. */
    int s[2];
    if (socketpair(AF_UNIX, SOCK_DGRAM, 0, s) != 0)
        printf("socketpair: %s\n", ename(errno));
    char big[8] = {0};
    struct iovec two[2] = {{(char *)"ab", 2}, {(char *)"cd", 2}};
    SYS("socket write", SYS_write, s[0], "one", 3);
    SYS("socket writev", SYS_writev, s[0], two, 2);
    SYS("socket read cuts the datagram", SYS_read, s[1], big, 2);
    printf("got %.2s\n", big);
    struct iovec in = {big, sizeof big};
    SYS("socket readv", SYS_readv, s[1], &in, 1);
    printf("got %.4s\n", big);
    SYS("socket pread", SYS_pread64, s[1], big, 2, 0);
    close(s[1]);
    SYS("socket write, peer gone", SYS_write, s[0], "x", 1);
    close(s[0]);
    all_ways("a closed socket", s[0]);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "fds";
    setvbuf(stdout, 0, _IOLBF, 0);
    setup();
    if (!strcmp(mode, "idsns")) {
        ns_root_reads = 1;
        ids();
    } else if (!strcmp(mode, "ids"))
        ids();
    else if (!strcmp(mode, "cwd"))
        cwd();
    else
        fds();
    cleanup();
    return 0;
}

#pragma clang attribute pop
