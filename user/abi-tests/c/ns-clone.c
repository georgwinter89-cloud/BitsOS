/* SX-2: clone/clone3 with namespace flags (linux-7.3.1/kernel/fork.c copy_process :2039-2059, and L7-PROC:
 * clone3's CLONE_AUTOREAP, CLONE_NNP, CLONE_PIDFD_AUTOKILL, CLONE_EMPTY_MNTNS :2089-2117, :2744; kernel.pid_max per
 * pid namespace, kernel/pid.c:782), linux-7.3.1/kernel/pid.c:159
 * alloc_pid, kernel/pid_namespace.c:192 zap_pid_ns_processes, kernel/signal.c:94 SIGNAL_UNKILLABLE): the
 * errno corners, Chromium's zygote shape (user + pid + net), a pid namespace's init (orphans, signals from
 * inside, its death ending the rest), a fork into a dead namespace, and clone3's set_tid. Shaped like
 * linux-7.3.1/tools/testing/selftests/pid_namespace and clone3/clone3_set_tid.c. */
#include "ns.h"
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <stdint.h>

struct cargs {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup;
};

static long clone3_x(uint64_t flags, uint64_t sig, int *pidfd) {
    struct cargs a = { 0 };
    a.flags = flags;
    a.exit_signal = sig;
    a.pidfd = (uint64_t)(uintptr_t)pidfd;
    return syscall(435, &a, sizeof a);
}

#define CLONE_AUTOREAP_ (1ull << 34)
#define CLONE_NNP_ (1ull << 35)
#define CLONE_PIDFD_AUTOKILL_ (1ull << 36)
#define CLONE_EMPTY_MNTNS_ (1ull << 37)
#define CLONE_PIDFD_ 0x1000ull

/* The exit code PIDFD_GET_INFO keeps for a reaped task, once its pidfd reads EPOLLHUP (-1 when it does not). */
static int exit_info(int pidfd) {
    struct pollfd p = { pidfd, POLLIN, 0 };
    for (int i = 0; i < 500; i++) {
        if (poll(&p, 1, 10) == 1 && (p.revents & POLLHUP))
            break;
    }
    uint64_t info[11] = { 8 };
    if (ioctl(pidfd, 0xc058ff0bul, info) < 0 || !(info[0] & 8))
        return -1;
    return (int)(info[7] >> 32);
}

static void linux7_flags(void) {
    int pfd = -1;
    R("clone3(AUTOREAP with SIGCHLD)", clone3_x(CLONE_AUTOREAP_, SIGCHLD, 0));
    R("clone3(AUTOREAP|PARENT)", clone3_x(CLONE_AUTOREAP_ | CLONE_PARENT, 0, 0));
    R("clone3(NNP|thread)", clone3_x(CLONE_NNP_ | CLONE_THREAD | CLONE_SIGHAND | CLONE_VM, 0, 0));
    R("clone3(PIDFD_AUTOKILL|AUTOREAP) without PIDFD", clone3_x(CLONE_PIDFD_AUTOKILL_ | CLONE_AUTOREAP_, 0, 0));
    R("clone3(PIDFD_AUTOKILL|PIDFD) without AUTOREAP", clone3_x(CLONE_PIDFD_AUTOKILL_ | CLONE_PIDFD_, SIGCHLD, &pfd));
    R("clone3(PIDFD_AUTOKILL|PIDFD|AUTOREAP) without NNP, unprivileged", clone3_x(CLONE_PIDFD_AUTOKILL_ | CLONE_PIDFD_ | CLONE_AUTOREAP_, 0, &pfd));
    R("clone3(EMPTY_MNTNS) unprivileged", clone3_x(CLONE_EMPTY_MNTNS_, SIGCHLD, 0));
    R("clone3(bit 38)", clone3_x(1ull << 38, SIGCHLD, 0));
    /* AUTOREAP | NNP: no SIGCHLD (blocked, so it would stay pending), no zombie; the exit code in the pidfs. */
    sigset_t chld, old;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    sigprocmask(SIG_BLOCK, &chld, &old);
    fflush(stdout);
    long c = clone3_x(CLONE_AUTOREAP_ | CLONE_NNP_ | CLONE_PIDFD_, 0, &pfd);
    if (c == 0)
        _exit(40 + prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0));
    printf("clone3(AUTOREAP|NNP|PIDFD) made a child = %s\n", yn(c > 0));
    printf("its exit code through PIDFD_GET_INFO = %#x\n", exit_info(pfd));
    R("wait4 after the autoreap child", wait4(-1, 0, WNOHANG, 0));
    sigset_t pend;
    sigpending(&pend);
    printf("SIGCHLD pending = %s\n", yn(sigismember(&pend, SIGCHLD)));
    sigprocmask(SIG_SETMASK, &old, 0);
    close(pfd);
    /* PIDFD_AUTOKILL: closing the clone pidfd kills the child (seen through a second pidfd). */
    fflush(stdout);
    c = clone3_x(CLONE_AUTOREAP_ | CLONE_NNP_ | CLONE_PIDFD_ | CLONE_PIDFD_AUTOKILL_, 0, &pfd);
    if (c == 0) {
        for (;;)
            pause();
    }
    printf("F_GETFL(autokill pidfd) = %#o\n", fcntl(pfd, F_GETFL));
    int other = (int)syscall(434, c, 0);
    close(pfd);
    printf("the child after the close: exit code %#x\n", exit_info(other));
    close(other);
}

/* kernel.pid_max belongs to the reader's pid namespace (linux-7.3.1/kernel/pid.c:782). */
static long read_pid_max(void) {
    char b[32] = { 0 };
    int fd = open("/proc/sys/kernel/pid_max", O_RDONLY);
    if (fd < 0)
        return -errno;
    long n = read(fd, b, sizeof b - 1);
    close(fd);
    return n > 0 ? strtol(b, 0, 10) : -1;
}

static long write_pid_max(const char *v) {
    int fd = open("/proc/sys/kernel/pid_max", O_WRONLY);
    if (fd < 0)
        return -errno;
    long n = write(fd, v, strlen(v));
    long e = n < 0 ? -errno : n;
    close(fd);
    return e;
}

static void pid_max_ns(void) {
    long before = read_pid_max();
    printf("pid_max write from outside, unprivileged = %ld\n", write_pid_max("5000"));
    pid_t h = fork();
    if (h == 0) {
        R("pid_max: unshare(CLONE_NEWUSER|CLONE_NEWPID)", unshare(CLONE_NEWUSER | CLONE_NEWPID));
        pid_t i = fork();
        if (i == 0) {
            printf("pid_max in a new namespace = %ld\n", read_pid_max());
            printf("write 100 = %ld\n", write_pid_max("100"));
            printf("write 5000 = %ld\n", write_pid_max("5000"));
            printf("pid_max there now = %ld\n", read_pid_max());
            _exit(0);
        }
        reap("pid_max: init", i);
        printf("pid_max: the creator still reads its own = %s\n", yn(read_pid_max() == before));
        _exit(0);
    }
    reap("pid_max helper", h);
    printf("pid_max outside unchanged = %s\n", yn(read_pid_max() == before));
}

/* A pidfd of a reaped child inside a pid namespace (linux-7.3.1/kernel/signal.c:4042 access_pidfd_pidns,
 * fs/pidfs.c:330 pid_in_current_pidns): the pid keeps its namespace after the reap, so pidfd_send_signal
 * answers ESRCH (not EINVAL) and PIDFD_GET_INFO still gives the exit code. */
static void pidfd_reaped_in_ns(void) {
    fflush(stdout);
    pid_t h = fork();
    if (h == 0) {
        R("reaped: unshare(CLONE_NEWUSER|CLONE_NEWPID)", unshare(CLONE_NEWUSER | CLONE_NEWPID));
        fflush(stdout);
        pid_t i = fork();
        if (i == 0) {
            int pfd = -1;
            fflush(stdout);
            long c = clone3_x(CLONE_PIDFD_, SIGCHLD, &pfd);
            if (c == 0)
                _exit(7);
            reap("reaped: child", c);
            R("reaped: pidfd_send_signal(0)", syscall(424, pfd, 0, 0, 0));
            printf("reaped: exit code through PIDFD_GET_INFO = %#x\n", exit_info(pfd));
            _exit(0);
        }
        reap("reaped: init", i);
        _exit(0);
    }
    reap("reaped helper", h);
}

static long clone3_tid(uint64_t flags, pid_t *tids, size_t n) {
    struct cargs a = { 0 };
    a.flags = flags;
    a.exit_signal = SIGCHLD;
    a.set_tid = (uint64_t)(uintptr_t)tids;
    a.set_tid_size = n;
    return syscall(435, &a, sizeof a);
}

/* Whether the pipe reaches EOF (every writer gone) within `ms`. */
static int eof_within(int fd, int ms) {
    for (int t = 0; t < ms; t += 20) {
        struct pollfd p = { fd, POLLIN, 0 };
        if (poll(&p, 1, 20) == 1) {
            char c;
            if (read(fd, &c, 1) == 0)
                return 1;
        }
    }
    return 0;
}

/* pid 1 of Chromium's zygote namespace. `w`: a pipe end its last member keeps open. */
static int zygote(int w) {
    printf("init: getpid %d getppid %d\n", (int)getpid(), (int)getppid());
    R("init: clone(CLONE_PARENT)", rclone(CLONE_PARENT | SIGCHLD));
    pid_t g = fork();
    if (g == 0) {
        printf("member: getpid %d getppid %d\n", (int)getpid(), (int)getppid());
        R("member: kill(1, SIGTERM)", kill(1, SIGTERM));
        R("member: kill(1, SIGKILL)", kill(1, SIGKILL));
        _exit(0);
    }
    reap("member", g);
    usleep(100000);
    printf("init alive after SIGTERM and SIGKILL from inside = yes\n");
    /* An orphan inside goes to this init, not to the global one. */
    int sync[2];
    if (pipe(sync))
        return 1;
    pid_t mid = fork();
    if (mid == 0) {
        pid_t o = fork();
        if (o == 0) {
            close(sync[0]);
            for (int i = 0; i < 200 && getppid() != 1; i++)
                usleep(10000);
            printf("orphan: getppid %d\n", (int)getppid());
            _exit(0);
        }
        _exit(0);
    }
    close(sync[1]);
    printf("orphan done = %s\n", yn(eof_within(sync[0], 5000)));
    reap("orphan's parent", mid);
    int st;
    pid_t any = wait(&st);
    printf("init reaped the orphan = %s\n", yn(any > 0 && WIFEXITED(st)));
    /* A sleeper holds `w`; the init's end must kill it. */
    pid_t s = fork();
    if (s == 0) {
        for (;;)
            pause();
    }
    close(w);
    printf("init: ends with a member left\n");
    return 7;
}

int main(void) {
    setvbuf(stdout, 0, _IOLBF, 0);
    R("clone(CLONE_NEWUSER|CLONE_FS)", rclone(CLONE_NEWUSER | CLONE_FS | SIGCHLD));
    R("clone(CLONE_NEWNS|CLONE_FS)", rclone(CLONE_NEWNS | CLONE_FS | SIGCHLD));
    R("clone(thread|CLONE_NEWUSER)", rclone(CLONE_THREAD | CLONE_SIGHAND | CLONE_VM | CLONE_NEWUSER));
    R("clone(thread|CLONE_NEWPID)", rclone(CLONE_THREAD | CLONE_SIGHAND | CLONE_VM | CLONE_NEWPID));
    R("clone(CLONE_THREAD without CLONE_SIGHAND)", rclone(CLONE_THREAD | CLONE_VM));
    R("clone(CLONE_SIGHAND without CLONE_VM)", rclone(CLONE_SIGHAND | SIGCHLD));
    R("clone(CLONE_NEWPID) unprivileged", rclone(CLONE_NEWPID | SIGCHLD));
    R("clone(CLONE_NEWNET) unprivileged", rclone(CLONE_NEWNET | SIGCHLD));

    /* Chromium's probe child: a user namespace, exit at once. */
    long c = rclone(CLONE_NEWUSER | SIGCHLD);
    if (c == 0) {
        printf("probe: uid %d\n", (int)getuid());
        _exit(3);
    }
    if (c > 0)
        reap("probe", c);
    printf("clone(CLONE_NEWUSER) made a child = %s\n", yn(c > 0));

    /* The zygote: user + pid + net namespaces. */
    int p[2];
    if (pipe(p))
        return 1;
    long z = rclone(CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET | SIGCHLD);
    if (z == 0) {
        close(p[0]);
        _exit(zygote(p[1]));
    }
    close(p[1]);
    reap("init", z);
    printf("the namespace's last member ended with its init = %s\n", yn(eof_within(p[0], 10000)));

    /* A fork into a namespace whose init is gone. */
    pid_t h = fork();
    if (h == 0) {
        R("helper: unshare(CLONE_NEWUSER|CLONE_NEWPID)", unshare(CLONE_NEWUSER | CLONE_NEWPID));
        pid_t i = fork();
        if (i == 0)
            _exit(0);
        reap("helper: init", i);
        pid_t again = fork();
        if (again == 0)
            _exit(9);
        R("helper: fork into the dead namespace", again);
        _exit(0);
    }
    reap("helper", h);

    /* clone3 set_tid (the innermost level). */
    h = fork();
    if (h == 0) {
        R("set_tid: unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
        pid_t one[1] = { 1 };
        long k = clone3_tid(CLONE_NEWPID, one, 1);
        if (k == 0) {
            printf("set_tid child: getpid %d\n", (int)getpid());
            _exit(0);
        }
        if (k > 0)
            reap("set_tid child", k);
        printf("clone3(CLONE_NEWPID, set_tid {1}) made a child = %s\n", yn(k > 0));
        pid_t five[1] = { 5 };
        R("clone3(CLONE_NEWPID, set_tid {5})", clone3_tid(CLONE_NEWPID, five, 1));
        pid_t two[2] = { 1, 30001 };
        R("clone3(CLONE_NEWPID, set_tid {1, n}) without CAP_CHECKPOINT_RESTORE outside", clone3_tid(CLONE_NEWPID, two, 2));
        pid_t three[3] = { 1, 1, 1 };
        R("clone3(CLONE_NEWPID, set_tid of 3 levels)", clone3_tid(CLONE_NEWPID, three, 3));
        R("clone3(set_tid {1}) in the caller's namespace", clone3_tid(0, one, 1));
        _exit(0);
    }
    reap("set_tid helper", h);
    linux7_flags();
    pid_max_ns();
    pidfd_reaped_in_ns();
    return 0;
}
