/* SX-2: unshare(2) as an unprivileged user (linux-7.3.1/kernel/fork.c:3254 ksys_unshare, :3179 check_unshare_flags
 * with UNSHARE_EMPTY_MNTNS):
 * unknown flags, namespaces without CAP_SYS_ADMIN, the implied thread-group flags with a second thread, then a
 * new user namespace (unmapped ids, full capabilities there) and the namespaces it owns. Shaped like
 * linux-7.3.1/tools/testing/selftests/filesystems/nsfs and the user-namespace parts of the clone3 selftests. */
#include "ns.h"
#include <pthread.h>
#include <sys/utsname.h>

static int gate[2];

static void *thread_main(void *a) {
    (void)a;
    char c;
    if (read(gate[0], &c, 1) != 1)
        return 0;
    return 0;
}

static char other_cwd[256];
static mode_t other_mask;

/* The second thread of mode "threadfs": its cwd and umask after the caller's unshare(CLONE_FS). */
static void *thread_fs(void *a) {
    (void)a;
    char c;
    if (read(gate[0], &c, 1) != 1)
        return 0;
    if (!getcwd(other_cwd, sizeof other_cwd))
        snprintf(other_cwd, sizeof other_cwd, "-%s", en(errno));
    other_mask = umask(0);
    umask(other_mask);
    return 0;
}

static void caps_line(const char *who) {
    char buf[4096];
    rd("/proc/self/status", buf, sizeof buf);
    char *p = strstr(buf, "CapEff:");
    char *e = p ? strchr(p, '\n') : 0;
    if (e)
        *e = 0;
    printf("%s: %s\n", who, p ? p : "no CapEff");
}

static int child(void) {
    R("child: unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
    printf("child: uid %d euid %d gid %d egid %d\n", (int)getuid(), (int)geteuid(), (int)getgid(), (int)getegid());
    caps_line("child");
    R("child: nested unshare(CLONE_NEWUSER) with an unmapped euid", unshare(CLONE_NEWUSER));
    R("child: unshare(CLONE_NEWUTS|CLONE_NEWIPC|CLONE_NEWNET|CLONE_NEWPID)", unshare(CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWPID));
    R("child: a second unshare(CLONE_NEWPID) before a fork", unshare(CLONE_NEWPID));
    R("child: sethostname", sethostname("sx2-uts", 7));
    struct utsname u;
    uname(&u);
    printf("child: nodename %s\n", u.nodename);
    R("child: unshare(CLONE_NEWUTS) again", unshare(CLONE_NEWUTS));
    uname(&u);
    printf("child: nodename after a second uts namespace %s\n", u.nodename);
    R("child: unshare(CLONE_FS|CLONE_FILES)", unshare(CLONE_FS | CLONE_FILES));
    pid_t p = fork();
    if (p == 0) {
        printf("grandchild: getpid %d getppid %d\n", (int)getpid(), (int)getppid());
        _exit(0);
    }
    reap("grandchild", p);
    return 0;
}

/* Mode "threadfs": unshare(CLONE_FS) with a second thread (kernel/fork.c unshare_fs): 0, the caller alone gets
 * its own cwd and umask; the other thread keeps the old ones. */
static int threadfs(void) {
    if (pipe(gate))
        return 1;
    R("chdir(/)", chdir("/"));
    umask(022);
    pthread_t t;
    pthread_create(&t, 0, thread_fs, 0);
    R("threaded: unshare(CLONE_FS)", unshare(CLONE_FS));
    R("threaded: chdir(/proc)", chdir("/proc"));
    printf("threaded: umask(077) was %03o\n", (unsigned)umask(077));
    if (write(gate[1], "x", 1) != 1)
        return 1;
    pthread_join(t, 0);
    char cwd[256];
    mode_t m = umask(0);
    umask(m);
    printf("caller: getcwd %s, umask %03o\n", getcwd(cwd, sizeof cwd) ? cwd : en(errno), (unsigned)m);
    printf("other thread: getcwd %s, umask %03o\n", other_cwd, (unsigned)other_mask);
    return 0;
}

static char self_exe[256];

/* The second thread of mode "threadexec": its own cwd and umask, then execve (fs/exec.c keeps the caller's
 * fs_struct: the new image starts with the thread's state, not the leader's). */
static void *thread_exec(void *a) {
    (void)a;
    R("thread: unshare(CLONE_FS)", unshare(CLONE_FS));
    R("thread: chdir(/proc)", chdir("/proc"));
    printf("thread: umask(027) was %03o\n", (unsigned)umask(027));
    char *av[] = { "abi-ns-unshare", "execd", 0 }, *ev[] = { "PATH=/bin", 0 };
    execve(self_exe, av, ev);
    printf("thread: execve = -%s\n", en(errno));
    _exit(1);
}

/* Mode "threadexec": in a child, the leader keeps / and 022 and blocks; the other thread execs. */
static int threadexec(void) {
    ssize_t n = readlink("/proc/self/exe", self_exe, sizeof self_exe - 1);
    if (n < 0) {
        puts("no /proc/self/exe");
        return 1;
    }
    self_exe[n] = 0;
    R("chdir(/)", chdir("/"));
    umask(022);
    pid_t c = fork();
    if (c == 0) {
        if (pipe(gate))
            _exit(1);
        pthread_t t;
        pthread_create(&t, 0, thread_exec, 0);
        char x;
        if (read(gate[0], &x, 1) < 0)
            _exit(1);
        _exit(2);
    }
    reap("child", c);
    return 0;
}

/* Mode "execd": the image a thread's execve started. */
static int execd(void) {
    char cwd[256];
    mode_t m = umask(0);
    umask(m);
    printf("exec: getcwd %s, umask %03o\n", getcwd(cwd, sizeof cwd) ? cwd : en(errno), (unsigned)m);
    return 0;
}

/* What a process sees in an empty mount namespace (`copy_mnt_ns` with CLONE_EMPTY_MNTNS, linux-7.3.1/fs/
 * namespace.c:4420): cwd and root on the copy of the root mount, nothing mounted below it. /proc is reached
 * through a descriptor taken before; its mountinfo shows the reader's (new) namespace. */
static void empty_view(const char *who, int procfd) {
    char l[128], cwd[256];
    snprintf(l, sizeof l, "%s: getcwd", who);
    printf("%s = %s\n", l, getcwd(cwd, sizeof cwd) ? cwd : en(errno));
    struct stat st;
    snprintf(l, sizeof l, "%s: stat(/proc/self)", who);
    R(l, stat("/proc/self", &st));
    snprintf(l, sizeof l, "%s: stat(/)", who);
    R(l, stat("/", &st));
    int fd = openat(procfd, "self/mountinfo", O_RDONLY);
    char buf[4096];
    long n = fd < 0 ? -1 : read(fd, buf, sizeof buf - 1);
    int lines = 0;
    char points[256] = "";
    for (char *p = buf, *e; n > 0 && p < buf + n && (e = memchr(p, '\n', buf + n - p)); p = e + 1) {
        /* Field 5 is the mount point. */
        char *f = p;
        for (int i = 0; i < 4 && f; i++)
            f = memchr(f, ' ', e - f) ? memchr(f, ' ', e - f) + 1 : 0;
        char *end = f ? memchr(f, ' ', e - f) : 0;
        if (end && strlen(points) + (end - f) + 2 < sizeof points) {
            strncat(points, f, end - f);
            strcat(points, " ");
        }
        lines++;
    }
    printf("%s: mountinfo lines = %d, mount points: %s\n", who, lines, points);
    if (fd >= 0)
        close(fd);
}

static int empty(void) {
    int procfd = open("/proc", O_PATH | O_DIRECTORY);
    char before[64] = "", after[64] = "";
    readlinkat(procfd, "self/ns/mnt", before, sizeof before - 1);
    R("unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
    R("chdir(/proc/self)", chdir("/proc/self"));
    R("unshare(UNSHARE_EMPTY_MNTNS)", unshare(0x00100000));
    readlinkat(procfd, "self/ns/mnt", after, sizeof after - 1);
    printf("a new mount namespace = %s\n", yn(strcmp(before, after) != 0));
    empty_view("unshare", procfd);
    struct clone_args { uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup; } a;
    memset(&a, 0, sizeof a);
    a.flags = 1ull << 37;
    a.exit_signal = SIGCHLD;
    fflush(stdout);
    R("chdir(/) before clone3", chdir("/"));
    pid_t p = syscall(435, &a, sizeof a);
    if (p == 0) {
        empty_view("clone3 child", procfd);
        _exit(0);
    }
    /* After the child's lines: printed by the parent once the child is gone. */
    reap("clone3 child", p);
    printf("clone3(CLONE_EMPTY_MNTNS) = %s\n", yn(p > 0));
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "threadfs"))
        return threadfs();
    if (argc > 1 && !strcmp(argv[1], "threadexec"))
        return threadexec();
    if (argc > 1 && !strcmp(argv[1], "execd"))
        return execd();
    if (argc > 1 && !strcmp(argv[1], "empty"))
        return empty();
    struct utsname before, after;
    uname(&before);
    R("unshare(0)", unshare(0));
    R("unshare(SIGCHLD)", syscall(SYS_unshare, SIGCHLD));
    R("unshare(CLONE_PARENT)", unshare(CLONE_PARENT));
    R("unshare(CLONE_VFORK)", unshare(CLONE_VFORK));
    R("unshare(CLONE_NEWNET) unprivileged", unshare(CLONE_NEWNET));
    R("unshare(CLONE_NEWPID) unprivileged", unshare(CLONE_NEWPID));
    R("unshare(CLONE_NEWUTS) unprivileged", unshare(CLONE_NEWUTS));
    R("unshare(CLONE_NEWIPC) unprivileged", unshare(CLONE_NEWIPC));
    R("unshare(CLONE_NEWNS) unprivileged", unshare(CLONE_NEWNS));
    R("unshare(CLONE_NEWCGROUP) unprivileged", unshare(CLONE_NEWCGROUP));
    R("unshare(UNSHARE_EMPTY_MNTNS) unprivileged", unshare(0x00100000));
    R("unshare(UNSHARE_EMPTY_MNTNS|CLONE_VFORK)", unshare(0x00100000 | CLONE_VFORK));
    R("unshare(CLONE_FS)", unshare(CLONE_FS));
    R("unshare(CLONE_FILES)", unshare(CLONE_FILES));
    R("unshare(CLONE_SYSVSEM)", unshare(CLONE_SYSVSEM));
    R("unshare(CLONE_THREAD) single-threaded", unshare(CLONE_THREAD));
    R("unshare(CLONE_SIGHAND) single-threaded", unshare(CLONE_SIGHAND));
    R("unshare(CLONE_VM) single-threaded", unshare(CLONE_VM));
    caps_line("outside");

    if (pipe(gate))
        return 1;
    pthread_t t;
    pthread_create(&t, 0, thread_main, 0);
    R("threaded: unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
    R("threaded: unshare(CLONE_THREAD)", unshare(CLONE_THREAD));
    R("threaded: unshare(CLONE_VM)", unshare(CLONE_VM));
    if (write(gate[1], "x", 1) != 1)
        return 1;
    pthread_join(t, 0);

    pid_t p = fork();
    if (p == 0)
        _exit(child());
    reap("child", p);
    uname(&after);
    printf("parent's nodename unchanged = %s\n", yn(!strcmp(before.nodename, after.nodename)));
    return 0;
}
