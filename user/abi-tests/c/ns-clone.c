/* SX-2: clone/clone3 with namespace flags (linux-6.6/kernel/fork.c copy_process :2260-2293, kernel/pid.c
 * alloc_pid, kernel/pid_namespace.c:169 zap_pid_ns_processes, kernel/signal.c:91 SIGNAL_UNKILLABLE): the
 * errno corners, Chromium's zygote shape (user + pid + net), a pid namespace's init (orphans, signals from
 * inside, its death ending the rest), a fork into a dead namespace, and clone3's set_tid. Shaped like
 * linux-6.6/tools/testing/selftests/pid_namespace and clone3/clone3_set_tid.c. */
#include "ns.h"
#include <poll.h>
#include <stdint.h>

struct cargs {
    uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup;
};

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
    return 0;
}
