/* DESK-LEFT 5 (WY): a many-threaded program (as every Chromium process is) ends while its threads wait in the
 * kernel, and its parent reaps it. /proc shows a process as a zombie also while its first thread has ended and
 * others still run; such a process never reaches its parent's wait when one of its threads cannot be ended. Each
 * mode starts a child with 22 threads blocked in futex (plain and bitset with a far deadline), pipe read and
 * write (full), poll, ppoll, select, epoll_wait, epoll_pwait, nanosleep (relative and absolute), recv on a stream
 * and a seqpacket socket, accept, a UDP recvfrom, waitid (on a sleeping grandchild), pause, flock (the parent
 * holds the lock), and reads of an eventfd, inotify, timerfd and signalfd, then ends it: SIGTERM without a handler (`term`), SIGTERM after its first
 * thread left with SYS_exit (`leader-gone`), exit_group from another thread (`thread-exit`), _exit from a SIGTERM
 * handler (`handler`), and SIGTERM to such a child that is pid 1 of a new pid namespace with a handler
 * (`ns-init`) or SIGKILL to one without (`ns-kill`). The parent prints how the child ended and whether every
 * process is gone. */
#include "ns.h"
#include <poll.h>
#include <pthread.h>
#include <sys/epoll.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <time.h>
#include <linux/futex.h>
#include <netinet/in.h>
#include <sys/eventfd.h>
#include <sys/file.h>
#include <sys/inotify.h>
#include <sys/select.h>
#include <sys/signalfd.h>
#include <sys/timerfd.h>
#include <sys/un.h>

static int fds[2], sv[2], ep, ready_w = -1, full[2], lsock, ev, ino, tfd, sfd, udp, lockfd, seq[2];
static int fut;
static pid_t sleeper;

static void *t_futex(void *a) {
    (void)a;
    for (;;)
        syscall(SYS_futex, &fut, FUTEX_WAIT, 0, 0, 0, 0);
    return 0;
}
static void *t_read(void *a) {
    (void)a;
    char c;
    for (;;)
        if (read(fds[0], &c, 1) == 0)
            pause();
    return 0;
}
static void *t_poll(void *a) {
    (void)a;
    struct pollfd p = { fds[0], POLLPRI, 0 };
    for (;;)
        poll(&p, 1, -1);
    return 0;
}
static void *t_epoll(void *a) {
    (void)a;
    struct epoll_event e;
    for (;;)
        epoll_wait(ep, &e, 1, -1);
    return 0;
}
static void *t_sleep(void *a) {
    (void)a;
    for (;;)
        sleep(100);
    return 0;
}
static void *t_recv(void *a) {
    (void)a;
    char b[8];
    for (;;)
        recv(sv[0], b, sizeof b, 0);
    return 0;
}
static void *t_wait(void *a) {
    (void)a;
    siginfo_t si;
    for (;;)
        waitid(P_PID, sleeper, &si, WEXITED);
    return 0;
}
static void *t_pause(void *a) {
    (void)a;
    for (;;)
        pause();
    return 0;
}
static void *t_write_full(void *a) {
    (void)a;
    char b[4096] = { 0 };
    for (;;)
        write(full[1], b, sizeof b);
    return 0;
}
static void *t_accept(void *a) {
    (void)a;
    for (;;)
        accept(lsock, 0, 0);
    return 0;
}
static void *t_select(void *a) {
    (void)a;
    for (;;) {
        fd_set r;
        FD_ZERO(&r);
        FD_SET(fds[0], &r);
        select(fds[0] + 1, &r, 0, 0, 0);
    }
    return 0;
}
static void *t_ppoll(void *a) {
    (void)a;
    struct pollfd p = { sv[0], POLLPRI, 0 };
    sigset_t m;
    sigemptyset(&m);
    for (;;)
        ppoll(&p, 1, 0, &m);
    return 0;
}
static void *t_epoll_pwait(void *a) {
    (void)a;
    struct epoll_event e;
    sigset_t m;
    sigemptyset(&m);
    for (;;)
        epoll_pwait(ep, &e, 1, -1, &m);
    return 0;
}
static void *t_fd_read(void *a) {
    long fd = (long)a;
    char b[512];
    for (;;)
        read(fd, b, sizeof b);
    return 0;
}
static void *t_bitset(void *a) {
    (void)a;
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    t.tv_sec += 100000;
    for (;;)
        syscall(SYS_futex, &fut, FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 0, &t, 0, FUTEX_BITSET_MATCH_ANY);
    return 0;
}
static void *t_abs_sleep(void *a) {
    (void)a;
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    t.tv_sec += 100000;
    for (;;)
        clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &t, 0);
    return 0;
}
static void *t_udp(void *a) {
    (void)a;
    char b[64];
    for (;;)
        recvfrom(udp, b, sizeof b, 0, 0, 0);
    return 0;
}
static void *t_flock(void *a) {
    (void)a;
    for (;;)
        flock(lockfd, LOCK_EX);
    return 0;
}
static void *t_seqpacket(void *a) {
    (void)a;
    char b[64];
    struct iovec v = { b, sizeof b };
    struct msghdr m = { 0 };
    m.msg_iov = &v;
    m.msg_iovlen = 1;
    for (;;)
        recvmsg(seq[0], &m, 0);
    return 0;
}

static void *t_exit_group(void *a) {
    (void)a;
    usleep(200000);
    syscall(SYS_exit_group, 3);
    return 0;
}

static void on_term(int s) {
    (void)s;
    _exit(0);
}

static long global_pid(void) {
    long v[8];
    return status_fields("NSpid:", v, 8) > 0 ? v[0] : -1;
}

/* The child: threads in every wait, then tells the parent its global pid and the sleeper's. */
static void child(const char *mode) {
    if (!strcmp(mode, "handler") || !strcmp(mode, "ns-init"))
        signal(SIGTERM, on_term);
    if (pipe(fds) || socketpair(AF_UNIX, SOCK_STREAM, 0, sv))
        _exit(90);
    ep = epoll_create1(0);
    struct epoll_event e = { EPOLLIN, { 0 } };
    epoll_ctl(ep, EPOLL_CTL_ADD, sv[1], &e);
    if (pipe(full) || socketpair(AF_UNIX, SOCK_SEQPACKET, 0, seq))
        _exit(94);
    lsock = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un ua;
    memset(&ua, 0, sizeof ua);
    ua.sun_family = AF_UNIX;
    snprintf(ua.sun_path + 1, sizeof ua.sun_path - 1, "abi-reap-%d", (int)getpid());
    if (bind(lsock, (struct sockaddr *)&ua, sizeof ua) || listen(lsock, 1))
        _exit(95);
    ev = eventfd(0, 0);
    ino = inotify_init();
    inotify_add_watch(ino, "/", IN_DELETE_SELF);
    tfd = timerfd_create(CLOCK_MONOTONIC, 0);
    sigset_t us;
    sigemptyset(&us);
    sigaddset(&us, SIGUSR2);
    sigprocmask(SIG_BLOCK, &us, 0);
    sfd = signalfd(-1, &us, 0);
    udp = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in in = { AF_INET, 0, { htonl(INADDR_LOOPBACK) }, { 0 } };
    bind(udp, (struct sockaddr *)&in, sizeof in);
    lockfd = open("/proc/self/exe", O_RDONLY);
    if (ev < 0 || ino < 0 || tfd < 0 || sfd < 0 || udp < 0 || lockfd < 0)
        _exit(96);
    sleeper = fork();
    if (sleeper == 0) {
        prctl(PR_SET_PDEATHSIG, SIGKILL);
        long m[2] = { 1, global_pid() };
        if (write(ready_w, m, sizeof m) != sizeof m)
            _exit(93);
        for (;;)
            pause();
    }
    void *(*fn[])(void *) = { t_futex, t_read, t_poll, t_epoll, t_sleep, t_recv, t_wait, t_pause, t_write_full, t_accept, t_select,
                              t_ppoll, t_epoll_pwait, t_bitset, t_abs_sleep, t_udp, t_flock, t_seqpacket };
    for (unsigned k = 0; k < sizeof fn / sizeof fn[0]; k++) {
        pthread_t t;
        if (pthread_create(&t, 0, fn[k], 0))
            _exit(91);
    }
    long rfd[] = { ev, ino, tfd, sfd };
    for (unsigned k = 0; k < sizeof rfd / sizeof rfd[0]; k++) {
        pthread_t t;
        if (pthread_create(&t, 0, t_fd_read, (void *)rfd[k]))
            _exit(91);
    }
    long m[2] = { 0, global_pid() };
    usleep(100000);
    if (write(ready_w, m, sizeof m) != sizeof m)
        _exit(92);
    if (!strcmp(mode, "thread-exit")) {
        pthread_t t;
        pthread_create(&t, 0, t_exit_group, 0);
    }
    if (!strcmp(mode, "leader-gone"))
        syscall(SYS_exit, 0);
    for (;;)
        pause();
}

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

static int there(long pid) {
    char path[64], b[64];
    snprintf(path, sizeof path, "/proc/%ld/stat", pid);
    return rd(path, b, sizeof b) > 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IOLBF, 0);
    const char *mode = argc > 1 ? argv[1] : "term";
    int ns = !strncmp(mode, "ns-", 3);
    int lk = open("/proc/self/exe", O_RDONLY);
    if (lk < 0 || flock(lk, LOCK_EX))
        return 1;
    int rp[2];
    if (pipe(rp))
        return 1;
    ready_w = rp[1];
    long c = ns ? rclone(CLONE_NEWUSER | CLONE_NEWPID | SIGCHLD) : fork();
    if (c == 0) {
        close(rp[0]);
        child(mode);
    }
    close(rp[1]);
    long pid_of[2] = { 0, 0 };
    for (int k = 0; k < 2; k++) {
        long m[2];
        if (read(rp[0], m, sizeof m) != sizeof m || m[0] < 0 || m[0] > 1) {
            printf("%s: the child did not start\n", mode);
            return 1;
        }
        pid_of[m[0]] = m[1];
    }
    long sl = pid_of[1];
    printf("%s: child with 22 waiting threads started = yes\n", mode);
    if (!strcmp(mode, "leader-gone")) {
        char path[64], b[512];
        snprintf(path, sizeof path, "/proc/%ld/stat", c);
        for (double end = now() + 5; now() < end; usleep(10000))
            if (rd(path, b, sizeof b) > 0 && strstr(b, ") Z "))
                break;
        printf("leader-gone: shown as a zombie while its threads run = %s\n", yn(rd(path, b, sizeof b) > 0 && strstr(b, ") Z ") != 0));
    }
    if (!strcmp(mode, "ns-kill"))
        kill(c, SIGKILL);
    else if (strcmp(mode, "thread-exit"))
        kill(c, SIGTERM);
    /* Waits with a deadline: a child that never ends must not hang the test. */
    int st = 0;
    pid_t r = 0;
    for (double end = now() + 10; now() < end && (r = waitpid(c, &st, WNOHANG)) == 0;)
        usleep(10000);
    if (r == c)
        printf("%s: child ended: %s %d\n", mode, WIFEXITED(st) ? "exit" : "signal", WIFEXITED(st) ? WEXITSTATUS(st) : WTERMSIG(st));
    else {
        char path[64], b[512];
        snprintf(path, sizeof path, "/proc/%ld/stat", c);
        char *e = rd(path, b, sizeof b) > 0 ? strrchr(b, ')') : 0;
        printf("%s: child not reaped within 10 s (state %c)\n", mode, e ? e[2] : '-');
        kill(c, SIGKILL);
        waitpid(c, &st, 0);
    }
    for (double end = now() + 5; now() < end && there(sl);)
        usleep(10000);
    printf("%s: its sleeping child is gone = %s\n", mode, yn(!there(sl)));
    return 0;
}
