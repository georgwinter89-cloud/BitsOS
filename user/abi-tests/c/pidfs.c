/* L7-RESTE-AB 2: pidfs file handles and setns through a pidfd.
 * Mode "handle": name_to_handle_at / open_by_handle_at on pidfds (linux-7.3.1/fs/fhandle.c, fs/pidfs.c:881
 * pidfs_encode_fh, :917 pidfs_fh_to_dentry, :973 pidfs_export_permission): the handle is the pid's pidfs inode
 * number (type FILEID_KERNFS), it opens a pidfd again while the task is not reaped and seen from the caller's pid
 * namespace, and every other answer on the way (EINVAL, EOVERFLOW, EFAULT, EBADF, EPERM, ESTALE).
 * Mode "setns": setns(pidfd, CLONE_NEW* flags) enters several namespaces of the target at once
 * (linux-7.3.1/kernel/nsproxy.c:569 setns, :396 validate_nsset, :531 commit_nsset): all or nothing.
 * Only relations and errno names are printed, never pids, inode or mount numbers. */
#include "ns.h"
#include <pthread.h>
#include <sys/eventfd.h>
#include <sys/prctl.h>
#include <sys/utsname.h>

#define PIDFD_THREAD_ O_EXCL
#define FD_PIDFS_ROOT_ -10002
#define FD_NSFS_ROOT_ -10003
#define AT_HANDLE_FID_ 0x200
#define AT_HANDLE_MNT_ID_UNIQUE_ 0x001
#define AT_HANDLE_CONNECTABLE_ 0x002
#define FILEID_IS_CONNECTABLE_ 0x10000
#define FILEID_IS_DIR_ 0x20000

struct fh {
    unsigned int bytes;
    int type;
    unsigned char h[136];
};

static const char *nm(int e) {
    if (e == ESTALE)
        return "ESTALE";
    if (e == EOVERFLOW)
        return "EOVERFLOW";
    return en(e);
}

static long say(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, nm(errno));
    else
        printf("%s = %ld\n", label, r);
    return r;
}

static long n2h(int dfd, const char *name, void *h, void *mnt, int flags) {
    return syscall(SYS_name_to_handle_at, dfd, name, h, mnt, flags);
}

static long obh(int mfd, void *h, int flags) {
    return syscall(SYS_open_by_handle_at, mfd, h, flags);
}

static long popen_(pid_t pid, unsigned flags) {
    return syscall(SYS_pidfd_open, pid, flags);
}

static unsigned long long ino_of(int fd) {
    struct stat st;
    return fstat(fd, &st) == 0 ? (unsigned long long)st.st_ino : 0;
}

/* A field of /proc/self/fdinfo/<fd> ("mnt_id:", "Pid:"); -2 when missing. */
static long fdinfo(int fd, const char *key) {
    char path[64], buf[256];
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    FILE *f = fopen(path, "r");
    long v = -2;
    if (!f)
        return v;
    size_t k = strlen(key);
    while (fgets(buf, sizeof buf, f))
        if (!strncmp(buf, key, k))
            v = strtol(buf + k, 0, 10);
    fclose(f);
    return v;
}

/* The handle of pidfd `fd`; 0 or -errno. */
static int handle_of(int fd, struct fh *h) {
    int mnt;
    h->bytes = 128;
    return n2h(fd, "", h, &mnt, AT_EMPTY_PATH) == 0 ? 0 : -errno;
}

/* open_by_handle_at's pidfd: its pid (fdinfo), F_GETFL and F_GETFD, then closed. */
static void opened(const char *label, long fd, pid_t want) {
    char l[160];
    snprintf(l, sizeof l, "%s", label);
    if (say(l, fd < 0 ? -1 : 0) < 0)
        return;
    printf("%s: names the pid = %s, F_GETFL = 0%o, F_GETFD = %d\n", label, yn(fdinfo(fd, "Pid:") == want), fcntl(fd, F_GETFL),
           fcntl(fd, F_GETFD));
    close(fd);
}

/* AT_HANDLE_FID of a file outside pidfs (fs/exportfs/expfs.c:389): its size and type, and whether it holds the
 * inode number where that type keeps it (64-bit fid: first; tmpfs: after the generation; 32-bit: first). */
static void fid(const char *label, int dfd, const char *name, int flags, unsigned long long ino) {
    struct fh h = {128, 0, {0}};
    int mnt;
    char l[96];
    snprintf(l, sizeof l, "n2h(%s, FID)", label);
    if (say(l, n2h(dfd, name, &h, &mnt, flags | AT_HANDLE_FID_)) != 0)
        return;
    unsigned w[3];
    memcpy(w, h.h, sizeof w);
    unsigned long long got = h.type == 0x81 ? w[0] | (unsigned long long)w[1] << 32
                             : h.bytes == 12 ? w[1] | (unsigned long long)w[2] << 32
                                             : w[0];
    printf("  handle_bytes %u, type 0x%x, holds the inode = %s\n", h.bytes, h.type, yn(got == (h.bytes == 8 ? (unsigned)ino : ino)));
}

static unsigned long long ino_at(const char *path) {
    struct stat st;
    return stat(path, &st) == 0 ? (unsigned long long)st.st_ino : 0;
}

static void fids(void) {
    fid("/proc/self/status", AT_FDCWD, "/proc/self/status", 0, ino_at("/proc/self/status"));
    struct fh h = {11, 0, {0}};
    int mnt;
    say("n2h(/proc/self/status, FID, handle_bytes 11)", n2h(AT_FDCWD, "/proc/self/status", &h, &mnt, AT_HANDLE_FID_));
    printf("  handle_bytes %u, type 0x%x\n", h.bytes, h.type);
    char tmp[64];
    snprintf(tmp, sizeof tmp, "/tmp/abi-pidfs-%d", getpid());
    int t = open(tmp, O_CREAT | O_RDWR, 0600);
    fid("tmpfs file", AT_FDCWD, tmp, 0, ino_at(tmp));
    fid("tmpfs file by fd", t, "", AT_EMPTY_PATH, ino_of(t));
    close(t);
    unlink(tmp);
    int efd = eventfd(0, 0), p[2];
    fid("eventfd", efd, "", AT_EMPTY_PATH, ino_of(efd));
    close(efd);
    if (pipe(p) == 0) {
        fid("pipe", p[0], "", AT_EMPTY_PATH, ino_of(p[0]));
        close(p[0]);
        close(p[1]);
    }
}

/* Whether SIGUSR1 is in the pending set `key` of a status file. */
static int usr1_pending(const char *path, const char *key) {
    char buf[256];
    FILE *f = fopen(path, "r");
    unsigned long long v = 0;
    size_t k = strlen(key);
    if (!f)
        return 0;
    while (fgets(buf, sizeof buf, f))
        if (!strncmp(buf, key, k))
            v = strtoull(buf + k, 0, 16);
    fclose(f);
    return v >> (SIGUSR1 - 1) & 1;
}

static int stop[2];
static volatile pid_t thread_tid;

static void *idle(void *a) {
    (void)a;
    thread_tid = syscall(SYS_gettid);
    char c;
    if (read(stop[0], &c, 1) < 0)
        return 0;
    return 0;
}

static int handle(void) {
    pid_t me = getpid();
    int pfd = popen_(me, 0);
    struct fh h, g;
    int mnt = -7;
    printf("pidfd_open = %s\n", yn(pfd >= 0));

    /* name_to_handle_at: the flags first, then the name, then the file system, then the handle. */
    h.bytes = 128;
    say("n2h(bad flag)", n2h(pfd, "", &h, &mnt, AT_EMPTY_PATH | 0x4000));
    say("n2h(CONNECTABLE|EMPTY_PATH)", n2h(pfd, "", &h, &mnt, AT_EMPTY_PATH | AT_HANDLE_CONNECTABLE_));
    say("n2h(CONNECTABLE|FID)", n2h(AT_FDCWD, "/", &h, &mnt, AT_HANDLE_FID_ | AT_HANDLE_CONNECTABLE_));
    say("n2h(pidfd, \"\") without EMPTY_PATH", n2h(pfd, "", &h, &mnt, 0));
    say("n2h(pidfd, \"x\")", n2h(pfd, "x", &h, &mnt, 0));
    say("n2h(bad name)", n2h(AT_FDCWD, KERNEL_ADDR, &h, &mnt, 0));
    say("n2h(-1, \"\", EMPTY_PATH)", n2h(-1, "", &h, &mnt, AT_EMPTY_PATH));
    say("n2h(missing file)", n2h(AT_FDCWD, "/no/such/file", &h, &mnt, 0));
    say("n2h(/proc/self/status)", n2h(AT_FDCWD, "/proc/self/status", &h, &mnt, 0));
    int efd = eventfd(0, 0);
    say("n2h(eventfd)", n2h(efd, "", &h, &mnt, AT_EMPTY_PATH));
    close(efd);
    fids();
    say("n2h(bad handle address)", n2h(pfd, "", KERNEL_ADDR, &mnt, AT_EMPTY_PATH));
    h.bytes = 129;
    say("n2h(handle_bytes 129)", n2h(pfd, "", &h, &mnt, AT_EMPTY_PATH));
    h.bytes = 4;
    h.type = 7;
    mnt = -7;
    say("n2h(handle_bytes 4)", n2h(pfd, "", &h, &mnt, AT_EMPTY_PATH));
    printf("  too small: handle_bytes %u, type %#x, mount id written = %s\n", h.bytes, h.type, yn(mnt != -7));
    h.bytes = 128;
    h.type = 7;
    say("n2h(bad mount id address)", n2h(pfd, "", &h, KERNEL_ADDR, AT_EMPTY_PATH));
    printf("  bad mount id: handle untouched = %s\n", yn(h.bytes == 128 && h.type == 7));
    memset(&h, 0, sizeof h);
    h.bytes = 128;
    mnt = -7;
    say("n2h(pidfd)", n2h(pfd, "", &h, &mnt, AT_EMPTY_PATH));
    unsigned long long hino;
    memcpy(&hino, h.h, 8);
    printf("  handle_bytes %u, type %#x, holds the pidfs inode = %s, mount id as fdinfo's = %s\n", h.bytes, h.type,
           yn(hino == ino_of(pfd)), yn(mnt == fdinfo(pfd, "mnt_id:")));
    g.bytes = 128;
    say("n2h(pidfd, FID)", n2h(pfd, "", &g, &mnt, AT_EMPTY_PATH | AT_HANDLE_FID_));
    printf("  FID handle is the same = %s\n", yn(g.bytes == h.bytes && g.type == h.type && !memcmp(g.h, h.h, 8)));
    unsigned long long umnt = 0;
    g.bytes = 128;
    say("n2h(pidfd, MNT_ID_UNIQUE)", n2h(pfd, "", &g, &umnt, AT_EMPTY_PATH | AT_HANDLE_MNT_ID_UNIQUE_));
    printf("  the same handle = %s\n", yn(!memcmp(g.h, h.h, 8)));
    int tfd = popen_(me, PIDFD_THREAD_);
    g.bytes = 128;
    say("n2h(thread pidfd of the leader)", n2h(tfd, "", &g, &mnt, AT_EMPTY_PATH));
    printf("  the same handle as the process's = %s\n", yn(!memcmp(g.h, h.h, 8)));
    close(tfd);

    /* open_by_handle_at: the handle header, the anchor, the flags, the full handle, then the lookup. */
    opened("obh(pidfd, O_RDONLY)", obh(pfd, &h, O_RDONLY), me);
    opened("obh(PIDFS_ROOT, O_CLOEXEC)", obh(FD_PIDFS_ROOT_, &h, O_CLOEXEC), me);
    opened("obh(PIDFS_ROOT, O_NONBLOCK)", obh(FD_PIDFS_ROOT_, &h, O_NONBLOCK), me);
    opened("obh(PIDFS_ROOT, O_EXCL)", obh(FD_PIDFS_ROOT_, &h, O_EXCL), me);
    opened("obh(PIDFS_ROOT, O_WRONLY)", obh(FD_PIDFS_ROOT_, &h, O_WRONLY), me);
    opened("obh(PIDFS_ROOT, O_RDWR|O_LARGEFILE)", obh(FD_PIDFS_ROOT_, &h, O_RDWR | O_LARGEFILE), me);
    int o = obh(FD_PIDFS_ROOT_, &h, 0);
    printf("obh: the same pidfs inode = %s\n", yn(o >= 0 && ino_of(o) == ino_of(pfd)));
    close(o);
    say("obh(O_TRUNC)", obh(FD_PIDFS_ROOT_, &h, O_TRUNC));
    say("obh(O_CREAT)", obh(FD_PIDFS_ROOT_, &h, O_CREAT));
    say("obh(O_DIRECTORY)", obh(FD_PIDFS_ROOT_, &h, O_DIRECTORY));
    say("obh(O_PATH)", obh(FD_PIDFS_ROOT_, &h, O_PATH));
    say("obh(O_APPEND)", obh(pfd, &h, O_APPEND));
    say("obh(bad handle address)", obh(pfd, KERNEL_ADDR, 0));
    g = h;
    g.bytes = 0;
    say("obh(handle_bytes 0)", obh(pfd, &g, 0));
    g.bytes = 129;
    say("obh(handle_bytes 129)", obh(pfd, &g, 0));
    g = h;
    g.type = -1;
    say("obh(type -1)", obh(pfd, &g, 0));
    g.type = h.type | 0x40000;
    say("obh(unknown user flag)", obh(pfd, &g, 0));
    g.type = h.type | FILEID_IS_CONNECTABLE_;
    opened("obh(IS_CONNECTABLE) under its own pidfd", obh(pfd, &g, 0), me);
    say("obh(IS_CONNECTABLE) under PIDFS_ROOT", obh(FD_PIDFS_ROOT_, &g, 0));
    g.type = h.type | FILEID_IS_DIR_;
    say("obh(IS_DIR)", obh(pfd, &g, 0));
    g.type = 1;
    say("obh(type 1)", obh(pfd, &g, 0));
    g = h;
    g.bytes = 4;
    say("obh(handle_bytes 4)", obh(pfd, &g, 0));
    g = h;
    g.bytes = 12;
    opened("obh(handle_bytes 12)", obh(pfd, &g, 0), me);
    unsigned long long none = ~0ull;
    memcpy(g.h, &none, 8);
    g.bytes = 8;
    say("obh(unknown inode)", obh(pfd, &g, 0));
    say("obh(-1)", obh(-1, &h, 0));
    say("obh(-10001)", obh(-10001, &h, 0));
    say("obh(closed fd)", obh(999, &h, 0));
    say("obh(-1, bad flags)", obh(-1, &h, O_TRUNC));
    say("obh(NSFS_ROOT)", obh(FD_NSFS_ROOT_, &h, 0));
    int nsf = open("/proc/self/ns/net", O_RDONLY);
    say("obh(ns file)", obh(nsf, &h, 0));
    close(nsf);
    say("obh(AT_FDCWD) unprivileged", obh(AT_FDCWD, &h, 0));
    say("obh(AT_FDCWD, O_DIRECTORY) unprivileged", obh(AT_FDCWD, &h, O_DIRECTORY));
    int dir = open("/", O_RDONLY | O_DIRECTORY);
    say("obh(a directory) unprivileged", obh(dir, &h, 0));
    close(dir);
    say("obh(AT_FDCWD, bad handle address)", obh(AT_FDCWD, KERNEL_ADDR, 0));

    /* A child: its handle opens while it lives and while it is a zombie, not after the reap. */
    int go[2];
    if (pipe(go))
        return 1;
    pid_t kid = fork();
    if (kid == 0) {
        char c;
        close(go[1]);
        if (read(go[0], &c, 1) < 0)
            _exit(1);
        _exit(3);
    }
    close(go[0]);
    int kfd = popen_(kid, 0);
    struct fh k;
    printf("child handle = %d\n", handle_of(kfd, &k));
    unsigned long long kino;
    memcpy(&kino, k.h, 8);
    printf("child handle differs from ours = %s\n", yn(kino != hino));
    opened("obh(child)", obh(kfd, &k, 0), kid);
    g = k;
    g.type |= FILEID_IS_CONNECTABLE_;
    say("obh(child's handle, IS_CONNECTABLE) under our pidfd", obh(pfd, &g, 0));
    opened("obh(child's handle, IS_CONNECTABLE) under its pidfd", obh(kfd, &g, 0), kid);
    close(go[1]);
    siginfo_t si;
    waitid(P_PID, kid, &si, WEXITED | WNOWAIT);
    opened("obh(zombie child)", obh(FD_PIDFS_ROOT_, &k, 0), kid);
    reap("child", kid);
    say("obh(reaped child)", obh(FD_PIDFS_ROOT_, &k, 0));
    int again = handle_of(kfd, &g);
    printf("n2h(reaped child's pidfd) = %d, the same handle = %s\n", again, yn(!memcmp(g.h, k.h, 8)));
    close(kfd);

    /* A thread: its own inode; O_EXCL opens a thread pidfd; gone with the thread. SIGUSR1 stays blocked in both
     * threads so a signal through the pidfds waits where it was sent. */
    pthread_t th;
    sigset_t usr1, old;
    sigemptyset(&usr1);
    sigaddset(&usr1, SIGUSR1);
    sigprocmask(SIG_BLOCK, &usr1, &old);
    if (pipe(stop) || pthread_create(&th, 0, idle, 0))
        return 1;
    while (!thread_tid)
        usleep(1000);
    int thfd = popen_(thread_tid, PIDFD_THREAD_);
    struct fh t;
    printf("thread handle = %d\n", handle_of(thfd, &t));
    unsigned long long tino;
    memcpy(&tino, t.h, 8);
    printf("thread handle differs from the process's = %s\n", yn(tino != hino));
    opened("obh(thread, O_EXCL)", obh(thfd, &t, O_EXCL), thread_tid);
    /* Without O_EXCL the file has no PIDFD_THREAD: a signal through it is for the thread's whole process
     * (linux-7.3.1/kernel/signal.c:4182, PIDTYPE_TGID), so it waits in the process's set, not the thread's. */
    long pt = obh(thfd, &t, 0);
    say("obh(thread, without O_EXCL)", pt < 0 ? -1 : 0);
    say("pidfd_send_signal(it, SIGUSR1)", syscall(SYS_pidfd_send_signal, pt, SIGUSR1, 0, 0));
    char task[64];
    snprintf(task, sizeof task, "/proc/self/task/%d/status", thread_tid);
    printf("  pending for the process = %s, for the thread = %s\n", yn(usr1_pending("/proc/self/status", "ShdPnd:")),
           yn(usr1_pending(task, "SigPnd:")));
    struct timespec now = {0, 0};
    printf("  taken by the leader = %s\n", yn(sigtimedwait(&usr1, 0, &now) == SIGUSR1));
    close(pt);
    if (write(stop[1], "x", 1) != 1)
        return 1;
    pthread_join(th, 0);
    int gone = -1;
    for (int i = 0; i < 200 && gone != 0; i++) {
        long r = obh(FD_PIDFS_ROOT_, &t, O_EXCL);
        if (r >= 0)
            close(r), usleep(5000);
        else
            gone = 0;
    }
    say("obh(ended thread)", obh(FD_PIDFS_ROOT_, &t, O_EXCL));
    close(thfd);
    sigprocmask(SIG_SETMASK, &old, 0);

    /* A pid namespace below: our handle is out of its sight, its own is not. */
    long c = rclone(CLONE_NEWUSER | CLONE_NEWPID | SIGCHLD);
    if (c == 0) {
        say("in a new pid namespace: obh(the parent's handle)", obh(FD_PIDFS_ROOT_, &h, 0));
        int self = popen_(getpid(), 0);
        struct fh s;
        printf("in a new pid namespace: own handle = %d\n", handle_of(self, &s));
        long o = obh(self, &s, 0);
        say("in a new pid namespace: obh(own)", o < 0 ? -1 : 0);
        printf("in a new pid namespace: the same pidfs inode = %s\n", yn(o >= 0 && ino_of(o) == ino_of(self)));
        _exit(0);
    }
    if (c < 0)
        printf("clone(NEWUSER|NEWPID) = -%s\n", nm(errno));
    else
        reap("pid namespace child", c);
    close(pfd);
    return 0;
}

static void nodename(const char *label) {
    struct utsname u;
    uname(&u);
    printf("%s: nodename %s\n", label, u.nodename);
}

static int same_ns(pid_t a, const char *kind) {
    char p[64], q[64];
    snprintf(p, sizeof p, "/proc/%d/ns/%s", (int)a, kind);
    snprintf(q, sizeof q, "/proc/self/ns/%s", kind);
    struct stat x, y;
    return stat(p, &x) == 0 && stat(q, &y) == 0 && x.st_ino == y.st_ino;
}

/* A target in its own user, uts, ipc, net and mount namespaces (named "pidfs-target") that waits on `go`; with
 * CLONE_NEWPID in `extra` the process that waits is its child, the first of a new pid namespace. Returns the pid of
 * the process that waits (its pid seen from here); the target itself in *outer. */
static pid_t target(int go[2], int ready[2], unsigned long extra, pid_t *outer) {
    pid_t t = fork();
    if (t == 0) {
        close(go[1]);
        if (unshare(CLONE_NEWUSER | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWNS | extra) || sethostname("pidfs-target", 12))
            _exit(2);
        pid_t in = 0;
        if (extra & CLONE_NEWPID) {
            in = fork();
            if (in == 0) {
                char c;
                if (read(go[0], &c, 1) < 0)
                    _exit(1);
                _exit(0);
            }
        }
        if (write(ready[1], &in, sizeof in) != sizeof in)
            _exit(1);
        if (in) {
            int st;
            waitpid(in, &st, 0);
            _exit(0);
        }
        char c;
        if (read(go[0], &c, 1) < 0)
            _exit(1);
        _exit(0);
    }
    *outer = t;
    pid_t in = -1;
    if (read(ready[0], &in, sizeof in) != sizeof in)
        return -1;
    return in ? in : t;
}

/* Runs the statements in a forked child (one thread, its own fs_struct) and reaps it. */
#define IN_CHILD(label, ...)                                                                                           \
    do {                                                                                                               \
        pid_t k_ = fork();                                                                                             \
        if (k_ == 0) {                                                                                                 \
            __VA_ARGS__;                                                                                               \
            _exit(0);                                                                                                  \
        }                                                                                                              \
        reap(label, k_);                                                                                               \
    } while (0)

static int same_ino(const char *a, const char *b) {
    struct stat x, y;
    return stat(a, &x) == 0 && stat(b, &y) == 0 && x.st_ino == y.st_ino;
}

static int setns_mode(void) {
    int go[2], ready[2];
    if (pipe(go) || pipe(ready))
        return 1;
    pid_t t0, t = target(go, ready, 0, &t0);
    int pfd = popen_(t, 0);
    int self = popen_(getpid(), 0);
    printf("target ready = %s\n", yn(t > 0 && pfd >= 0));
    R("setns(pidfd, 0)", setns(pfd, 0));
    R("setns(pidfd, CLONE_NEWUTS|CLONE_VM)", setns(pfd, CLONE_NEWUTS | CLONE_VM));
    R("setns(pidfd, CLONE_NEWUTS) unprivileged", setns(pfd, CLONE_NEWUTS));
    R("setns(pidfd, CLONE_NEWNET|CLONE_NEWUTS) unprivileged", setns(pfd, CLONE_NEWNET | CLONE_NEWUTS));
    R("setns(own pidfd, CLONE_NEWUSER)", setns(self, CLONE_NEWUSER));
    R("setns(own pidfd, CLONE_NEWUSER|CLONE_NEWUTS)", setns(self, CLONE_NEWUSER | CLONE_NEWUTS));
    int ev = eventfd(0, 0);
    R("setns(eventfd, CLONE_NEWUTS)", setns(ev, CLONE_NEWUTS));
    close(ev);
    R("setns(closed fd, CLONE_NEWUTS)", setns(999, CLONE_NEWUTS));
    struct utsname u;
    uname(&u);
    printf("parent: nodename not the target's = %s\n", yn(strcmp(u.nodename, "pidfs-target") != 0));

    /* All or nothing: the pid namespace refuses, so the user namespace is not entered either. */
    IN_CHILD("user+pid", {
        R("child: setns(pidfd, CLONE_NEWUSER|CLONE_NEWPID)", setns(pfd, CLONE_NEWUSER | CLONE_NEWPID));
        printf("child: still in the parent's user namespace = %s\n", yn(!same_ns(t, "user")));
        printf("child: uid %d\n", (int)getuid());
    });
    IN_CHILD("user+uts+net", {
        R("child: setns(pidfd, CLONE_NEWUSER|CLONE_NEWUTS|CLONE_NEWNET)", setns(pfd, CLONE_NEWUSER | CLONE_NEWUTS | CLONE_NEWNET));
        nodename("child");
        printf("child: user, uts, net as the target's = %s %s %s, mnt not = %s\n", yn(same_ns(t, "user")), yn(same_ns(t, "uts")),
               yn(same_ns(t, "net")), yn(!same_ns(t, "mnt")));
        printf("child: uid %d\n", (int)getuid());
    });
    IN_CHILD("all", {
        R("child: chdir(/proc)", chdir("/proc"));
        R("child: setns(pidfd, USER|MNT|UTS|IPC|NET)", setns(pfd, CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET));
        char cwd[64] = "";
        printf("child: cwd %s\n", getcwd(cwd, sizeof cwd) ? cwd : "(none)");
        printf("child: user, mnt, uts, ipc, net as the target's = %s %s %s %s %s\n", yn(same_ns(t, "user")),
               yn(same_ns(t, "mnt")), yn(same_ns(t, "uts")), yn(same_ns(t, "ipc")), yn(same_ns(t, "net")));
        /* Once in its user namespace, its namespaces are ours to enter one by one too. */
        R("child: setns(pidfd, CLONE_NEWUTS) again", setns(pfd, CLONE_NEWUTS));
        R("child: setns(pidfd, CLONE_NEWUSER) again", setns(pfd, CLONE_NEWUSER));
    });
    IN_CHILD("shared fs", {
        /* Entering a user namespace needs an fs_struct nobody else uses; a mount namespace among several works
         * on a copy (only the user part refuses here). */
        long sib = rclone(CLONE_FS | SIGCHLD);
        if (sib == 0) {
            usleep(300000);
            _exit(0);
        }
        R("child: setns(pidfd, CLONE_NEWUSER|CLONE_NEWUTS) with a CLONE_FS sibling", setns(pfd, CLONE_NEWUSER | CLONE_NEWUTS));
        reap("sibling", sib);
    });
    IN_CHILD("thread pidfd", {
        int tfd = popen_(t, PIDFD_THREAD_);
        R("child: setns(thread pidfd, CLONE_NEWUSER|CLONE_NEWUTS)", setns(tfd, CLONE_NEWUSER | CLONE_NEWUTS));
        nodename("child");
    });
    if (write(go[1], "gg", 2) != 2)
        return 1;
    siginfo_t si;
    waitid(P_PID, t, &si, WEXITED | WNOWAIT);
    IN_CHILD("zombie", { R("child: setns(zombie's pidfd, CLONE_NEWUTS)", setns(pfd, CLONE_NEWUTS)); });
    reap("target", t);
    R("setns(reaped's pidfd, CLONE_NEWUTS)", setns(pfd, CLONE_NEWUTS));
    close(pfd);

    /* A target that is not dumpable: ptrace_may_access(PTRACE_MODE_READ_REALCREDS) refuses (EPERM). */
    int g2[2], r2[2];
    if (pipe(g2) || pipe(r2))
        return 1;
    pid_t nd = fork();
    if (nd == 0) {
        char c;
        close(g2[1]);
        prctl(PR_SET_DUMPABLE, 0);
        if (write(r2[1], "r", 1) != 1 || read(g2[0], &c, 1) < 0)
            _exit(1);
        _exit(0);
    }
    char c;
    if (read(r2[0], &c, 1) != 1)
        return 1;
    int nfd = popen_(nd, 0);
    R("setns(non-dumpable's pidfd, CLONE_NEWUTS)", setns(nfd, CLONE_NEWUTS));
    R("setns(non-dumpable's pidfd, 0)", setns(nfd, 0));
    close(g2[1]);
    reap("non-dumpable", nd);

    /* A target in its own pid namespace: its children start there after setns(USER|PID). */
    int g3[2], r3[2];
    if (pipe(g3) || pipe(r3))
        return 1;
    pid_t p, inner = target(g3, r3, CLONE_NEWPID, &p);
    int ifd = popen_(inner, 0);
    printf("pid namespace target ready = %s\n", yn(inner > 0 && ifd >= 0));
    IN_CHILD("user+pid", {
        R("child: setns(pidfd, CLONE_NEWUSER|CLONE_NEWPID)", setns(ifd, CLONE_NEWUSER | CLONE_NEWPID));
        char a[64];
        snprintf(a, sizeof a, "/proc/%d/ns/pid", (int)inner);
        printf("child: pid_for_children is the target's pid namespace = %s, own pid namespace unchanged = %s\n",
               yn(same_ino(a, "/proc/self/ns/pid_for_children")), yn(!same_ns(inner, "pid")));
        pid_t gk = fork();
        if (gk == 0) {
            long v[4];
            printf("grandchild: NSpid levels %d\n", status_fields("NSpid:", v, 4));
            _exit(0);
        }
        reap("grandchild", gk);
    });
    if (write(g3[1], "gg", 2) != 2)
        return 1;
    reap("pid namespace target", p);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "handle"))
        return handle();
    if (argc > 1 && !strcmp(argv[1], "setns"))
        return setns_mode();
    printf("usage: abi-pidfs handle|setns\n");
    return 2;
}
