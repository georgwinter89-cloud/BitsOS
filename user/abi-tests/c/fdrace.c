/* PF-3: close and dup2/dup3 against threads that use the same fd numbers (linux-6.6/fs/file.c close_fd,
 * ksys_dup3/do_dup2, __fget_light). Cases (argv[1]):
 *   inflight  a read blocked in another thread keeps its file across close(fd) and dup2(x, fd)
 *   storm     threads dup2 two files onto one number, close it and use it: only success or EBADF, and a
 *             look-up only ever sees one of the two files
 *   rules     dup2/dup3 answers (EINVAL, EBADF, EBUSY-free numbers, close-on-exec flags)
 *   locks     dup2 over an fd drops the caller's POSIX locks on the replaced file's inode (do_dup2 ->
 *             filp_close), seen from a child with F_GETLK
 *   exec      fork from a thread while another thread dup2s and closes; the child execs and sees exactly the
 *             fds without close-on-exec
 *   range     close_range (plain and CLOSE_RANGE_CLOEXEC) over the number other threads dup2 onto and use:
 *             only success or EBADF, and afterwards the range is closed or close-on-exec
 *   unshare   close_range(CLOSE_RANGE_UNSHARE) from a second thread (CLONE_FILES): Linux gives that thread a
 *             private copy (__close_range -> unshare_fd), so the main thread keeps the fd
 * Only the main thread prints. */
#include "abi.h"
#include <fcntl.h>
#include <pthread.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>

#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef CLOSE_RANGE_UNSHARE
#define CLOSE_RANGE_UNSHARE 2
#define CLOSE_RANGE_CLOEXEC 4
#endif

static char self[4096];
static volatile int stop;

static void flush(void) {
    fflush(stdout);
    fflush(stderr);
}

/* ---- inflight ---------------------------------------------------------------------------------------- */

struct reader {
    int fd;
    volatile int started;
    long got;
    int err;
    char buf[8];
};

static void *read_one(void *arg) {
    struct reader *r = arg;
    r->started = 1;
    r->got = read(r->fd, r->buf, sizeof r->buf);
    r->err = r->got < 0 ? errno : 0;
    return 0;
}

static void wait_blocked(struct reader *r) {
    while (!r->started)
        sched_yield();
    /* Long enough for the thread to sleep in read(); a reader that lost would answer EBADF and show. */
    usleep(200 * 1000);
}

static void inflight_one(const char *label, int replace) {
    int p[2], q[2];
    if (pipe(p) || pipe(q)) {
        printf("%s: pipe -%s\n", label, ename(errno));
        return;
    }
    struct reader r = {.fd = p[0]};
    pthread_t t;
    pthread_create(&t, 0, read_one, &r);
    wait_blocked(&r);
    if (replace)
        printf("dup2 over the fd in use answers it: %s\n", dup2(q[0], p[0]) == p[0] ? "yes" : "no");
    else
        show("close of the fd in use", close(p[0]));
    /* The read goes on with the old pipe: the write to its other end wakes it. */
    show("write old pipe", write(p[1], "abc", 3));
    pthread_join(t, 0);
    if (r.got < 0)
        printf("%s: read = -%s\n", label, ename(r.err));
    else
        printf("%s: read = %ld \"%.*s\"\n", label, r.got, (int)r.got, r.buf);
    /* After the replace, the number reads the new pipe. */
    if (replace) {
        show("write new pipe", write(q[1], "xy", 2));
        char b[4];
        show("read number now", read(p[0], b, sizeof b));
    } else {
        char b[4];
        show("read closed number", read(p[0], b, sizeof b));
    }
    close(p[0]);
    close(p[1]);
    close(q[0]);
    close(q[1]);
}

static void inflight(void) {
    inflight_one("close", 0);
    inflight_one("dup2", 1);
}

/* ---- storm --------------------------------------------------------------------------------------------- */

#define NUM 50
static int file_a, file_b;
static dev_t dev_a, dev_b;
static ino_t ino_a, ino_b;
static long bad_errno, foreign, seen_a, seen_b;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;

static void *dupper(void *arg) {
    int src = *(int *)arg;
    while (!stop) {
        if (dup2(src, NUM) != NUM) {
            pthread_mutex_lock(&mu);
            bad_errno++;
            pthread_mutex_unlock(&mu);
        }
    }
    return 0;
}

static void *closer(void *arg) {
    (void)arg;
    while (!stop) {
        if (close(NUM) && errno != EBADF) {
            pthread_mutex_lock(&mu);
            bad_errno++;
            pthread_mutex_unlock(&mu);
        }
    }
    return 0;
}

static void *user(void *arg) {
    (void)arg;
    char c;
    while (!stop) {
        struct stat st;
        long a = 0, b = 0, other = 0, bad = 0;
        if (fstat(NUM, &st) == 0) {
            if (st.st_dev == dev_a && st.st_ino == ino_a)
                a++;
            else if (st.st_dev == dev_b && st.st_ino == ino_b)
                b++;
            else
                other++;
        } else if (errno != EBADF) {
            bad++;
        }
        long n = pread(NUM, &c, 1, 0);
        if (n < 0 && errno != EBADF)
            bad++;
        int fl = fcntl(NUM, F_GETFD);
        if (fl < 0 && errno != EBADF)
            bad++;
        pthread_mutex_lock(&mu);
        seen_a += a;
        seen_b += b;
        foreign += other;
        bad_errno += bad;
        pthread_mutex_unlock(&mu);
    }
    return 0;
}

static void storm(void) {
    struct stat st;
    file_a = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    file_b = open("/dev/null", O_RDONLY | O_CLOEXEC);
    if (file_a < 0 || file_b < 0) {
        printf("open: -%s\n", ename(errno));
        return;
    }
    fstat(file_a, &st);
    dev_a = st.st_dev, ino_a = st.st_ino;
    fstat(file_b, &st);
    dev_b = st.st_dev, ino_b = st.st_ino;
    pthread_t t[6];
    pthread_create(&t[0], 0, dupper, &file_a);
    pthread_create(&t[1], 0, dupper, &file_b);
    pthread_create(&t[2], 0, closer, 0);
    pthread_create(&t[3], 0, user, 0);
    pthread_create(&t[4], 0, user, 0);
    pthread_create(&t[5], 0, closer, 0);
    usleep(500 * 1000);
    stop = 1;
    for (int i = 0; i < 6; i++)
        pthread_join(t[i], 0);
    printf("storm: unexpected errors %s\n", bad_errno ? "yes" : "no");
    printf("storm: a file never put there was seen %s\n", foreign ? "yes" : "no");
    printf("storm: both files seen %s\n", seen_a && seen_b ? "yes" : "no");
    /* What is left at NUM is one of the two or nothing; close it and the number is free. */
    close(NUM);
    show("fcntl(NUM, F_GETFD) after", fcntl(NUM, F_GETFD));
    close(file_a);
    close(file_b);
}

/* ---- range --------------------------------------------------------------------------------------------- */

static long close_range3(unsigned lo, unsigned hi, unsigned flags) {
    return syscall(SYS_close_range, lo, hi, flags);
}

static void *ranger(void *arg) {
    (void)arg;
    unsigned n = 0;
    while (!stop) {
        unsigned flags = n++ % 3 == 2 ? CLOSE_RANGE_CLOEXEC : 0;
        if (close_range3(NUM - 1, n % 2 ? ~0U : NUM + 2, flags)) {
            pthread_mutex_lock(&mu);
            bad_errno++;
            pthread_mutex_unlock(&mu);
        }
    }
    return 0;
}

static void range(void) {
    struct stat st;
    file_a = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    file_b = open("/dev/null", O_RDONLY | O_CLOEXEC);
    if (file_a < 0 || file_b < 0) {
        printf("open: -%s\n", ename(errno));
        return;
    }
    fstat(file_a, &st);
    dev_a = st.st_dev, ino_a = st.st_ino;
    fstat(file_b, &st);
    dev_b = st.st_dev, ino_b = st.st_ino;
    pthread_t t[5];
    pthread_create(&t[0], 0, dupper, &file_a);
    pthread_create(&t[1], 0, dupper, &file_b);
    pthread_create(&t[2], 0, ranger, 0);
    pthread_create(&t[3], 0, user, 0);
    pthread_create(&t[4], 0, ranger, 0);
    usleep(500 * 1000);
    stop = 1;
    for (int i = 0; i < 5; i++)
        pthread_join(t[i], 0);
    printf("range: unexpected errors %s\n", bad_errno ? "yes" : "no");
    printf("range: a file never put there was seen %s\n", foreign ? "yes" : "no");
    printf("range: a file seen %s\n", seen_a || seen_b ? "yes" : "no");
    show("close_range(NUM-1, ~0, 0)", close_range3(NUM - 1, ~0U, 0));
    show("fcntl(NUM, F_GETFD) after", fcntl(NUM, F_GETFD));
    show("dup2(a, NUM)", dup2(file_a, NUM));
    show("dup2(b, NUM+1)", dup2(file_b, NUM + 1));
    show("close_range(NUM, NUM, CLOEXEC)", close_range3(NUM, NUM, CLOSE_RANGE_CLOEXEC));
    show("fcntl(NUM, F_GETFD)", fcntl(NUM, F_GETFD));
    show("fcntl(NUM+1, F_GETFD)", fcntl(NUM + 1, F_GETFD));
    show("close_range(NUM+1, NUM, 0)", close_range3(NUM + 1, NUM, 0));
    show("close_range(NUM, ~0, 8)", close_range3(NUM, ~0U, 8));
    show("close_range(NUM, ~0, 0)", close_range3(NUM, ~0U, 0));
    show("fcntl(NUM+1, F_GETFD) after", fcntl(NUM + 1, F_GETFD));
    close(file_a);
    close(file_b);
}

/* ---- unshare ------------------------------------------------------------------------------------------- */

static long unshare_rv, unshare_own;

static void *unsharer(void *arg) {
    unsigned flags = *(unsigned *)arg;
    unshare_rv = close_range3(NUM, NUM, CLOSE_RANGE_UNSHARE | flags);
    if (unshare_rv)
        unshare_rv = -errno;
    unshare_own = fcntl(NUM, F_GETFD);
    if (unshare_own < 0)
        unshare_own = -errno;
    return 0;
}

static void unshare_one(const char *label, unsigned flags) {
    pthread_t t;
    if (dup2(1, NUM) != NUM) {
        printf("%s: dup2 -%s\n", label, ename(errno));
        return;
    }
    pthread_create(&t, 0, unsharer, &flags);
    pthread_join(t, 0);
    printf("%s: close_range in the thread = %ld\n", label, unshare_rv);
    printf("%s: the thread's F_GETFD = %s%s\n", label, unshare_own < 0 ? "-" : "", unshare_own < 0 ? ename((int)-unshare_own) : (unshare_own ? "1" : "0"));
    show("the main thread's F_GETFD", fcntl(NUM, F_GETFD));
    close(NUM);
}

static void unshare_case(void) {
    unshare_one("unshare", 0);
    unshare_one("unshare|cloexec", CLOSE_RANGE_CLOEXEC);
}

/* ---- rules --------------------------------------------------------------------------------------------- */

static void rules(void) {
    int fd = open("/dev/null", O_RDONLY);
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    SYS("dup3(fd, fd, 0)", SYS_dup3, fd, fd, 0);
    SYS("dup3(fd, 70, 1)", SYS_dup3, fd, 70, 1);
    SYS("dup3(-1, 70, 0)", SYS_dup3, -1, 70, 0);
    SYS("dup3(fd, -1, 0)", SYS_dup3, fd, -1, 0);
    SYS("dup3(fd, rlimit, 0)", SYS_dup3, fd, (long)rl.rlim_cur, 0);
    SYS("dup3(99, 99, 0)", SYS_dup3, 99, 99, 0);
    SYS("dup2(fd, fd)", SYS_dup2, fd, fd);
    SYS("dup2(99, 99)", SYS_dup2, 99, 99);
    SYS("dup2(fd, rlimit)", SYS_dup2, fd, (long)rl.rlim_cur);
    SYS("dup2(fd, -1)", SYS_dup2, fd, -1);
    show("dup3(fd, 70, O_CLOEXEC)", dup3(fd, 70, O_CLOEXEC));
    show("F_GETFD 70", fcntl(70, F_GETFD));
    show("dup2(fd, 70) over it", dup2(fd, 70));
    show("F_GETFD 70 after dup2", fcntl(70, F_GETFD));
    show("close(70)", close(70));
    show("close(70) again", close(70));
    show("close(-1)", close(-1));
    /* A high fd beyond the table so far grows it; a hole below stays free. */
    show("dup2(fd, 1000)", dup2(fd, 1000));
    show("dup() lowest free", dup(fd) == fd + 1 ? 1 : 0);
    close(fd + 1);
    close(1000);
    close(fd);
}

/* ---- locks --------------------------------------------------------------------------------------------- */

static int child_sees_lock(const char *path) {
    pid_t c = fork();
    if (c == 0) {
        int fd = open(path, O_RDWR);
        struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
        if (fcntl(fd, F_GETLK, &fl))
            _exit(2);
        _exit(fl.l_type == F_UNLCK ? 0 : 1);
    }
    int st = 0;
    waitpid(c, &st, 0);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 3;
}

static void locks(void) {
    const char *path = "/tmp/abi-fdrace-lock";
    unlink(path);
    int a = open(path, O_RDWR | O_CREAT, 0600);
    int other = open("/dev/null", O_RDONLY);
    struct flock fl = {.l_type = F_WRLCK, .l_whence = SEEK_SET};
    show("F_SETLK", fcntl(a, F_SETLK, &fl));
    printf("locked, child sees a lock: %s\n", child_sees_lock(path) == 1 ? "yes" : "no");
    show("dup2(other, a)", dup2(other, a));
    printf("after dup2 over the locked fd, child sees a lock: %s\n", child_sees_lock(path) == 1 ? "yes" : "no");
    close(a);
    a = open(path, O_RDWR);
    int b = dup(a);
    show("F_SETLK again", fcntl(a, F_SETLK, &fl));
    show("dup3(other, b, 0)", dup3(other, b, 0));
    printf("after dup3 over a second fd of the file, child sees a lock: %s\n", child_sees_lock(path) == 1 ? "yes" : "no");
    close(a);
    close(b);
    close(other);
    unlink(path);
}

/* ---- exec ---------------------------------------------------------------------------------------------- */

static void *churn(void *arg) {
    int src = *(int *)arg;
    while (!stop) {
        dup2(src, 80);
        close(80);
    }
    return 0;
}

struct forker {
    pid_t pid;
};

static void *fork_exec(void *arg) {
    struct forker *f = arg;
    pid_t c = fork();
    if (c == 0) {
        char *av[] = {self, "probe", 0};
        char *ev[] = {"PATH=/bin", 0};
        execve(self, av, ev);
        _exit(127);
    }
    f->pid = c;
    return 0;
}

static void probe(void) {
    show("child F_GETFD 60 (close-on-exec)", fcntl(60, F_GETFD));
    show("child F_GETFD 61", fcntl(61, F_GETFD));
    show("child F_GETFD 62 (dup3 O_CLOEXEC)", fcntl(62, F_GETFD));
    show("child F_GETFD 63 (dup2 cleared it)", fcntl(63, F_GETFD));
}

static void exec_case(void) {
    int fd = open("/dev/null", O_RDONLY);
    dup3(fd, 60, O_CLOEXEC);
    dup2(fd, 61);
    dup3(fd, 62, O_CLOEXEC);
    dup3(fd, 63, O_CLOEXEC);
    dup2(fd, 63);
    pthread_t c, f;
    struct forker fk = {.pid = -1};
    stop = 0;
    pthread_create(&c, 0, churn, &fd);
    flush();
    pthread_create(&f, 0, fork_exec, &fk);
    pthread_join(f, 0);
    int st = 0;
    if (fk.pid < 0 || waitpid(fk.pid, &st, 0) != fk.pid)
        printf("child: -%s\n", ename(errno));
    else
        printf("child: exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    stop = 1;
    pthread_join(c, 0);
    for (int i = 60; i <= 63; i++)
        close(i);
    close(fd);
}

int main(int argc, char **argv) {
    if (readlink("/proc/self/exe", self, sizeof self - 1) < 0) {
        puts("no /proc/self/exe");
        return 1;
    }
    const char *c = argc > 1 ? argv[1] : "";
    if (!strcmp(c, "inflight"))
        inflight();
    else if (!strcmp(c, "storm"))
        storm();
    else if (!strcmp(c, "rules"))
        rules();
    else if (!strcmp(c, "locks"))
        locks();
    else if (!strcmp(c, "exec"))
        exec_case();
    else if (!strcmp(c, "range"))
        range();
    else if (!strcmp(c, "unshare"))
        unshare_case();
    else if (!strcmp(c, "probe"))
        probe();
    else {
        printf("unknown case %s\n", c);
        return 2;
    }
    flush();
    return 0;
}
