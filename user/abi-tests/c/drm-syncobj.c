/* DR-3: DRM sync objects and sync_file as Mesa's vk_drm_syncobj and a compositor meet them, on the first amdgpu
 * render node (CachyOS: the real GPU, V2 under QEMU: the fake). Only syncobj and sync_file calls: nothing here
 * submits GPU work. Fences come from the kernel's own stub (CREATE_SIGNALED, SIGNAL, TIMELINE_SIGNAL); a second
 * thread signals while the first waits (WAIT_FOR_SUBMIT, EVENTFD). Prints results and errno names only. */
#include "abi.h"
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/stat.h>

struct drm_version {
    int major, minor, patch, pad;
    unsigned long name_len;
    char *name;
    unsigned long date_len;
    char *date;
    unsigned long desc_len;
    char *desc;
};
struct so_create { uint32_t handle, flags; };
struct so_destroy { uint32_t handle, pad; };
struct so_handle { uint32_t handle, flags; int32_t fd; uint32_t pad; uint64_t point; };
struct so_transfer { uint32_t src, dst; uint64_t src_point, dst_point; uint32_t flags, pad; };
struct so_wait { uint64_t handles; int64_t timeout; uint32_t count, flags, first, pad; uint64_t deadline; };
struct so_twait { uint64_t handles, points; int64_t timeout; uint32_t count, flags, first, pad; uint64_t deadline; };
struct so_eventfd { uint32_t handle, flags; uint64_t point; int32_t fd; uint32_t pad; };
struct so_array { uint64_t handles; uint32_t count, pad; };
struct so_tarray { uint64_t handles, points; uint32_t count, flags; };
struct sync_merge { char name[32]; int32_t fd2, fence; uint32_t flags, pad; };
struct sync_fence_info { char obj_name[32], driver_name[32]; int32_t status; uint32_t flags; uint64_t timestamp_ns; };
struct sync_file_info { char name[32]; int32_t status; uint32_t flags, num_fences, pad; uint64_t info; };

#define IOC(dir, ty, nr, size) ((unsigned long)(dir) << 30 | (unsigned long)(size) << 16 | (unsigned long)(ty) << 8 | (nr))
#define D(nr, s) IOC(3u, 0x64u, nr, sizeof(struct s))
#define VERSION IOC(3u, 0x64u, 0x00, 64)
#define CREATE D(0xbf, so_create)
#define DESTROY D(0xc0, so_destroy)
#define TO_FD D(0xc1, so_handle)
#define FROM_FD D(0xc2, so_handle)
#define WAIT D(0xc3, so_wait)
#define RESET D(0xc4, so_array)
#define SIGNAL D(0xc5, so_array)
#define TWAIT D(0xca, so_twait)
#define QUERY D(0xcb, so_tarray)
#define TRANSFER D(0xcc, so_transfer)
#define TSIGNAL D(0xcd, so_tarray)
#define EVENTFD D(0xcf, so_eventfd)
#define SYNC_MERGE IOC(3u, 0x3eu, 3, sizeof(struct sync_merge))
#define SYNC_INFO IOC(3u, 0x3eu, 4, sizeof(struct sync_file_info))

#define WAIT_ALL 1u
#define FOR_SUBMIT 2u
#define AVAILABLE 4u
#define EXPORT_SYNC_FILE 1u
#define IMPORT_SYNC_FILE 1u
#define TIMELINE 2u

static int fd = -1;

static const char *en(int e) {
    return e == ETIME ? "ETIME" : ename(e);
}

static long io(int f, unsigned long cmd, void *arg) {
    return ioctl(f, cmd, arg) == -1 ? -errno : 0;
}

static void res(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, en((int)-r));
    else
        printf("%s = %ld\n", label, r);
}

static int pick_node(void) {
    DIR *d = opendir("/dev/dri");
    if (!d)
        return 0;
    char best[64] = "";
    struct dirent *e;
    while ((e = readdir(d))) {
        if (strncmp(e->d_name, "renderD", 7))
            continue;
        char p[64];
        snprintf(p, sizeof p, "/dev/dri/%s", e->d_name);
        int f = open(p, O_RDWR | O_CLOEXEC);
        if (f < 0)
            continue;
        char name[16] = "";
        struct drm_version v = {.name_len = sizeof name - 1, .name = name};
        if (ioctl(f, VERSION, &v) == 0 && !strcmp(name, "amdgpu") && (!best[0] || strcmp(p, best) < 0))
            strcpy(best, p);
        close(f);
    }
    closedir(d);
    if (!best[0])
        return 0;
    fd = open(best, O_RDWR | O_CLOEXEC);
    return fd >= 0;
}

static uint32_t create(uint32_t flags) {
    struct so_create c = {.flags = flags};
    return io(fd, CREATE, &c) ? 0 : c.handle;
}
static void destroy(uint32_t h) {
    struct so_destroy d = {.handle = h};
    io(fd, DESTROY, &d);
}
static long wait1(uint32_t h, uint32_t flags, int64_t timeout, uint32_t *first) {
    struct so_wait w = {.handles = (uintptr_t)&h, .timeout = timeout, .count = 1, .flags = flags, .first = 77};
    long r = io(fd, WAIT, &w);
    if (first)
        *first = w.first;
    return r;
}
static long twait1(uint32_t h, uint64_t point, uint32_t flags, int64_t timeout) {
    struct so_twait w = {.handles = (uintptr_t)&h, .points = (uintptr_t)&point, .timeout = timeout, .count = 1, .flags = flags};
    return io(fd, TWAIT, &w);
}
static long signal1(uint32_t h) {
    struct so_array a = {.handles = (uintptr_t)&h, .count = 1};
    return io(fd, SIGNAL, &a);
}
static long tsignal1(uint32_t h, uint64_t point) {
    struct so_tarray a = {.handles = (uintptr_t)&h, .points = (uintptr_t)&point, .count = 1};
    return io(fd, TSIGNAL, &a);
}
static long query1(uint32_t h, uint32_t flags, uint64_t *point) {
    struct so_tarray a = {.handles = (uintptr_t)&h, .points = (uintptr_t)point, .count = 1, .flags = flags};
    return io(fd, QUERY, &a);
}
static long to_fd(uint32_t h, uint32_t flags, uint64_t point, int *out) {
    struct so_handle a = {.handle = h, .flags = flags, .fd = -1, .point = point};
    long r = io(fd, TO_FD, &a);
    *out = a.fd;
    return r;
}
static long from_fd(int f, uint32_t flags, uint64_t point, uint32_t *h) {
    struct so_handle a = {.handle = *h, .flags = flags, .fd = f, .point = point};
    long r = io(fd, FROM_FD, &a);
    *h = a.handle;
    return r;
}
static uint64_t now(void) {
    return ns_of(CLOCK_MONOTONIC);
}

static void create_checks(void) {
    uint32_t a = create(0), b = create(0);
    yes("create: handles from 1, distinct", a >= 1 && b >= 1 && a != b);
    struct so_create c = {.flags = 2};
    res("create flags=2", io(fd, CREATE, &c));
    struct so_destroy d = {.handle = a, .pad = 1};
    res("destroy pad=1", io(fd, DESTROY, &d));
    d.pad = 0;
    res("destroy", io(fd, DESTROY, &d));
    res("destroy again", io(fd, DESTROY, &d));
    uint32_t c2 = create(0);
    yes("create: a freed handle comes back", c2 == a);
    destroy(b);
    destroy(c2);
}

static void wait_checks(void) {
    uint32_t sig = create(1), none = create(0), first = 0;
    res("wait signaled, timeout 0", wait1(sig, 0, 0, &first));
    printf("wait signaled: first_signaled = %u\n", first);
    res("wait no fence", wait1(none, 0, 0, &first));
    printf("wait no fence: first_signaled = %u\n", first);
    res("wait no fence, for submit, timeout 0", wait1(none, FOR_SUBMIT, 0, &first));
    printf("wait timed out: first_signaled = %u\n", first);
    res("wait no fence, for submit, past", wait1(none, FOR_SUBMIT, 1, NULL));
    uint64_t t0 = now();
    long r = wait1(none, FOR_SUBMIT, (int64_t)(t0 + 20000000), NULL);
    res("wait no fence, for submit, 20 ms", r);
    yes("wait 20 ms: not back before the deadline", now() >= t0 + 20000000);
    res("wait flags=available", wait1(sig, AVAILABLE, 0, NULL));
    res("wait flags=0x10", wait1(sig, 0x10, 0, NULL));
    res("wait unknown handle", wait1(9999, 0, 0, NULL));
    struct so_wait w = {.handles = (uintptr_t)&sig, .count = 0};
    res("wait count 0", io(fd, WAIT, &w));
    w.count = 1;
    w.handles = (uintptr_t)KERNEL_ADDR;
    res("wait handles kernel address", io(fd, WAIT, &w));
    uint32_t two[2] = {none, sig};
    struct so_wait w2 = {.handles = (uintptr_t)two, .count = 2, .flags = FOR_SUBMIT, .first = 77};
    res("wait any of [none, signaled]", io(fd, WAIT, &w2));
    printf("wait any: first_signaled = %u\n", w2.first);
    w2.flags = FOR_SUBMIT | WAIT_ALL;
    w2.first = 77;
    res("wait all of [none, signaled], timeout 0", io(fd, WAIT, &w2));
    printf("wait all timed out: first_signaled = %u\n", w2.first);
    w2.flags = WAIT_ALL;
    res("wait all of [none, signaled], no submit flag", io(fd, WAIT, &w2));
    uint32_t sig2 = create(1);
    uint32_t both[2] = {sig, sig2};
    struct so_wait w3 = {.handles = (uintptr_t)both, .count = 2, .flags = WAIT_ALL, .first = 77};
    res("wait all of two signaled", io(fd, WAIT, &w3));
    printf("wait all: first_signaled = %u\n", w3.first);
    struct so_array a = {.handles = (uintptr_t)&sig, .count = 1};
    res("reset", io(fd, RESET, &a));
    res("wait after reset", wait1(sig, 0, 0, NULL));
    a.pad = 1;
    res("reset pad=1", io(fd, RESET, &a));
    a.pad = 0;
    a.count = 0;
    res("reset count 0", io(fd, RESET, &a));
    res("signal count 0", io(fd, SIGNAL, &a));
    a.count = 1;
    res("signal", io(fd, SIGNAL, &a));
    res("wait after signal", wait1(sig, 0, 0, NULL));
    uint32_t bad = 9999;
    a.handles = (uintptr_t)&bad;
    res("signal unknown handle", io(fd, SIGNAL, &a));
    a.handles = (uintptr_t)KERNEL_ADDR;
    res("signal handles kernel address", io(fd, SIGNAL, &a));
    destroy(sig);
    destroy(sig2);
    destroy(none);
}

static long eventfd_count(int e);

struct later { uint32_t h; uint64_t point; int efd; };

static void *signal_later(void *p) {
    struct later *l = p;
    usleep(20000);
    if (l->point)
        tsignal1(l->h, l->point);
    else
        signal1(l->h);
    return NULL;
}

static void blocking_checks(void) {
    struct later l = {.h = create(0)};
    pthread_t t;
    pthread_create(&t, NULL, signal_later, &l);
    res("wait for submit, signaled by a thread", wait1(l.h, FOR_SUBMIT, (int64_t)(now() + 5000000000ull), NULL));
    pthread_join(t, NULL);
    destroy(l.h);
    l.h = create(0);
    l.point = 4;
    pthread_create(&t, NULL, signal_later, &l);
    res("timeline wait point 4, signaled by a thread", twait1(l.h, 4, FOR_SUBMIT, (int64_t)(now() + 5000000000ull)));
    pthread_join(t, NULL);
    destroy(l.h);

    /* FS-17: a sleeper in epoll_wait or poll on an eventfd armed by SYNCOBJ_EVENTFD must be woken by the
     * signal of another thread (a lost wake shows as 0 events after 5 s). */
    int e = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN, .data.u64 = 7};
    epoll_ctl(ep, EPOLL_CTL_ADD, e, &ev);
    l.h = create(0);
    l.point = 6;
    struct so_eventfd a = {.handle = l.h, .point = 6, .fd = e};
    res("eventfd on point 6 for a blocking epoll", io(fd, EVENTFD, &a));
    pthread_create(&t, NULL, signal_later, &l);
    struct epoll_event got = {0};
    int n = epoll_wait(ep, &got, 1, 5000);
    printf("blocking epoll_wait on the eventfd: n=%d events=%#x data=%llu\n", n, n > 0 ? got.events : 0,
           n > 0 ? (unsigned long long)got.data.u64 : 0ull);
    pthread_join(t, NULL);
    res("eventfd count after the blocking epoll", eventfd_count(e));
    l.point = 7;
    a.point = 7;
    res("eventfd on point 7 for a blocking poll", io(fd, EVENTFD, &a));
    pthread_create(&t, NULL, signal_later, &l);
    struct pollfd pf = {.fd = e, .events = POLLIN};
    n = poll(&pf, 1, 5000);
    printf("blocking poll on the eventfd: n=%d revents=%#x\n", n, n > 0 ? pf.revents : 0);
    pthread_join(t, NULL);
    res("eventfd count after the blocking poll", eventfd_count(e));
    destroy(l.h);
    close(ep);
    close(e);
}

static void timeline_checks(void) {
    uint32_t t = create(0);
    uint64_t p = 99;
    res("query fresh", query1(t, 0, &p));
    printf("query fresh = %llu\n", (unsigned long long)p);
    res("timeline signal 5", tsignal1(t, 5));
    query1(t, 0, &p);
    printf("query after 5 = %llu\n", (unsigned long long)p);
    query1(t, 1, &p);
    printf("query last submitted after 5 = %llu\n", (unsigned long long)p);
    res("timeline wait 3", twait1(t, 3, 0, 0));
    res("timeline wait 5", twait1(t, 5, 0, 0));
    res("timeline wait 7", twait1(t, 7, 0, 0));
    res("timeline wait 7, for submit, timeout 0", twait1(t, 7, FOR_SUBMIT, 0));
    res("timeline wait 5, available", twait1(t, 5, AVAILABLE, 0));
    res("timeline wait 7, available", twait1(t, 7, AVAILABLE, 0));
    res("timeline wait 7, available and for submit, timeout 0", twait1(t, 7, AVAILABLE | FOR_SUBMIT, 0));
    res("timeline wait flags=8", twait1(t, 5, 8, 0));
    res("timeline wait flags=0x10", twait1(t, 5, 0x10, 0));
    res("timeline signal 3 (out of order)", tsignal1(t, 3));
    query1(t, 0, &p);
    printf("query after 3 = %llu\n", (unsigned long long)p);
    res("timeline wait 4 after out of order", twait1(t, 4, 0, 0));
    tsignal1(t, 9);
    query1(t, 0, &p);
    printf("query after 9 = %llu\n", (unsigned long long)p);
    res("timeline wait 5 after 9", twait1(t, 5, 0, 0));
    struct so_tarray a = {.handles = (uintptr_t)&t, .points = 0, .count = 1};
    res("timeline signal without points", io(fd, TSIGNAL, &a));
    query1(t, 0, &p);
    printf("query after point 0 = %llu\n", (unsigned long long)p);
    a.flags = 1;
    res("timeline signal flags=1", io(fd, TSIGNAL, &a));
    a.flags = 0;
    a.count = 0;
    res("timeline signal count 0", io(fd, TSIGNAL, &a));
    a.count = 1;
    a.points = (uintptr_t)KERNEL_ADDR;
    res("timeline signal points kernel address", io(fd, TSIGNAL, &a));
    struct so_tarray q = {.handles = (uintptr_t)&t, .points = (uintptr_t)KERNEL_ADDR, .count = 1};
    res("query points kernel address", io(fd, QUERY, &q));
    q.points = (uintptr_t)&p;
    q.flags = 2;
    res("query flags=2", io(fd, QUERY, &q));
    q.flags = 0;
    q.count = 0;
    res("query count 0", io(fd, QUERY, &q));
    uint32_t bin = create(1);
    query1(bin, 0, &p);
    printf("query binary = %llu\n", (unsigned long long)p);
    uint64_t pt = 2;
    struct so_twait tw = {.handles = (uintptr_t)&bin, .points = (uintptr_t)&pt, .count = 1};
    res("timeline wait point 2 on a binary", io(fd, TWAIT, &tw));
    pt = 0;
    res("timeline wait point 0 on a binary", io(fd, TWAIT, &tw));
    tw.points = 0;
    res("timeline wait without points", io(fd, TWAIT, &tw));
    tw.points = (uintptr_t)KERNEL_ADDR;
    res("timeline wait points kernel address", io(fd, TWAIT, &tw));

    uint32_t t2 = create(0);
    tsignal1(t2, 1);
    tsignal1(t2, 2);
    tsignal1(t2, 3);
    query1(t2, 0, &p);
    printf("query 1,2,3 = %llu\n", (unsigned long long)p);
    destroy(t2);
    destroy(bin);
    destroy(t);
}

static void transfer_checks(void) {
    uint32_t bin = create(1), t = create(0), out = create(0);
    struct so_transfer x = {.src = bin, .dst = t, .src_point = 0, .dst_point = 10};
    res("transfer binary to point 10", io(fd, TRANSFER, &x));
    uint64_t p = 0;
    query1(t, 0, &p);
    printf("query after transfer = %llu\n", (unsigned long long)p);
    struct so_transfer y = {.src = t, .dst = out, .src_point = 10, .dst_point = 0};
    res("transfer point 10 to binary", io(fd, TRANSFER, &y));
    res("wait the binary", wait1(out, 0, 0, NULL));
    y.src_point = 11;
    res("transfer point 11 (not submitted)", io(fd, TRANSFER, &y));
    y.src_point = 10;
    y.pad = 1;
    res("transfer pad=1", io(fd, TRANSFER, &y));
    y.pad = 0;
    y.dst = 9999;
    res("transfer to unknown", io(fd, TRANSFER, &y));
    y.dst = out;
    y.src = 9999;
    res("transfer from unknown", io(fd, TRANSFER, &y));
    y.src = t;
    y.flags = 0x10;
    res("transfer flags=0x10", io(fd, TRANSFER, &y));
    uint32_t empty = create(0);
    struct so_transfer z = {.src = empty, .dst = out};
    res("transfer from a binary without fence", io(fd, TRANSFER, &z));
    res("the destination kept its fence", wait1(out, 0, 0, NULL));
    destroy(empty);
    destroy(bin);
    destroy(t);
    destroy(out);
}

static void fd_checks(void) {
    uint32_t h = create(0);
    int f = -1;
    res("handle to fd", to_fd(h, 0, 0, &f));
    yes("handle to fd: an fd", f >= 0);
    yes("syncobj fd: close-on-exec", f >= 0 && (fcntl(f, F_GETFD) & FD_CLOEXEC));
    char link[64] = "", path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d", f);
    long n = readlink(path, link, sizeof link - 1);
    if (n > 0)
        link[n] = 0;
    printf("syncobj fd link = %s\n", link);
    struct stat st;
    res("syncobj fd fstat", fstat(f, &st) ? -errno : 0);
    printf("syncobj fd type = %o\n", st.st_mode & S_IFMT);
    char b[8];
    res("syncobj fd read", read(f, b, 8) < 0 ? -errno : 0);
    struct pollfd pf = {.fd = f, .events = POLLIN | POLLOUT};
    res("syncobj fd poll", poll(&pf, 1, 0));
    printf("syncobj fd poll revents = %#x\n", pf.revents);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN};
    res("syncobj fd epoll_ctl", epoll_ctl(ep, EPOLL_CTL_ADD, f, &ev) ? -errno : 0);
    res("syncobj fd unknown ioctl", io(f, SYNC_INFO, b));
    uint32_t h2 = 0;
    res("fd to handle", from_fd(f, 0, 0, &h2));
    yes("fd to handle: a new handle", h2 != 0 && h2 != h);
    signal1(h);
    res("signaled through one handle, waited through the other", wait1(h2, 0, 0, NULL));
    destroy(h);
    destroy(h2);
    uint32_t h3 = 0;
    res("fd to handle after both handles went", from_fd(f, 0, 0, &h3));
    res("the object lives on in the fd", wait1(h3, 0, 0, NULL));
    destroy(h3);
    uint32_t h4 = 0;
    res("fd to handle of the drm fd", from_fd(fd, 0, 0, &h4));
    res("fd to handle of a closed fd", from_fd(1000, 0, 0, &h4));
    res("fd to handle of fd -1", from_fd(-1, 0, 0, &h4));
    struct so_handle a = {.handle = h4, .fd = f, .pad = 1};
    res("fd to handle pad=1", io(fd, FROM_FD, &a));
    a.pad = 0;
    a.flags = 4;
    res("fd to handle flags=4", io(fd, FROM_FD, &a));
    a.flags = 0;
    a.point = 3;
    res("fd to handle point without timeline flag", io(fd, FROM_FD, &a));
    int g;
    res("handle to fd of unknown", to_fd(9999, 0, 0, &g));
    uint32_t h5 = create(0);
    res("handle to fd flags=4", to_fd(h5, 4, 0, &g));
    res("handle to fd point without timeline flag", to_fd(h5, 0, 3, &g));
    struct so_handle pa = {.handle = h5, .pad = 1};
    res("handle to fd pad=1", io(fd, TO_FD, &pa));
    destroy(h5);
    close(ep);
    close(f);
}

static void info(const char *label, int f) {
    struct sync_file_info i = {.num_fences = 0};
    long r = io(f, SYNC_INFO, &i);
    res(label, r);
    printf("%s: name \"%s\", status %d, fences %u\n", label, i.name, i.status, i.num_fences);
    if (r || i.num_fences == 0 || i.num_fences > 4)
        return;
    struct sync_fence_info fi[4];
    memset(fi, 0x55, sizeof fi);
    i.info = (uintptr_t)fi;
    r = io(f, SYNC_INFO, &i);
    res(label, r);
    for (uint32_t k = 0; k < i.num_fences; k++)
        printf("%s: fence %u obj \"%s\" driver \"%s\" status %d flags %u timestamp %s\n", label, k, fi[k].obj_name, fi[k].driver_name,
               fi[k].status, fi[k].flags, fi[k].timestamp_ns ? "set" : "zero");
}

static void sync_file_checks(void) {
    uint32_t none = create(0), sig = create(1);
    int s = -1, s2 = -1;
    res("export a syncobj without fence", to_fd(none, EXPORT_SYNC_FILE, 0, &s));
    res("export a signaled syncobj", to_fd(sig, EXPORT_SYNC_FILE, 0, &s));
    yes("sync_file: close-on-exec", s >= 0 && (fcntl(s, F_GETFD) & FD_CLOEXEC));
    char link[64] = "", path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d", s);
    long n = readlink(path, link, sizeof link - 1);
    if (n > 0)
        link[n] = 0;
    printf("sync_file link = %s\n", link);
    struct pollfd pf = {.fd = s, .events = POLLIN | POLLOUT};
    res("sync_file poll", poll(&pf, 1, 0));
    printf("sync_file poll revents = %#x\n", pf.revents);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    struct epoll_event ev = {.events = EPOLLIN}, got = {0};
    res("sync_file epoll_ctl", epoll_ctl(ep, EPOLL_CTL_ADD, s, &ev) ? -errno : 0);
    res("sync_file epoll_wait", epoll_wait(ep, &got, 1, 0));
    printf("sync_file epoll events = %#x\n", got.events);
    close(ep);
    info("sync_file info", s);
    char b[8];
    res("sync_file read", read(s, b, 8) < 0 ? -errno : 0);
    res("sync_file unknown ioctl", io(s, IOC(3u, 0x3eu, 9, 8), b));
    struct sync_file_info bad = {.flags = 1};
    res("sync_file info flags=1", io(s, SYNC_INFO, &bad));
    res("sync_file info kernel address", io(s, SYNC_INFO, KERNEL_ADDR));
    struct sync_file_info i0 = {.num_fences = 1, .info = (uintptr_t)KERNEL_ADDR};
    res("sync_file info array kernel address", io(s, SYNC_INFO, &i0));

    signal1(none);
    res("export the second syncobj", to_fd(none, EXPORT_SYNC_FILE, 0, &s2));
    struct sync_merge m = {.name = "merged", .fd2 = s2, .fence = -1};
    res("merge", io(s, SYNC_MERGE, &m));
    yes("merge: a new fd", m.fence >= 0 && m.fence != s && m.fence != s2);
    yes("merged: close-on-exec", m.fence >= 0 && (fcntl(m.fence, F_GETFD) & FD_CLOEXEC));
    info("merged info", m.fence);
    struct sync_merge m2 = {.name = "x", .fd2 = s2, .flags = 1};
    res("merge flags=1", io(s, SYNC_MERGE, &m2));
    m2.flags = 0;
    m2.pad = 1;
    res("merge pad=1", io(s, SYNC_MERGE, &m2));
    m2.pad = 0;
    m2.fd2 = fd;
    res("merge with the drm fd", io(s, SYNC_MERGE, &m2));
    m2.fd2 = 1000;
    res("merge with a closed fd", io(s, SYNC_MERGE, &m2));
    m2.fd2 = s;
    res("merge with itself", io(s, SYNC_MERGE, &m2));
    if (m2.fence >= 0)
        close(m2.fence);

    uint32_t imp = create(0);
    res("import a sync_file", from_fd(s, IMPORT_SYNC_FILE, 0, &imp));
    res("the importer is signaled", wait1(imp, 0, 0, NULL));
    uint32_t unk = 9999;
    res("import into an unknown handle", from_fd(s, IMPORT_SYNC_FILE, 0, &unk));
    uint32_t imp2 = imp;
    res("import the drm fd", from_fd(fd, IMPORT_SYNC_FILE, 0, &imp2));
    int sofd = -1;
    to_fd(imp, 0, 0, &sofd);
    imp2 = imp;
    res("import a syncobj fd as sync_file", from_fd(sofd, IMPORT_SYNC_FILE, 0, &imp2));
    uint32_t imp3 = 0;
    res("fd to handle of a sync_file", from_fd(s, 0, 0, &imp3));
    close(sofd);

    uint32_t t = create(0);
    tsignal1(t, 6);
    int ts = -1;
    res("export point 6 of a timeline", to_fd(t, EXPORT_SYNC_FILE | TIMELINE, 6, &ts));
    if (ts >= 0)
        close(ts);
    res("export point 8 of a timeline (not submitted)", to_fd(t, EXPORT_SYNC_FILE | TIMELINE, 8, &ts));
    int tf = -1;
    res("handle to fd, timeline flag, point 0", to_fd(t, TIMELINE, 0, &tf));
    if (tf >= 0)
        close(tf);
    res("handle to fd, timeline flag, point 6", to_fd(t, TIMELINE, 6, &tf));
    uint32_t tt = t;
    res("import a sync_file at point 9", from_fd(s, IMPORT_SYNC_FILE | TIMELINE, 9, &tt));
    uint64_t p = 0;
    query1(t, 0, &p);
    printf("query after import at 9 = %llu\n", (unsigned long long)p);
    tt = t;
    res("fd to handle, timeline flag only", from_fd(s, TIMELINE, 0, &tt));
    destroy(t);
    destroy(imp);
    destroy(none);
    destroy(sig);
    close(m.fence);
    close(s);
    close(s2);
}

static long eventfd_count(int e) {
    uint64_t v = 0;
    return read(e, &v, 8) == 8 ? (long)v : -errno;
}

static void eventfd_checks(void) {
    int e = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    uint32_t sig = create(1), none = create(0), t = create(0);
    struct so_eventfd a = {.handle = sig, .fd = e};
    res("eventfd on a signaled syncobj", io(fd, EVENTFD, &a));
    res("eventfd count", eventfd_count(e));
    a.handle = none;
    res("eventfd on a syncobj without fence", io(fd, EVENTFD, &a));
    res("eventfd count before signal", eventfd_count(e));
    signal1(none);
    res("eventfd count after signal", eventfd_count(e));
    a.handle = t;
    a.point = 3;
    res("eventfd on timeline point 3", io(fd, EVENTFD, &a));
    tsignal1(t, 2);
    res("eventfd count after point 2", eventfd_count(e));
    tsignal1(t, 3);
    res("eventfd count after point 3", eventfd_count(e));
    a.point = 5;
    a.flags = AVAILABLE;
    res("eventfd on point 5, available", io(fd, EVENTFD, &a));
    tsignal1(t, 5);
    res("eventfd count after point 5", eventfd_count(e));
    a.flags = 1;
    res("eventfd flags=1", io(fd, EVENTFD, &a));
    a.flags = 0;
    a.pad = 1;
    res("eventfd pad=1", io(fd, EVENTFD, &a));
    a.pad = 0;
    a.handle = 9999;
    res("eventfd unknown handle", io(fd, EVENTFD, &a));
    a.handle = t;
    a.fd = fd;
    res("eventfd on the drm fd", io(fd, EVENTFD, &a));
    a.fd = 1000;
    res("eventfd on a closed fd", io(fd, EVENTFD, &a));

    /* A registration outlives its syncobj handle: destroy, then signal through a second handle. */
    uint32_t w = create(0);
    int wf = -1;
    to_fd(w, 0, 0, &wf);
    a.handle = w;
    a.fd = e;
    a.point = 0;
    res("eventfd on a syncobj that is destroyed next", io(fd, EVENTFD, &a));
    destroy(w);
    uint32_t w2 = 0;
    from_fd(wf, 0, 0, &w2);
    signal1(w2);
    res("eventfd count after signal through the fd's handle", eventfd_count(e));
    destroy(w2);
    close(wf);
    destroy(sig);
    destroy(none);
    destroy(t);
    close(e);
}

static void more_checks(void) {
    uint32_t s = create(1), n = create(0), t = create(0);
    res("wait flags=deadline", wait1(s, 8, 0, NULL));
    struct so_twait tw = {.count = 0, .flags = 0x40};
    res("timeline wait count 0, flags=0x40", io(fd, TWAIT, &tw));
    tw.flags = 0;
    res("timeline wait count 0", io(fd, TWAIT, &tw));
    for (uint32_t f = 1; f <= 8; f <<= 1) {
        struct so_transfer x = {.src = s, .dst = n, .flags = f};
        char l[48];
        snprintf(l, sizeof l, "transfer flags=%#x", f);
        res(l, io(fd, TRANSFER, &x));
    }
    res("timeline wait binary without fence, available", twait1(n, 0, AVAILABLE, 0));
    tsignal1(t, 5);
    tsignal1(t, 3);
    uint64_t p = 0;
    query1(t, 1, &p);
    printf("query last submitted after 5, 3 = %llu\n", (unsigned long long)p);
    signal1(t);
    query1(t, 0, &p);
    printf("query after a binary signal = %llu\n", (unsigned long long)p);
    res("timeline wait 1 after a binary signal", twait1(t, 1, 0, 0));
    int sf = -1;
    res("export with a point but no timeline flag", to_fd(t, EXPORT_SYNC_FILE, 6, &sf));
    uint32_t t2 = create(0);
    tsignal1(t2, 4);
    uint32_t tt = t2;
    res("import with a point but no timeline flag", from_fd(sf, IMPORT_SYNC_FILE, 9, &tt));
    query1(t2, 0, &p);
    printf("query after that import = %llu\n", (unsigned long long)p);
    struct sync_fence_info fi[3];
    memset(fi, 0x55, sizeof fi);
    struct sync_file_info si = {.num_fences = 3, .info = (uintptr_t)fi};
    res("sync_file info room for 3", io(sf, SYNC_INFO, &si));
    printf("info room for 3: fences %u, second entry untouched %s\n", si.num_fences, fi[1].status == 0x55555555 ? "yes" : "no");
    struct sync_merge m = {.fd2 = sf};
    memset(m.name, 'a', sizeof m.name);
    res("merge with an unterminated name", io(sf, SYNC_MERGE, &m));
    struct sync_file_info si2 = {0};
    io(m.fence, SYNC_INFO, &si2);
    printf("merged name length = %zu\n", strlen(si2.name));
    close(m.fence);
    res("merge kernel address", io(sf, SYNC_MERGE, KERNEL_ADDR));
    struct stat st;
    fstat(sf, &st);
    printf("sync_file type %o mode %o\n", st.st_mode & S_IFMT, st.st_mode & 07777);
    printf("sync_file F_GETFL = %#x\n", fcntl(sf, F_GETFL));
    res("sync_file write", write(sf, "x", 1) < 0 ? -errno : 0);
    struct { uint64_t d, pad; } dl = {123, 0};
    res("set deadline", io(sf, IOC(1u, 0x3eu, 5, 16), &dl));
    dl.pad = 1;
    res("set deadline pad=1", io(sf, IOC(1u, 0x3eu, 5, 16), &dl));
    int e = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    uint32_t t3 = create(0);
    struct so_eventfd ea = {.handle = t3, .fd = e, .point = 5};
    io(fd, EVENTFD, &ea);
    tsignal1(t3, 7);
    res("eventfd for point 5 after point 7", eventfd_count(e));
    ea.point = 0;
    uint32_t n2 = create(0);
    ea.handle = n2;
    io(fd, EVENTFD, &ea);
    tsignal1(n2, 1);
    res("eventfd for point 0 after timeline point 1", eventfd_count(e));
    close(e);
    close(sf);
    destroy(t3);
    destroy(n2);
    destroy(t2);
    destroy(s);
    destroy(n);
    destroy(t);
}

static void close_checks(void) {
    /* Handles of a closed file are gone with it; another open has its own table. */
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    char node[64] = "";
    long n = readlink(p, node, sizeof node - 1);
    if (n <= 0)
        return;
    node[n] = 0;
    int f2 = open(node, O_RDWR | O_CLOEXEC);
    struct so_create c = {0};
    io(f2, CREATE, &c);
    uint32_t h = c.handle;
    res("a second open's handle in the first open", wait1(h, 0, 0, NULL) == -ENOENT ? -ENOENT : 0);
    close(f2);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (!pick_node()) {
        printf("amdgpu render node: none\n");
        return 0;
    }
    printf("amdgpu render node: found\n");
    create_checks();
    wait_checks();
    blocking_checks();
    timeline_checks();
    transfer_checks();
    fd_checks();
    sync_file_checks();
    eventfd_checks();
    more_checks();
    close_checks();
    /* Last: Linux 7.3 frees the never-used handle 0 here (a later CREATE would get 0). */
    struct so_destroy d0 = {.handle = 0};
    res("destroy handle 0", io(fd, DESTROY, &d0));
    close(fd);
    return 0;
}
