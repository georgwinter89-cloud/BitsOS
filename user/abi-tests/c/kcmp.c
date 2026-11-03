/* AB-17: kcmp and F_DUPFD_QUERY, the two ways Mesa asks whether two fds name the same open file (Mesa tries
 * F_DUPFD_QUERY first). Only equality (== 0) and antisymmetry (one way 1, the other 2) are printed: the order
 * itself comes from scrambled kernel addresses. Covers KCMP_FILE (dup, O_PATH, across fork, a reopened file),
 * KCMP_VM/FILES/FS/SIGHAND between threads, a fork child and a CLONE_FILES child, a zombie, and the errors in
 * Linux's order (pids ESRCH, permission EPERM with a non-dumpable child, type EINVAL, fds EBADF, the
 * KCMP_EPOLL_TFD slot EFAULT/EBADF/EINVAL, then the target lookup). Run as root, it first becomes nobody. */
#include "abi.h"
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/prctl.h>
#include <sys/wait.h>

#define NO_PID 0x3ffffff0
#ifndef F_DUPFD_QUERY
#define F_DUPFD_QUERY 1027
#endif
enum { KCMP_FILE, KCMP_VM, KCMP_FILES, KCMP_FS, KCMP_SIGHAND, KCMP_IO, KCMP_SYSVSEM, KCMP_EPOLL_TFD, KCMP_TYPES };
struct slot {
    uint32_t efd, tfd, toff;
};

static long kc(pid_t a, pid_t b, int type, unsigned long i1, unsigned long i2) {
    return syscall(SYS_kcmp, a, b, type, i1, i2);
}

/* "label: equal" / "differ (antisymmetric)" / "differ (not antisymmetric)" / "-ERRNO". */
static void rel(const char *label, pid_t a, pid_t b, int type, unsigned long i1, unsigned long i2) {
    long x = kc(a, b, type, i1, i2);
    if (x < 0) {
        printf("%s = -%s\n", label, ename(errno));
        return;
    }
    long y = kc(b, a, type, i2, i1);
    if (x == 0 && y == 0)
        printf("%s: equal\n", label);
    else
        printf("%s: differ (%s)\n", label, x + y == 3 && x * y == 2 ? "antisymmetric" : "not antisymmetric");
}

/* KCMP_EPOLL_TFD of one task against itself: "label: equal" / "differ" / "-ERRNO". */
static void tfd(const char *label, pid_t me, unsigned long idx1, struct slot *s) {
    long x = kc(me, me, KCMP_EPOLL_TFD, idx1, (unsigned long)s);
    if (x < 0)
        printf("%s = -%s\n", label, ename(errno));
    else
        printf("%s: %s\n", label, x == 0 ? "equal" : "differ");
}

static void query(const char *label, int fd, long other) {
    long r = fcntl(fd, F_DUPFD_QUERY, other);
    if (r < 0)
        printf("%s = -%s\n", label, ename(errno));
    else
        printf("%s = %ld\n", label, r);
}

static volatile pid_t live_tid;
static volatile int stop_thread;
static void *spin_thread(void *arg) {
    (void)arg;
    live_tid = (pid_t)syscall(SYS_gettid);
    while (!stop_thread)
        usleep(1000);
    return 0;
}

/* A process whose leader thread ended while another thread runs: the leader keeps its signal handlers but
 * has no memory, files or fs state any more. */
static pid_t leader;
static int leader_fd;
static void *after_leader(void *arg) {
    (void)arg;
    pid_t me = (pid_t)syscall(SYS_gettid);
    for (int i = 0; i < 3000 && kc(leader, me, KCMP_VM, 0, 0) == 0; i++)
        usleep(1000);
    rel("ended leader KCMP_VM against a running thread", leader, me, KCMP_VM, 0, 0);
    rel("ended leader KCMP_VM against itself", leader, leader, KCMP_VM, 0, 0);
    rel("ended leader KCMP_FILES against a running thread", leader, me, KCMP_FILES, 0, 0);
    rel("ended leader KCMP_FS against a running thread", leader, me, KCMP_FS, 0, 0);
    rel("ended leader KCMP_SIGHAND against a running thread", leader, me, KCMP_SIGHAND, 0, 0);
    rel("ended leader KCMP_FILE", leader, me, KCMP_FILE, leader_fd, leader_fd);
    rel("running thread KCMP_FILE against itself", me, me, KCMP_FILE, leader_fd, leader_fd);
    _exit(0);
}
static void ended_leader(int fd) {
    pid_t p = fork();
    if (p == 0) {
        leader = getpid();
        leader_fd = fd;
        pthread_t th;
        if (pthread_create(&th, 0, after_leader, 0))
            _exit(1);
        syscall(SYS_exit, 0);
    }
    waitpid(p, 0, 0);
}

static int to_child[2], to_parent[2];
static char ask(char c) {
    char r = '?';
    write(to_child[1], &c, 1);
    read(to_parent[0], &r, 1);
    return r;
}

/* A child that answers commands: 'r' reopens fd `e1` on another file, 'd' turns dumpable off, 'q' ends. */
static void child_loop(int e1) {
    char c;
    while (read(to_child[0], &c, 1) == 1) {
        char out = 'n';
        if (c == 'r') {
            int n = eventfd(0, 0);
            out = n >= 0 && dup2(n, e1) == e1 ? 'y' : 'n';
            close(n);
        }
        if (c == 'd')
            out = prctl(PR_SET_DUMPABLE, 0) == 0 ? 'y' : 'n';
        if (c == 'q')
            _exit(0);
        write(to_parent[1], &out, 1);
    }
    _exit(1);
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    if (geteuid() == 0 && (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))) {
        printf("cannot drop root\n");
        return 1;
    }
    prctl(PR_SET_DUMPABLE, 1);
    pid_t me = getpid();
    int e1 = eventfd(0, 0), e2 = eventfd(0, 0);
    int d1 = dup(e1), dc = fcntl(e1, F_DUPFD_CLOEXEC, 0);
    int op = open("/", O_PATH), opd = dup(op);

    /* F_DUPFD_QUERY next to KCMP_FILE. */
    query("F_DUPFD_QUERY(e1, dup)", e1, d1);
    query("F_DUPFD_QUERY(e1, CLOEXEC dup)", e1, dc);
    query("F_DUPFD_QUERY(e1, itself)", e1, e1);
    query("F_DUPFD_QUERY(e1, e2)", e1, e2);
    query("F_DUPFD_QUERY(e1, closed)", e1, 999);
    query("F_DUPFD_QUERY(e1, -1)", e1, -1);
    query("F_DUPFD_QUERY(O_PATH, its dup)", op, opd);
    query("F_DUPFD_QUERY(O_PATH, e1)", op, e1);
    query("F_DUPFD_QUERY(e1, O_PATH)", e1, op);
    query("F_DUPFD_QUERY(closed, e1)", 999, e1);
    rel("KCMP_FILE(e1, dup)", me, me, KCMP_FILE, e1, d1);
    rel("KCMP_FILE(e1, CLOEXEC dup)", me, me, KCMP_FILE, e1, dc);
    rel("KCMP_FILE(e1, e2)", me, me, KCMP_FILE, e1, e2);
    rel("KCMP_FILE(O_PATH, its dup)", me, me, KCMP_FILE, op, opd);
    rel("KCMP_FILE(e1, closed)", me, me, KCMP_FILE, e1, 999);
    rel("KCMP_FILE(closed, e1)", me, me, KCMP_FILE, 999, e1);
    rel("KCMP_FILE(fd 2^32 + e1: an unsigned int)", me, me, KCMP_FILE, (1ul << 32) + e1, d1);

    /* A thread shares everything. */
    pthread_t th;
    if (pthread_create(&th, 0, spin_thread, 0) == 0) {
        while (!live_tid)
            usleep(1000);
        rel("thread KCMP_VM", me, live_tid, KCMP_VM, 0, 0);
        rel("thread KCMP_FILES", me, live_tid, KCMP_FILES, 0, 0);
        rel("thread KCMP_FS", me, live_tid, KCMP_FS, 0, 0);
        rel("thread KCMP_SIGHAND", me, live_tid, KCMP_SIGHAND, 0, 0);
        rel("thread KCMP_FILE(e1, dup)", me, live_tid, KCMP_FILE, e1, d1);
        stop_thread = 1;
        pthread_join(th, 0);
    }

    /* A fork child: the same open files, nothing else shared. */
    if (pipe(to_child) || pipe(to_parent))
        return 1;
    pid_t c = fork();
    if (c == 0)
        child_loop(e1);
    rel("child KCMP_FILE(e1, e1)", me, c, KCMP_FILE, e1, e1);
    rel("child KCMP_FILE(e1, e2)", me, c, KCMP_FILE, e1, e2);
    rel("child KCMP_VM", me, c, KCMP_VM, 0, 0);
    rel("child KCMP_FILES", me, c, KCMP_FILES, 0, 0);
    rel("child KCMP_FS", me, c, KCMP_FS, 0, 0);
    rel("child KCMP_SIGHAND", me, c, KCMP_SIGHAND, 0, 0);
    printf("child reopened e1: %c\n", ask('r'));
    rel("child KCMP_FILE(e1, e1) after the reopen", me, c, KCMP_FILE, e1, e1);

    /* The errors, in Linux's order. */
    rel("type KCMP_TYPES", me, c, KCMP_TYPES, 0, 0);
    rel("type -1", me, c, -1, 0, 0);
    rel("no such pid 1 (bad type)", NO_PID, me, 99, 0, 0);
    rel("no such pid 2", me, NO_PID, KCMP_VM, 0, 0);
    rel("pid 0", 0, me, KCMP_VM, 0, 0);
    rel("pid -1", me, -1, KCMP_VM, 0, 0);
    int ep = epoll_create1(0);
    struct epoll_event ev = {.events = EPOLLIN};
    epoll_ctl(ep, EPOLL_CTL_ADD, e2, &ev);
    SYS("EPOLL_TFD slot unreadable", SYS_kcmp, me, me, KCMP_EPOLL_TFD, e2, KERNEL_ADDR);
    struct slot s = {.efd = 999, .tfd = e2, .toff = 0};
    SYS("EPOLL_TFD idx1 closed", SYS_kcmp, me, me, KCMP_EPOLL_TFD, 999, &s);
    SYS("EPOLL_TFD efd closed", SYS_kcmp, me, me, KCMP_EPOLL_TFD, e2, &s);
    s.efd = e1;
    SYS("EPOLL_TFD efd not an epoll", SYS_kcmp, me, me, KCMP_EPOLL_TFD, e2, &s);
    s.efd = op;
    SYS("EPOLL_TFD efd O_PATH", SYS_kcmp, me, me, KCMP_EPOLL_TFD, e2, &s);
    s.efd = ep;
    tfd("EPOLL_TFD target is idx1's file", me, e2, &s);
    tfd("EPOLL_TFD target is another file", me, e1, &s);
    s.toff = 1;
    tfd("EPOLL_TFD toff past the items", me, e2, &s);
    s.toff = 0;
    s.tfd = e1;
    tfd("EPOLL_TFD tfd not watched", me, e1, &s);
    int e3 = dup(e2);
    epoll_ctl(ep, EPOLL_CTL_ADD, e3, &ev);
    s.tfd = e3;
    tfd("EPOLL_TFD dup'ed target, same open file", me, e2, &s);
    close(e3);
    tfd("EPOLL_TFD target closed", me, e2, &s);
    printf("child undumpable: %c\n", ask('d'));
    rel("undumpable KCMP_VM", me, c, KCMP_VM, 0, 0);
    rel("undumpable, bad type", me, c, 99, 0, 0);
    rel("undumpable, closed fds", me, c, KCMP_FILE, 999, 999);
    rel("undumpable, no such pid", NO_PID, c, KCMP_VM, 0, 0);

    /* A CLONE_FILES child shares the table but not the memory. It waits on a pipe of its own: two readers of
     * to_child would race for the parent's 'q' (INT-W24: on V2 the CLONE_FILES child sometimes took it, so the
     * fork child never ended; Linux wakes one exclusive reader, the one that waited first). */
    int to_cf[2];
    if (pipe(to_cf))
        return 1;
    pid_t cf = (pid_t)syscall(SYS_clone, CLONE_FILES | SIGCHLD, 0, 0, 0, 0);
    if (cf == 0) {
        char b;
        read(to_cf[0], &b, 1);
        _exit(0);
    }
    if (cf > 0) {
        rel("CLONE_FILES child KCMP_FILES", me, cf, KCMP_FILES, 0, 0);
        rel("CLONE_FILES child KCMP_VM", me, cf, KCMP_VM, 0, 0);
        rel("CLONE_FILES child KCMP_FILE(e1, e1)", me, cf, KCMP_FILE, e1, e1);
    }

    write(to_child[1], "q", 1);
    waitpid(c, 0, 0);
    rel("collected", me, c, KCMP_VM, 0, 0);
    /* A dumpable child that ended: as a zombie it has no memory or files of its own any more (Linux 6.6 lets
     * anyone with the uids look at a zombie; newer kernels keep refusing one that was not dumpable, so the
     * undumpable child is not asked here). */
    pid_t z = fork();
    if (z == 0)
        _exit(0);
    siginfo_t si;
    waitid(P_PID, z, &si, WEXITED | WNOWAIT);
    rel("zombie KCMP_VM against me", me, z, KCMP_VM, 0, 0);
    rel("zombie KCMP_VM against itself", z, z, KCMP_VM, 0, 0);
    rel("zombie KCMP_FILES against itself", z, z, KCMP_FILES, 0, 0);
    rel("zombie KCMP_SIGHAND against itself", z, z, KCMP_SIGHAND, 0, 0);
    rel("zombie KCMP_FILE", me, z, KCMP_FILE, e1, e1);
    waitpid(z, 0, 0);
    ended_leader(e1);
    if (cf > 0) {
        write(to_cf[1], "x", 1);
        waitpid(cf, 0, 0);
    }
    return 0;
}
