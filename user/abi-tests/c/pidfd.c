/* AB-12, L7-PROC: process handles as Linux 7.3.1 has them (fs/pidfs.c). pidfd_open (PIDFD_THREAD too),
 * CLONE_PIDFD (clone and clone3), pidfd_send_signal (scopes, PIDFD_SELF_*), pidfd_getfd, waitid(P_PIDFD),
 * poll/epoll on a pidfd (readable when the task ended, EPOLLHUP once it has none), /proc/self/fdinfo's Pid
 * line, fstat of the pidfs inode and the pidfs ioctls. Only relations are printed (never pids or inode
 * numbers); PIDFD_INFO_CGROUPID is masked out (V2 has no cgroups). */
#include "abi.h"
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/wait.h>

#ifndef P_PIDFD
#define P_PIDFD 3
#endif
#define CLONE_PIDFD_ 0x1000
#define PIDFD_NONBLOCK_ O_NONBLOCK
#define NO_PID 0x3ffffff0

struct clone_args_v0 {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls;
};

static void flush(void) {
    fflush(stdout);
    fflush(stderr);
}

static long popen_(pid_t pid, unsigned flags) {
    return syscall(SYS_pidfd_open, pid, flags);
}

/* poll(fd, POLLIN, ms): the revents (or -1). */
static int ready(int fd, int ms) {
    struct pollfd p = {.fd = fd, .events = POLLIN};
    int r = poll(&p, 1, ms);
    return r < 0 ? -1 : p.revents;
}

/* The Pid: line of /proc/self/fdinfo/<fd> (-2 when missing) and whether NSpid: says the same. */
static long fdinfo_pid(int fd, int *ns_same) {
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    FILE *f = fopen(path, "r");
    long pid = -2, ns = -3;
    if (!f)
        return pid;
    while (fgets(buf, sizeof buf, f)) {
        sscanf(buf, "Pid:\t%ld", &pid);
        sscanf(buf, "NSpid:\t%ld", &ns);
    }
    fclose(f);
    *ns_same = pid == ns;
    return pid;
}

/* The flags: line of /proc/self/fdinfo/<fd> as printed ("?" when missing). */
static const char *fdinfo_flags(int fd) {
    static char out[32];
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    strcpy(out, "?");
    FILE *f = fopen(path, "r");
    if (!f)
        return out;
    while (fgets(buf, sizeof buf, f))
        sscanf(buf, "flags:\t%31s", out);
    fclose(f);
    return out;
}

static void show_fdinfo(const char *label, int fd, long want) {
    int same = 0;
    long pid = fdinfo_pid(fd, &same);
    printf("%s: Pid %s, NSpid same: %s\n", label, pid == want ? "as expected" : pid == -2 ? "missing" : "other", same ? "yes" : "no");
}

static void *thread_main(void *arg) {
    *(pid_t *)arg = (pid_t)syscall(SYS_gettid);
    return 0;
}

static void t_open(void) {
    SYS("pidfd_open(flag 1)", SYS_pidfd_open, getpid(), 1);
    SYS("pidfd_open(0)", SYS_pidfd_open, 0, 0);
    SYS("pidfd_open(-1)", SYS_pidfd_open, -1, 0);
    SYS("pidfd_open(no such pid)", SYS_pidfd_open, NO_PID, 0);
    int fd = popen_(getpid(), 0);
    int nb = popen_(getpid(), PIDFD_NONBLOCK_);
    yes("pidfd_open(self) >= 0", fd >= 0);
    yes("pidfd_open(self, NONBLOCK) >= 0", nb >= 0);
    yes("close-on-exec", fcntl(fd, F_GETFD) == FD_CLOEXEC);
    yes("F_GETFL = O_RDWR", fcntl(fd, F_GETFL) == O_RDWR);
    yes("F_GETFL(NONBLOCK) = O_RDWR|O_NONBLOCK", fcntl(nb, F_GETFL) == (O_RDWR | O_NONBLOCK));
    char b[16];
    SYS("read(pidfd)", SYS_read, fd, b, sizeof b);
    SYS("write(pidfd)", SYS_write, fd, b, 1);
    SYS("lseek(pidfd)", SYS_lseek, fd, 0, SEEK_SET);
    char path[64], link[64] = {0};
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    long n = readlink(path, link, sizeof link - 1);
    printf("link = %s\n", n > 0 ? link : "?");
    printf("poll(self) revents = %d\n", ready(fd, 0));
    show_fdinfo("fdinfo(self)", fd, getpid());
    /* A thread that is not the leader: EINVAL. */
    pid_t tid = 0;
    pthread_t th;
    if (pthread_create(&th, 0, thread_main, &tid) == 0) {
        pthread_join(th, 0);
        /* pthread_join returns when the tid word is cleared, before the kernel releases the thread: wait for
         * the release, then its id is unknown to pidfd_open (ESRCH). */
        long r = 0;
        for (int i = 0; i < 1000 && (r = syscall(SYS_pidfd_open, tid, O_EXCL)) >= 0; i++) {
            close(r);
            usleep(1000);
        }
        printf("pidfd_open(released thread, PIDFD_THREAD) = %s\n", r < 0 ? ename(errno) : "a pidfd");
    }
    close(fd);
    close(nb);
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

#define PIDFD_GET_INFO_ 0xc058ff0bul
#define PIDFD_GET_INFO_V0 0xc040ff0bul

struct info_ {
    uint64_t mask, cgroupid;
    uint32_t pid, tgid, ppid, ruid, rgid, euid, egid, suid, sgid, fsuid, fsgid;
    int32_t exit_code;
    uint32_t cd_mask, cd_signal, cd_code, cd_pad;
    uint64_t supported;
};

static long get_info(int fd, uint64_t want, struct info_ *i) {
    memset(i, 0, sizeof *i);
    i->mask = want;
    long r = ioctl(fd, PIDFD_GET_INFO_, i);
    return r < 0 ? -errno : r;
}

static void show_info(const char *label, int fd, uint64_t want, pid_t pid, pid_t tgid) {
    struct info_ i;
    long r = get_info(fd, want, &i);
    if (r < 0) {
        printf("%s = -%s\n", label, ename(-r));
        return;
    }
    printf("%s = 0, mask %#lx, pid %s, tgid %s, ppid me %s, ruid me %s, exit %#x, coredump %#x, supported %#lx\n", label,
           (unsigned long)(i.mask & ~4ul), i.pid == (uint32_t)pid ? "ok" : "other", i.tgid == (uint32_t)tgid ? "ok" : "other",
           i.ppid == (uint32_t)getpid() ? "yes" : "no", i.ruid == getuid() ? "yes" : "no", i.exit_code, i.cd_mask,
           (unsigned long)i.supported);
}

static unsigned long ino_of(int fd) {
    struct stat st;
    return fstat(fd, &st) == 0 ? (unsigned long)st.st_ino : 0;
}

static void t_thread(void) {
    pthread_t th;
    if (pthread_create(&th, 0, spin_thread, 0) != 0)
        return;
    while (!live_tid)
        usleep(1000);
    /* linux-7.3.1/kernel/fork.c:1946: a non-leader without PIDFD_THREAD is ENOENT. */
    SYS("pidfd_open(live non-leader thread)", SYS_pidfd_open, live_tid, 0);
    int tf = popen_(live_tid, O_EXCL), tf2 = popen_(live_tid, O_EXCL);
    int pf = popen_(getpid(), 0), pf2 = popen_(getpid(), 0), lf = popen_(getpid(), O_EXCL);
    yes("thread pidfds opened", tf >= 0 && tf2 >= 0 && lf >= 0);
    printf("F_GETFL(thread pidfd) = %#o\n", fcntl(tf, F_GETFL));
    printf("F_GETFL(leader thread pidfd) = %#o\n", fcntl(lf, F_GETFL));
    /* fdinfo prints f_flags, PIDFD_THREAD included (linux-7.3.1/fs/proc/fd.c:42). */
    printf("fdinfo flags(thread pidfd) = %s\n", fdinfo_flags(tf));
    printf("fdinfo flags(process pidfd) = %s\n", fdinfo_flags(pf));
    yes("two pidfds of one thread: one inode", ino_of(tf) == ino_of(tf2) && ino_of(tf) != 0);
    yes("two pidfds of one process: one inode", ino_of(pf) == ino_of(pf2));
    yes("the leader's thread pidfd: the process's inode", ino_of(lf) == ino_of(pf));
    yes("a thread's inode differs from its process's", ino_of(tf) != ino_of(pf));
    printf("poll(live thread) revents = %d\n", ready(tf, 0));
    SYS("send(thread pidfd, 0)", SYS_pidfd_send_signal, tf, 0, 0, 0);
    show_info("GET_INFO(thread)", tf, 0x23, live_tid, getpid());
    stop_thread = 1;
    pthread_join(th, 0);
    printf("poll(ended thread) revents = %d\n", ready(tf, 1000));
    show_info("GET_INFO(ended thread)", tf, 0x2, live_tid, getpid());
    show_info("GET_INFO(ended thread, EXIT)", tf, 0x8, live_tid, getpid());
    SYS("send(ended thread)", SYS_pidfd_send_signal, tf, 0, 0, 0);
    close(tf);
    close(tf2);
    close(pf);
    close(pf2);
    close(lf);
}

/* fstat of the pidfs inode and the pidfs ioctls (linux-7.3.1/fs/pidfs.c:365, :530, :1006). */
static void t_pidfs(void) {
    int self = popen_(getpid(), 0), e = eventfd(0, 0);
    struct stat st, est;
    fstat(self, &st);
    fstat(e, &est);
    printf("fstat(pidfd): mode %#o, nlink %lu, uid %u, size %ld, blksize %ld, dev differs from eventfd's: %s\n",
           st.st_mode, (unsigned long)st.st_nlink, st.st_uid, (long)st.st_size, (long)st.st_blksize,
           st.st_dev != est.st_dev ? "yes" : "no");
    unsigned v = 99;
    SYS("FS_IOC_GETVERSION", SYS_ioctl, self, 0x80087601ul, &v);
    printf("  generation %u\n", v);
    SYS("FS_IOC_GETVERSION(NULL)", SYS_ioctl, self, 0x80087601ul, 0);
    SYS("GET_INFO(NULL)", SYS_ioctl, self, PIDFD_GET_INFO_, 0);
    SYS("GET_INFO(kernel address)", SYS_ioctl, self, PIDFD_GET_INFO_, KERNEL_ADDR);
    SYS("GET_INFO(size 8)", SYS_ioctl, self, 0xc008ff0bul, &v);
    struct info_ i;
    memset(&i, 0, sizeof i);
    SYS("GET_INFO(size 64)", SYS_ioctl, self, PIDFD_GET_INFO_V0, &i);
    printf("  mask %#lx, pid ok %s\n", (unsigned long)(i.mask & ~4ul), i.pid == (uint32_t)getpid() ? "yes" : "no");
    show_info("GET_INFO(self, everything)", self, 0xff, getpid(), getpid());
    SYS("ioctl(pidfd, 0xff0c)", SYS_ioctl, self, 0xff0cul, 0);
    SYS("ioctl(pidfd, TCGETS)", SYS_ioctl, self, 0x5401ul, &i);
    long nfd = ioctl(self, 0xff04, 0);
    char a[64] = {0}, b[64] = {0}, pth[64];
    snprintf(pth, sizeof pth, "/proc/self/fd/%ld", nfd);
    readlink(pth, a, sizeof a - 1);
    readlink("/proc/self/ns/net", b, sizeof b - 1);
    printf("GET_NET_NAMESPACE: fd %s, same namespace %s, close-on-exec %s\n", nfd >= 0 ? "yes" : "no", !strcmp(a, b) ? "yes" : "no",
           fcntl(nfd, F_GETFD) == FD_CLOEXEC ? "yes" : "no");
    close(nfd);
    nfd = ioctl(self, 0xff09, 0);
    memset(a, 0, sizeof a);
    memset(b, 0, sizeof b);
    snprintf(pth, sizeof pth, "/proc/self/fd/%ld", nfd);
    readlink(pth, a, sizeof a - 1);
    readlink("/proc/self/ns/user", b, sizeof b - 1);
    printf("GET_USER_NAMESPACE: same namespace %s\n", nfd >= 0 && !strcmp(a, b) ? "yes" : "no");
    close(nfd);
    SYS("GET_NET_NAMESPACE(arg 1)", SYS_ioctl, self, 0xff04ul, 1);
    flush();
    pid_t c = fork();
    if (c == 0)
        _exit(9);
    int cfd = popen_(c, 0);
    siginfo_t w;
    waitid(P_PID, c, &w, WEXITED | WNOWAIT);
    show_info("GET_INFO(zombie, EXIT)", cfd, 0x8, c, c);
    SYS("GET_NET_NAMESPACE(zombie)", SYS_ioctl, cfd, 0xff04ul, 0);
    waitpid(c, 0, 0);
    printf("poll(reaped) revents = %d\n", ready(cfd, 0));
    show_info("GET_INFO(reaped, EXIT)", cfd, 0x8, c, c);
    show_info("GET_INFO(reaped, EXIT | SUPPORTED)", cfd, 0x28, c, c);
    show_info("GET_INFO(reaped)", cfd, 0x3, c, c);
    SYS("GET_NET_NAMESPACE(reaped)", SYS_ioctl, cfd, 0xff04ul, 0);
    close(cfd);
    close(e);
    close(self);
}

static void t_signal(void) {
    int self = popen_(getpid(), 0);
    int p[2];
    if (pipe(p))
        return;
    /* Flags 1, 2 and 4 are PIDFD_SIGNAL_* (one at a time); 8 is unknown. */
    SYS("send(flags 8)", SYS_pidfd_send_signal, self, 0, 0, 8);
    SYS("send(flags 3)", SYS_pidfd_send_signal, self, 0, 0, 3);
    SYS("send(self, THREAD scope)", SYS_pidfd_send_signal, self, 0, 0, 1);
    SYS("send(self, THREAD_GROUP scope)", SYS_pidfd_send_signal, self, 0, 0, 2);
    SYS("send(PIDFD_SELF_THREAD)", SYS_pidfd_send_signal, -10000, 0, 0, 0);
    SYS("send(PIDFD_SELF_THREAD_GROUP)", SYS_pidfd_send_signal, -10001, 0, 0, 0);
    SYS("send(PIDFD_SELF_THREAD_GROUP, flags 8)", SYS_pidfd_send_signal, -10001, 0, 0, 8);
    SYS("send(-10002)", SYS_pidfd_send_signal, -10002, 0, 0, 0);
    SYS("send(closed fd)", SYS_pidfd_send_signal, 9999, 0, 0, 0);
    SYS("send(a pipe)", SYS_pidfd_send_signal, p[0], 0, 0, 0);
    SYS("send(self, 0)", SYS_pidfd_send_signal, self, 0, 0, 0);
    SYS("send(self, 65)", SYS_pidfd_send_signal, self, 65, 0, 0);
    SYS("send(bad siginfo)", SYS_pidfd_send_signal, self, 0, KERNEL_ADDR, 0);
    siginfo_t si;
    memset(&si, 0, sizeof si);
    si.si_signo = SIGUSR1;
    si.si_code = SI_QUEUE;
    SYS("send(si_signo differs)", SYS_pidfd_send_signal, self, 0, &si, 0);
    si.si_signo = 0;
    si.si_code = SI_USER;
    SYS("send(self, SI_USER info)", SYS_pidfd_send_signal, self, 0, &si, 0);
    flush();
    pid_t c = fork();
    if (c == 0) {
        pause();
        _exit(0);
    }
    int cfd = popen_(c, 0);
    SYS("send(child, SI_USER info)", SYS_pidfd_send_signal, cfd, 0, &si, 0);
    si.si_code = SI_TKILL;
    SYS("send(child, SI_TKILL info)", SYS_pidfd_send_signal, cfd, 0, &si, 0);
    si.si_code = SI_QUEUE;
    SYS("send(child, SI_QUEUE info)", SYS_pidfd_send_signal, cfd, 0, &si, 0);
    printf("poll(running child) revents = %d\n", ready(cfd, 0));
    siginfo_t w;
    memset(&w, 0, sizeof w);
    SYS("waitid(running child, WNOHANG)", SYS_waitid, P_PIDFD, cfd, &w, WEXITED | WNOHANG, 0);
    yes("si_pid 0", w.si_pid == 0);
    int nb = popen_(c, PIDFD_NONBLOCK_);
    SYS("waitid(nonblocking pidfd)", SYS_waitid, P_PIDFD, nb, &w, WEXITED, 0);
    SYS("send(child, SIGKILL)", SYS_pidfd_send_signal, cfd, SIGKILL, 0, 0);
    printf("poll(ending child, 5 s) revents = %d\n", ready(cfd, 5000));
    show_fdinfo("fdinfo(zombie)", cfd, c);
    SYS("send(zombie, 0)", SYS_pidfd_send_signal, cfd, 0, 0, 0);
    SYS("send(zombie, SIGTERM)", SYS_pidfd_send_signal, cfd, SIGTERM, 0, 0);
    memset(&w, 0, sizeof w);
    SYS("waitid(P_PIDFD)", SYS_waitid, P_PIDFD, cfd, &w, WEXITED, 0);
    yes("si_pid is the child", w.si_pid == c);
    printf("si_code = %d, si_status = %d\n", w.si_code, w.si_status);
    SYS("send(reaped)", SYS_pidfd_send_signal, cfd, 0, 0, 0);
    SYS("waitid(reaped)", SYS_waitid, P_PIDFD, cfd, &w, WEXITED | WNOHANG, 0);
    SYS("waitid(nonblocking, reaped)", SYS_waitid, P_PIDFD, nb, &w, WEXITED, 0);
    printf("poll(reaped) revents = %d\n", ready(cfd, 0));
    show_fdinfo("fdinfo(reaped)", cfd, -1);
    close(cfd);
    close(nb);
    close(self);
    close(p[0]);
    close(p[1]);
}

/* PIDFD_SIGNAL_PROCESS_GROUP through the pidfd of a group leader that was reaped while its group lives on
 * (linux-7.3.1/kernel/signal.c:4125 kill_pgrp_info): the members keep the pid as their pgid, so the signal
 * reaches them; once the group is gone too it is ESRCH. A subreaper collects the orphaned member. */
static void t_pgrp(void) {
    int p[2];
    if (pipe(p))
        return;
    prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    flush();
    pid_t l = fork();
    if (l == 0) {
        setpgid(0, 0);
        pid_t m = fork();
        if (m == 0) {
            for (;;)
                pause();
        }
        if (write(p[1], &m, sizeof m) != sizeof m)
            _exit(1);
        _exit(0);
    }
    int lfd = popen_(l, 0);
    pid_t m = 0;
    yes("the member's pid arrived", read(p[0], &m, sizeof m) == sizeof m && m > 0);
    int st = 0;
    yes("the leader was reaped", waitpid(l, &st, 0) == l && WIFEXITED(st));
    SYS("send(reaped leader, 0)", SYS_pidfd_send_signal, lfd, 0, 0, 0);
    SYS("send(reaped leader, PROCESS_GROUP, 0)", SYS_pidfd_send_signal, lfd, 0, 0, 4);
    SYS("send(reaped leader, PROCESS_GROUP, SIGKILL)", SYS_pidfd_send_signal, lfd, SIGKILL, 0, 4);
    st = 0;
    yes("the member ended by SIGKILL", m > 0 && waitpid(m, &st, 0) == m && WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);
    SYS("send(reaped leader, PROCESS_GROUP) after the group ended", SYS_pidfd_send_signal, lfd, 0, 0, 4);
    prctl(PR_SET_CHILD_SUBREAPER, 0, 0, 0, 0);
    close(lfd);
    close(p[0]);
    close(p[1]);
}

static void t_waitid(void) {
    siginfo_t w;
    int self = popen_(getpid(), 0);
    int e = eventfd(0, 0);
    SYS("waitid(P_PIDFD, -1)", SYS_waitid, P_PIDFD, -1, &w, WEXITED, 0);
    SYS("waitid(P_PIDFD, closed)", SYS_waitid, P_PIDFD, 9999, &w, WEXITED, 0);
    SYS("waitid(P_PIDFD, eventfd)", SYS_waitid, P_PIDFD, e, &w, WEXITED, 0);
    SYS("waitid(P_PIDFD, self)", SYS_waitid, P_PIDFD, self, &w, WEXITED | WNOHANG, 0);
    /* A child collected by P_PID leaves its pidfd naming a reaped process. */
    flush();
    pid_t c = fork();
    if (c == 0)
        _exit(5);
    int cfd = popen_(c, 0);
    SYS("waitid(P_PID)", SYS_waitid, P_PID, c, &w, WEXITED, 0);
    SYS("waitid(P_PIDFD after P_PID)", SYS_waitid, P_PIDFD, cfd, &w, WEXITED | WNOHANG, 0);
    close(cfd);
    close(e);
    close(self);
}

/* clone3 like fork, with CLONE_PIDFD: the child sleeps `ms` and exits with `code`. */
static long clone3_pidfd(int *pidfd, uint64_t extra, int ms, int code) {
    struct clone_args_v0 a;
    memset(&a, 0, sizeof a);
    a.flags = CLONE_PIDFD_ | extra;
    a.pidfd = (uint64_t)(uintptr_t)pidfd;
    a.exit_signal = SIGCHLD;
    flush();
    long r = syscall(SYS_clone3, &a, sizeof a);
    if (r == 0) {
        usleep(ms * 1000);
        _exit(code);
    }
    return r;
}

static void t_clone(void) {
    int pidfd = -1;
    SYS("clone3(PIDFD, bad pointer)", SYS_clone3, &(struct clone_args_v0){.flags = CLONE_PIDFD_, .pidfd = (uint64_t)KERNEL_ADDR, .exit_signal = SIGCHLD}, sizeof(struct clone_args_v0));
    SYS("wait4 after the failed clone3", SYS_wait4, -1, 0, WNOHANG, 0);
    /* CLONE_PIDFD | CLONE_THREAD gives a thread pidfd (ktest ab.l7.pidfd_thread). */
    long c = clone3_pidfd(&pidfd, 0, 100, 3);
    yes("clone3(PIDFD) made a child", c > 0);
    yes("pidfd >= 0", pidfd >= 0);
    yes("pidfd close-on-exec", fcntl(pidfd, F_GETFD) == FD_CLOEXEC);
    show_fdinfo("fdinfo(clone3 child)", pidfd, c);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 0x55};
    SYS("epoll_ctl(ADD pidfd)", SYS_epoll_ctl, ep, EPOLL_CTL_ADD, pidfd, &ev);
    struct epoll_event out[2];
    SYS("epoll_wait(running)", SYS_epoll_wait, ep, out, 2, 0);
    long n = epoll_wait(ep, out, 2, 5000);
    printf("epoll_wait(5 s) = %ld, events = %#x, data ok: %s\n", n, n == 1 ? out[0].events : 0, n == 1 && out[0].data.u64 == 0x55 ? "yes" : "no");
    printf("epoll_wait again = %d\n", epoll_wait(ep, out, 2, 0));
    printf("poll revents = %d\n", ready(pidfd, 0));
    siginfo_t w;
    memset(&w, 0, sizeof w);
    SYS("waitid(P_PIDFD, clone3 child)", SYS_waitid, P_PIDFD, pidfd, &w, WEXITED, 0);
    printf("si_code = %d, si_status = %d, si_pid ok: %s\n", w.si_code, w.si_status, w.si_pid == c ? "yes" : "no");
    close(ep);
    close(pidfd);
    /* A poll that sleeps until the child ends. */
    c = clone3_pidfd(&pidfd, 0, 100, 4);
    printf("poll(blocking) revents = %d\n", ready(pidfd, 5000));
    memset(&w, 0, sizeof w);
    SYS("waitid(P_PIDFD, blocking child)", SYS_waitid, P_PIDFD, pidfd, &w, WEXITED, 0);
    printf("si_status = %d\n", w.si_status);
    close(pidfd);
    /* Legacy clone: the pidfd goes to parent_tid. */
    SYS("clone(PIDFD|PARENT_SETTID)", SYS_clone, CLONE_PIDFD_ | CLONE_PARENT_SETTID | SIGCHLD, 0, &pidfd, 0, 0);
    pidfd = -1;
    flush();
    long r = syscall(SYS_clone, CLONE_PIDFD_ | SIGCHLD, 0, &pidfd, 0, 0);
    if (r == 0)
        _exit(6);
    yes("clone(PIDFD) made a child", r > 0);
    yes("legacy pidfd >= 0", pidfd >= 0);
    memset(&w, 0, sizeof w);
    SYS("waitid(P_PIDFD, clone child)", SYS_waitid, P_PIDFD, pidfd, &w, WEXITED, 0);
    printf("si_status = %d\n", w.si_status);
    close(pidfd);
    /* vfork-style: the pidfd is there before the parent waits for the child's exit. */
    pidfd = -1;
    flush();
    r = syscall(SYS_clone, CLONE_PIDFD_ | CLONE_VFORK | SIGCHLD, 0, &pidfd, 0, 0);
    if (r == 0)
        _exit(7);
    yes("vfork pidfd >= 0", pidfd >= 0);
    yes("vfork child ended (POLLIN)", ready(pidfd, 5000) & POLLIN);
    SYS("waitid(P_PIDFD, vfork child)", SYS_waitid, P_PIDFD, pidfd, &w, WEXITED, 0);
    close(pidfd);
}

static void t_getfd(void) {
    int sync[2];
    if (pipe(sync))
        return;
    flush();
    pid_t c = fork();
    if (c == 0) {
        int e = eventfd(7, 0);
        dup2(e, 50);
        close(e);
        if (write(sync[1], "x", 1) != 1)
            _exit(1);
        pause();
        _exit(0);
    }
    char x;
    if (read(sync[0], &x, 1) != 1)
        return;
    int cfd = popen_(c, 0);
    SYS("getfd(flags 1)", SYS_pidfd_getfd, cfd, 50, 1);
    SYS("getfd(closed pidfd)", SYS_pidfd_getfd, 9999, 50, 0);
    SYS("getfd(not a pidfd)", SYS_pidfd_getfd, sync[0], 50, 0);
    SYS("getfd(no such fd)", SYS_pidfd_getfd, cfd, 999, 0);
    SYS("getfd(-1)", SYS_pidfd_getfd, cfd, -1, 0);
    long n = syscall(SYS_pidfd_getfd, cfd, 50, 0);
    yes("getfd(child's eventfd) >= 0", n >= 0);
    yes("getfd close-on-exec", fcntl(n, F_GETFD) == FD_CLOEXEC);
    uint64_t v = 0;
    yes("the same eventfd (reads 7)", read(n, &v, 8) == 8 && v == 7);
    close(n);
    int self = popen_(getpid(), 0);
    n = syscall(SYS_pidfd_getfd, self, sync[0], 0);
    yes("getfd(own fd) >= 0", n >= 0);
    close(n);
    /* A child that made itself non-dumpable: the ptrace-attach rule refuses a same-uid caller (root may). */
    flush();
    pid_t d = fork();
    if (d == 0) {
        prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
        int e = eventfd(3, 0);
        dup2(e, 50);
        close(e);
        if (write(sync[1], "y", 1) != 1)
            _exit(1);
        pause();
        _exit(0);
    }
    if (read(sync[0], &x, 1) == 1) {
        int dfd = popen_(d, 0);
        errno = 0;
        long m = syscall(SYS_pidfd_getfd, dfd, 50, 0);
        int err = errno;
        printf("getfd(non-dumpable child): %s\n", getuid() == 0 ? (m >= 0 ? "allowed (root)" : "refused") : (m < 0 && err == EPERM ? "EPERM" : "allowed"));
        if (m >= 0)
            close(m);
        kill(d, SIGKILL);
        siginfo_t dw;
        SYS("waitid(P_PIDFD, non-dumpable)", SYS_waitid, P_PIDFD, dfd, &dw, WEXITED, 0);
        close(dfd);
    }
    kill(c, SIGKILL);
    printf("poll(killed child, 5 s) revents = %d\n", ready(cfd, 5000));
    /* linux-7.3.1/kernel/pid.c:861: the task is exiting (PF_EXITING): ESRCH. */
    SYS("getfd(zombie)", SYS_pidfd_getfd, cfd, 50, 0);
    siginfo_t w;
    SYS("waitid(P_PIDFD)", SYS_waitid, P_PIDFD, cfd, &w, WEXITED, 0);
    SYS("getfd(reaped)", SYS_pidfd_getfd, cfd, 50, 0);
    close(cfd);
    close(self);
    close(sync[0]);
    close(sync[1]);
}

int main(void) {
    t_open();
    t_thread();
    t_pidfs();
    t_signal();
    t_pgrp();
    t_waitid();
    t_clone();
    t_getfd();
    return 0;
}
