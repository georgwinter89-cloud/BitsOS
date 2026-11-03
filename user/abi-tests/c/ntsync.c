/* abi-ntsync (SC-8): /dev/ntsync (Linux 6.14+ drivers/misc/ntsync.c) as a program sees it, for the ABI diff
 * (CachyOS is the oracle) and the ktest sc.ntsync.abi (compared with expected/ntsync*.txt). Prints results,
 * errno names and yes/no relations only. Modes (argv[1]):
 *   (none)   the node, every ioctl's argument rules and errno order, object states, wait-any/wait-all with
 *            timeouts that have passed (no sleeping), the alert event, objects of another device
 *   threads  waits that sleep, ended by another thread (musl pthreads): a semaphore release, wait-all that
 *            completes only with its last object, an event pulse, an abandoned mutex, a signal (EINTR)
 * Without /dev/ntsync both modes print one SKIP line. The structs and numbers are written out here (the
 * sysroot's kernel headers may predate ntsync). */
#include "abi.h"
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>

struct sem_args { uint32_t count, max; };
struct mutex_args { uint32_t owner, count; };
struct event_args { uint32_t manual, signaled; };
struct wait_args { uint64_t timeout, objs; uint32_t count, index, flags, owner, alert, pad; };

#define IOC(dir, nr, size) ((unsigned long)(((dir) << 30) | ((size) << 16) | ('N' << 8) | (nr)))
#define CREATE_SEM IOC(1u, 0x80, 8)
#define SEM_RELEASE IOC(3u, 0x81, 4)
#define WAIT_ANY IOC(3u, 0x82, 40)
#define WAIT_ALL IOC(3u, 0x83, 40)
#define CREATE_MUTEX IOC(1u, 0x84, 8)
#define MUTEX_UNLOCK IOC(3u, 0x85, 8)
#define MUTEX_KILL IOC(1u, 0x86, 4)
#define CREATE_EVENT IOC(1u, 0x87, 8)
#define EVENT_SET IOC(2u, 0x88, 4)
#define EVENT_RESET IOC(2u, 0x89, 4)
#define EVENT_PULSE IOC(2u, 0x8a, 4)
#define SEM_READ IOC(2u, 0x8b, 8)
#define MUTEX_READ IOC(2u, 0x8c, 8)
#define EVENT_READ IOC(2u, 0x8d, 8)
#define WAIT_REALTIME 1u
#define NEVER UINT64_MAX

static int dev;

static const char *en(int e) {
    switch (e) {
    case EOVERFLOW: return "EOVERFLOW";
    case ETIMEDOUT: return "ETIMEDOUT";
    case EOWNERDEAD: return "EOWNERDEAD";
    }
    return ename(e);
}

/* "label = <result>" or "label = -ERRNO" of one ioctl. */
static long io(const char *label, int fd, unsigned long cmd, void *arg) {
    long r = syscall(SYS_ioctl, fd, cmd, arg);
    if (r == -1)
        printf("%s = -%s\n", label, en(errno));
    else
        printf("%s = %ld\n", label, r);
    return r;
}

static int mk(unsigned long cmd, uint32_t a, uint32_t b) {
    uint32_t args[2] = {a, b};
    return (int)syscall(SYS_ioctl, dev, cmd, args);
}

static void sem_show(const char *l, int fd) {
    struct sem_args s = {99, 99};
    long r = syscall(SYS_ioctl, fd, SEM_READ, &s);
    printf("%s: ret %s count %u max %u\n", l, r ? en(errno) : "0", s.count, s.max);
}
static void mutex_show(const char *l, int fd) {
    struct mutex_args m = {99, 99};
    long r = syscall(SYS_ioctl, fd, MUTEX_READ, &m);
    printf("%s: ret %s owner %u count %u\n", l, r ? en(errno) : "0", m.owner, m.count);
}
static void event_show(const char *l, int fd) {
    struct event_args e = {99, 99};
    long r = syscall(SYS_ioctl, fd, EVENT_READ, &e);
    printf("%s: ret %s manual %u signaled %u\n", l, r ? en(errno) : "0", e.manual, e.signaled);
}

/* One wait on `on` (a device fd); prints the result and, on success or EOWNERDEAD, the index. */
static long waitx(const char *l, int on, unsigned long cmd, int *fds, uint32_t n, uint32_t alert, uint32_t owner, uint64_t timeout, uint32_t flags) {
    struct wait_args w = {timeout, (uint64_t)(uintptr_t)fds, n, 777, flags, owner, alert, 0};
    long r = syscall(SYS_ioctl, on, cmd, &w);
    if (r == 0 || errno == EOWNERDEAD)
        printf("%s = %s index %u\n", l, r == 0 ? "0" : "-EOWNERDEAD", w.index);
    else
        printf("%s = -%s\n", l, en(errno));
    return r;
}

static uint64_t realtime_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_REALTIME, &t);
    return (uint64_t)t.tv_sec * 1000000000ull + (uint64_t)t.tv_nsec;
}

static void node(void) {
    struct stat st;
    stat("/dev/ntsync", &st);
    yes("node is a character device", S_ISCHR(st.st_mode));
    printf("node mode %o major %u\n", st.st_mode & 07777, major(st.st_rdev));
    char b[8];
    show("read device", read(dev, b, sizeof b));
    show("write device", write(dev, b, 4));
    io("unknown command on device", dev, IOC(3u, 0x7f, 8), b);
    io("SEM_READ on device", dev, SEM_READ, b);
    io("CREATE_SEM bad pointer", dev, CREATE_SEM, KERNEL_ADDR);
}

static void objfd(int fd) {
    printf("object fd >= 0: %s\n", fd >= 0 ? "yes" : "no");
    printf("F_GETFD %d F_GETFL %o\n", fcntl(fd, F_GETFD), fcntl(fd, F_GETFL));
    char p[64], l[64] = {0};
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    readlink(p, l, sizeof l - 1);
    printf("link %s\n", l);
    struct stat st;
    fstat(fd, &st);
    printf("fstat type %o perm %o\n", st.st_mode & S_IFMT, st.st_mode & 07777);
}

static void sems(void) {
    uint32_t v;
    show("create sem count>max", mk(CREATE_SEM, 3, 2));
    int s = mk(CREATE_SEM, 1, 2);
    objfd(s);
    sem_show("sem new", s);
    v = 1;
    io("release 1", s, SEM_RELEASE, &v);
    printf("release 1 previous %u\n", v);
    v = 1;
    io("release past max", s, SEM_RELEASE, &v);
    printf("release past max leaves %u\n", v);
    v = 0xffffffffu;
    io("release overflow", s, SEM_RELEASE, &v);
    io("release bad pointer", s, SEM_RELEASE, KERNEL_ADDR);
    io("SEM_READ bad pointer", s, SEM_READ, KERNEL_ADDR);
    io("EVENT_SET on sem", s, EVENT_SET, &v);
    io("MUTEX_READ on sem", s, MUTEX_READ, &v);
    io("CREATE_SEM on object", s, CREATE_SEM, &v);
    io("WAIT_ANY on object", s, WAIT_ANY, &v);
    io("unknown command on object", s, IOC(2u, 0x8e, 8), &v);
    sem_show("sem now", s);
    waitx("wait sem 1", dev, WAIT_ANY, &s, 1, 0, 1, 0, 0);
    waitx("wait sem 2", dev, WAIT_ANY, &s, 1, 0, 1, 0, 0);
    waitx("wait sem empty", dev, WAIT_ANY, &s, 1, 0, 1, 0, 0);
    sem_show("sem after waits", s);
    close(s);
}

static void mutexes(void) {
    struct mutex_args a;
    uint32_t o;
    show("create mutex count without owner", mk(CREATE_MUTEX, 0, 1));
    show("create mutex owner without count", mk(CREATE_MUTEX, 5, 0));
    int m = mk(CREATE_MUTEX, 5, 2);
    objfd(m);
    mutex_show("mutex new", m);
    a = (struct mutex_args){0, 0};
    io("unlock owner 0", m, MUTEX_UNLOCK, &a);
    a = (struct mutex_args){6, 0};
    io("unlock by other", m, MUTEX_UNLOCK, &a);
    a = (struct mutex_args){5, 77};
    io("unlock 1", m, MUTEX_UNLOCK, &a);
    printf("unlock 1 previous %u\n", a.count);
    a = (struct mutex_args){5, 77};
    io("unlock 2", m, MUTEX_UNLOCK, &a);
    printf("unlock 2 previous %u\n", a.count);
    mutex_show("mutex free", m);
    a = (struct mutex_args){5, 77};
    io("unlock free", m, MUTEX_UNLOCK, &a);
    io("unlock bad pointer", m, MUTEX_UNLOCK, KERNEL_ADDR);
    waitx("wait mutex owner 7", dev, WAIT_ANY, &m, 1, 0, 7, 0, 0);
    waitx("wait mutex owner 7 again", dev, WAIT_ANY, &m, 1, 0, 7, 0, 0);
    waitx("wait mutex owner 8", dev, WAIT_ANY, &m, 1, 0, 8, 0, 0);
    mutex_show("mutex owned twice", m);
    o = 0;
    io("kill owner 0", m, MUTEX_KILL, &o);
    o = 8;
    io("kill by other", m, MUTEX_KILL, &o);
    io("kill bad pointer", m, MUTEX_KILL, KERNEL_ADDR);
    o = 7;
    io("kill owner", m, MUTEX_KILL, &o);
    mutex_show("mutex abandoned", m);
    io("kill abandoned", m, MUTEX_KILL, &o);
    waitx("wait abandoned owner 8", dev, WAIT_ANY, &m, 1, 0, 8, 0, 0);
    mutex_show("mutex taken after abandon", m);
    io("SEM_RELEASE on mutex", m, SEM_RELEASE, &o);
    o = 8;
    io("EVENT_RESET on mutex", m, EVENT_RESET, &o);
    close(m);
    /* Owner 0 is no owner: a wait with it is EINVAL (Linux 7.3; 6.14 to 7.2 took the mutex for owner 0). */
    m = mk(CREATE_MUTEX, 0, 0);
    waitx("wait mutex owner 0", dev, WAIT_ANY, &m, 1, 0, 0, 0, 0);
    mutex_show("mutex after owner 0", m);
    close(m);
}

static void events(void) {
    uint32_t v;
    int e = mk(CREATE_EVENT, 0, 1);
    int me = mk(CREATE_EVENT, 7, 0);
    objfd(e);
    event_show("auto new", e);
    event_show("manual new", me);
    waitx("wait auto", dev, WAIT_ANY, &e, 1, 0, 1, 0, 0);
    event_show("auto after wait", e);
    waitx("wait auto again", dev, WAIT_ANY, &e, 1, 0, 1, 0, 0);
    v = 9;
    io("set", e, EVENT_SET, &v);
    printf("set previous %u\n", v);
    io("set again", e, EVENT_SET, &v);
    printf("set again previous %u\n", v);
    io("reset", e, EVENT_RESET, &v);
    printf("reset previous %u\n", v);
    io("reset again", e, EVENT_RESET, &v);
    printf("reset again previous %u\n", v);
    io("pulse", me, EVENT_PULSE, &v);
    printf("pulse previous %u\n", v);
    event_show("manual after pulse", me);
    io("set bad pointer", me, EVENT_SET, KERNEL_ADDR);
    event_show("manual after set with bad pointer", me);
    waitx("wait manual 1", dev, WAIT_ANY, &me, 1, 0, 1, 0, 0);
    waitx("wait manual 2", dev, WAIT_ANY, &me, 1, 0, 1, 0, 0);
    io("reset bad pointer", me, EVENT_RESET, KERNEL_ADDR);
    event_show("manual after reset with bad pointer", me);
    io("EVENT_READ bad pointer", me, EVENT_READ, KERNEL_ADDR);
    io("SEM_RELEASE on event", e, SEM_RELEASE, &v);
    io("MUTEX_KILL on event", e, MUTEX_KILL, &v);
    close(e);
    close(me);
}

static void waits(void) {
    struct wait_args w;
    int s = mk(CREATE_SEM, 0, 10);
    int e = mk(CREATE_EVENT, 0, 0);
    int al = mk(CREATE_EVENT, 1, 0);
    int m = mk(CREATE_MUTEX, 0, 0);
    int fds[70];
    for (int i = 0; i < 70; i++)
        fds[i] = s;
    w = (struct wait_args){0, (uint64_t)(uintptr_t)fds, 1, 0, 0, 1, 0, 1};
    io("wait pad", dev, WAIT_ANY, &w);
    w.pad = 0;
    w.flags = 2;
    io("wait unknown flag", dev, WAIT_ANY, &w);
    w.flags = 0;
    w.count = 65;
    io("wait 65 objects", dev, WAIT_ANY, &w);
    w.count = 64;
    io("wait 64 objects", dev, WAIT_ANY, &w);
    w.objs = (uint64_t)(uintptr_t)KERNEL_ADDR;
    io("wait objs bad pointer", dev, WAIT_ANY, &w);
    w.count = 0;
    io("wait 0 objects bad pointer", dev, WAIT_ANY, &w);
    w.count = 1;
    w.owner = 0;
    io("wait owner 0 bad pointer", dev, WAIT_ANY, &w);
    io("wait_all owner 0 bad pointer", dev, WAIT_ALL, &w);
    w.owner = 1;
    w.count = 65;
    io("wait 65 objects bad pointer", dev, WAIT_ANY, &w);
    w.pad = 1;
    io("wait pad bad pointer", dev, WAIT_ANY, &w);
    io("wait args bad pointer", dev, WAIT_ANY, KERNEL_ADDR);
    io("wait_all args bad pointer", dev, WAIT_ALL, KERNEL_ADDR);
    w = (struct wait_args){0, (uint64_t)(uintptr_t)fds, 64, 0, 0, 1, 0, 0};
    io("wait_all 64 same objects", dev, WAIT_ALL, &w);
    int bad[2] = {999, 1};
    waitx("wait closed fd", dev, WAIT_ANY, bad, 1, 0, 1, 0, 0);
    waitx("wait stdout", dev, WAIT_ANY, bad + 1, 1, 0, 1, 0, 0);
    waitx("wait device fd", dev, WAIT_ANY, &dev, 1, 0, 1, 0, 0);
    waitx("wait alert closed fd", dev, WAIT_ANY, &s, 1, 999, 1, 0, 0);
    waitx("wait nothing", dev, WAIT_ANY, fds, 0, 0, 1, 0, 0);
    waitx("wait_all nothing", dev, WAIT_ALL, fds, 0, 0, 1, 0, 0);
    int d2 = open("/dev/ntsync", O_RDWR);
    waitx("wait on other device", d2, WAIT_ANY, &s, 1, 0, 1, 0, 0);
    close(d2);
    /* any: duplicates allowed, the first signaled index wins, the alert after all objects */
    uint32_t v = 1;
    syscall(SYS_ioctl, s, SEM_RELEASE, &v);
    int set1[3] = {e, s, s};
    waitx("wait_any duplicates", dev, WAIT_ANY, set1, 3, 0, 1, 0, 0);
    waitx("wait_all duplicates", dev, WAIT_ALL, set1 + 1, 2, 0, 1, 0, 0);
    waitx("wait_all alert duplicate", dev, WAIT_ALL, &s, 1, s, 1, 0, 0);
    v = 0;
    syscall(SYS_ioctl, al, EVENT_SET, &v);
    waitx("wait_any alert", dev, WAIT_ANY, &e, 1, al, 1, 0, 0);
    v = 1;
    syscall(SYS_ioctl, s, SEM_RELEASE, &v);
    waitx("wait_any object before alert", dev, WAIT_ANY, &s, 1, al, 1, 0, 0);
    waitx("wait_any only alert", dev, WAIT_ANY, fds, 0, al, 1, 0, 0);
    /* all: everything or nothing */
    v = 2;
    syscall(SYS_ioctl, s, SEM_RELEASE, &v);
    int set2[3] = {s, m, e};
    waitx("wait_all one missing", dev, WAIT_ALL, set2, 3, 0, 4, 0, 0);
    sem_show("sem after failed wait_all", s);
    mutex_show("mutex after failed wait_all", m);
    waitx("wait_all one missing, alert", dev, WAIT_ALL, set2, 3, al, 4, 0, 0);
    sem_show("sem after alert wait_all", s);
    syscall(SYS_ioctl, e, EVENT_SET, &v);
    waitx("wait_all", dev, WAIT_ALL, set2, 3, al, 4, 0, 0);
    sem_show("sem after wait_all", s);
    mutex_show("mutex after wait_all", m);
    event_show("auto after wait_all", e);
    syscall(SYS_ioctl, e, EVENT_SET, &v);
    waitx("wait_all mutex of other owner", dev, WAIT_ALL, set2, 3, 0, 5, 0, 0);
    waitx("wait_all mutex own again", dev, WAIT_ALL, set2, 3, 0, 4, 0, 0);
    mutex_show("mutex after second wait_all", m);
    /* timeouts that have passed: monotonic, realtime, 1 ns */
    waitx("wait timeout 1", dev, WAIT_ANY, &e, 1, 0, 1, 1, 0);
    waitx("wait realtime past", dev, WAIT_ANY, &e, 1, 0, 1, realtime_ns() - 1000000000ull, WAIT_REALTIME);
    waitx("wait_all realtime past", dev, WAIT_ALL, &e, 1, 0, 1, realtime_ns() - 1000000000ull, WAIT_REALTIME);
    syscall(SYS_ioctl, e, EVENT_SET, &v);
    waitx("wait signaled realtime past", dev, WAIT_ANY, &e, 1, 0, 1, 1, WAIT_REALTIME);
    /* the index is written back, nothing else */
    w = (struct wait_args){0, (uint64_t)(uintptr_t)&al, 1, 555, 0, 3, 0, 0};
    long r = syscall(SYS_ioctl, dev, WAIT_ANY, &w);
    printf("written back: ret %ld index %u count %u owner %u timeout %llu\n", r, w.index, w.count, w.owner, (unsigned long long)w.timeout);
    close(s);
    close(e);
    close(al);
    close(m);
}

/* ---------------------------------------------------------------------------------------------- threads */

struct job {
    unsigned long cmd;
    int fds[4];
    uint32_t n, alert, owner;
    long ret;
    int err;
    uint32_t index;
    atomic_int done;
};

static void *waiter(void *p) {
    struct job *j = p;
    struct wait_args w = {NEVER, (uint64_t)(uintptr_t)j->fds, j->n, 777, 0, j->owner, j->alert, 0};
    j->ret = syscall(SYS_ioctl, dev, j->cmd, &w);
    j->err = j->ret ? errno : 0;
    j->index = w.index;
    atomic_store(&j->done, 1);
    return 0;
}

static void start(pthread_t *t, struct job *j) {
    atomic_store(&j->done, 0);
    pthread_create(t, 0, waiter, j);
}

static void report(const char *l, pthread_t t, struct job *j) {
    pthread_join(t, 0);
    if (j->ret == 0 || j->err == EOWNERDEAD)
        printf("%s = %s index %u\n", l, j->ret == 0 ? "0" : "-EOWNERDEAD", j->index);
    else
        printf("%s = -%s\n", l, en(j->err));
}

static void nap(void) {
    struct timespec t = {0, 2000000};
    nanosleep(&t, 0);
}

static void on_usr1(int s) {
    (void)s;
}

static void threads(void) {
    pthread_t t;
    uint32_t v;
    int s = mk(CREATE_SEM, 0, 10);
    int e = mk(CREATE_EVENT, 0, 0);
    int me = mk(CREATE_EVENT, 1, 0);
    int m = mk(CREATE_MUTEX, 5, 1);
    struct job j = {.cmd = WAIT_ANY, .fds = {e, s}, .n = 2, .owner = 1};
    start(&t, &j);
    v = 1;
    syscall(SYS_ioctl, s, SEM_RELEASE, &v);
    report("thread wait_any woken by release", t, &j);
    sem_show("sem after thread took it", s);

    j = (struct job){.cmd = WAIT_ALL, .fds = {s, e}, .n = 2, .owner = 1};
    start(&t, &j);
    v = 1;
    syscall(SYS_ioctl, s, SEM_RELEASE, &v);
    for (int i = 0; i < 20; i++)
        nap();
    printf("wait_all still waiting with one object: %s\n", atomic_load(&j.done) ? "no" : "yes");
    sem_show("sem not taken by a waiting wait_all", s);
    syscall(SYS_ioctl, e, EVENT_SET, &v);
    report("thread wait_all woken by the last object", t, &j);
    sem_show("sem after wait_all", s);
    event_show("auto after wait_all", e);

    j = (struct job){.cmd = WAIT_ANY, .fds = {me}, .n = 1, .owner = 1};
    start(&t, &j);
    while (!atomic_load(&j.done)) {
        syscall(SYS_ioctl, me, EVENT_PULSE, &v);
        nap();
    }
    report("thread woken by pulse", t, &j);
    event_show("manual after pulse", me);

    j = (struct job){.cmd = WAIT_ANY, .fds = {s}, .n = 1, .alert = me, .owner = 1};
    start(&t, &j);
    syscall(SYS_ioctl, me, EVENT_SET, &v);
    report("thread woken by alert", t, &j);
    syscall(SYS_ioctl, me, EVENT_RESET, &v);

    j = (struct job){.cmd = WAIT_ANY, .fds = {m}, .n = 1, .owner = 6};
    start(&t, &j);
    v = 5;
    syscall(SYS_ioctl, m, MUTEX_KILL, &v);
    report("thread takes abandoned mutex", t, &j);
    mutex_show("mutex after thread", m);

    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, 0);
    j = (struct job){.cmd = WAIT_ANY, .fds = {s}, .n = 1, .owner = 1};
    start(&t, &j);
    while (!atomic_load(&j.done)) {
        pthread_kill(t, SIGUSR1);
        nap();
    }
    report("thread wait ended by a signal", t, &j);

    j = (struct job){.cmd = WAIT_ALL, .fds = {s, e}, .n = 2, .owner = 1};
    start(&t, &j);
    while (!atomic_load(&j.done)) {
        pthread_kill(t, SIGUSR1);
        nap();
    }
    report("thread wait_all ended by a signal", t, &j);
    sem_show("sem after interrupted waits", s);
    close(s);
    close(e);
    close(me);
    close(m);
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    dev = open("/dev/ntsync", O_RDWR | O_CLOEXEC);
    if (dev < 0) {
        printf("SKIP: no /dev/ntsync (%s)\n", ename(errno));
        return 0;
    }
    if (argc > 1 && !strcmp(argv[1], "threads")) {
        threads();
        return 0;
    }
    node();
    sems();
    mutexes();
    events();
    waits();
    return 0;
}
