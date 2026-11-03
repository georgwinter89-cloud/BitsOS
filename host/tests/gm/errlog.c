// GM-1a: an LD_PRELOAD logger for the Mesa programs: every failed call of the libc entry points Mesa, LLVM, the
// Vulkan loader and Wayland use for kernel work, with its errno, once per (call, key, errno) and process. V2 has no
// ptrace (no strace), and the kernel logs only ENOSYS/ENOTTY; this names the EINVALs too. Run on CachyOS with the
// same programs, it gives the failures Linux has as well (the diff is V2's gaps).
//   ERRLOG_FILE=path (default /dev/stderr), lines: `errlog <prog> <call> <key> -> <errno name>`
// EAGAIN/EINTR (normal waits) are left out, and ENOENT too unless the path is below /dev, /sys or /proc: those
// name an interface V2 lacks (a render node, a sysfs file), the others are search paths.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dirent.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

extern char *program_invocation_short_name;

static int logfd = -1;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
#define SEEN_MAX 512
static unsigned long seen[SEEN_MAX];
static int nseen;

static const char *ename(int e) {
    switch (e) {
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case ENXIO: return "ENXIO";
    case EBADF: return "EBADF";
    case ENOMEM: return "ENOMEM";
    case EACCES: return "EACCES";
    case EFAULT: return "EFAULT";
    case EBUSY: return "EBUSY";
    case EEXIST: return "EEXIST";
    case ENODEV: return "ENODEV";
    case ENOTDIR: return "ENOTDIR";
    case EISDIR: return "EISDIR";
    case EINVAL: return "EINVAL";
    case ENOTTY: return "ENOTTY";
    case ENOSPC: return "ENOSPC";
    case ESPIPE: return "ESPIPE";
    case EROFS: return "EROFS";
    case ERANGE: return "ERANGE";
    case ENOSYS: return "ENOSYS";
    case ENOTSUP: return "EOPNOTSUPP";
    case ENOPROTOOPT: return "ENOPROTOOPT";
    case EAFNOSUPPORT: return "EAFNOSUPPORT";
    case ECONNREFUSED: return "ECONNREFUSED";
    default: return "E?";
    }
}

// One line per new (call, key, errno); errno is kept for the caller.
static void note(const char *call, const char *key, int e) {
    if (e == EAGAIN || e == EINTR)
        return;
    if (e == ENOENT && strncmp(key, "/dev/", 5) && strncmp(key, "/sys/", 5) && strncmp(key, "/proc/", 6))
        return;
    unsigned long h = 5381;
    for (const char *s = call; *s; s++) h = h * 33 + (unsigned char)*s;
    for (const char *s = key; *s; s++) h = h * 33 + (unsigned char)*s;
    h = h * 33 + (unsigned)e;
    pthread_mutex_lock(&lock);
    for (int i = 0; i < nseen; i++)
        if (seen[i] == h) { pthread_mutex_unlock(&lock); return; }
    if (nseen < SEEN_MAX) seen[nseen++] = h;
    if (logfd < 0) {
        const char *p = getenv("ERRLOG_FILE");
        logfd = p ? (int)syscall(SYS_openat, AT_FDCWD, p, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644) : 2;
    }
    char line[512];
    int n = snprintf(line, sizeof line, "errlog %s %s %s -> %s (%d)\n", program_invocation_short_name, call, key, ename(e), e);
    if (logfd >= 0 && n > 0) (void)!write(logfd, line, (size_t)n < sizeof line ? (size_t)n : sizeof line - 1);
    pthread_mutex_unlock(&lock);
}

#define REAL(ret, name, ...) \
    static ret (*real)(__VA_ARGS__); \
    if (!real) real = (ret (*)(__VA_ARGS__))dlsym(RTLD_NEXT, name)

static void fdkey(char *k, size_t n, int fd, const char *more) {
    char p[64], t[160];
    snprintf(p, sizeof p, "/proc/self/fd/%d", fd);
    ssize_t r = syscall(SYS_readlinkat, AT_FDCWD, p, t, sizeof t - 1);
    t[r > 0 ? r : 0] = 0;
    snprintf(k, n, "fd=%s%s", r > 0 ? t : "?", more);
}

int ioctl(int fd, unsigned long req, ...) {
    va_list ap;
    va_start(ap, req);
    void *arg = va_arg(ap, void *);
    va_end(ap);
    REAL(int, "ioctl", int, unsigned long, void *);
    int r = real(fd, req, arg);
    if (r < 0) {
        int e = errno;
        char m[48], k[256];
        snprintf(m, sizeof m, ",req=0x%lx", req);
        fdkey(k, sizeof k, fd, m);
        note("ioctl", k, e);
        errno = e;
    }
    return r;
}

static int open_common(const char *call, int r, const char *path, int flags) {
    if (r < 0) {
        int e = errno;
        char k[256];
        snprintf(k, sizeof k, "%s,flags=0x%x", path ? path : "?", flags);
        note(call, k, e);
        errno = e;
    }
    return r;
}

#define OPEN_WRAP(name)                                                          \
    int name(const char *path, int flags, ...) {                                 \
        va_list ap;                                                              \
        va_start(ap, flags);                                                     \
        int mode = va_arg(ap, int);                                              \
        va_end(ap);                                                              \
        REAL(int, #name, const char *, int, int);                                \
        return open_common(#name, real(path, flags, mode), path, flags);         \
    }
OPEN_WRAP(open)
OPEN_WRAP(open64)

#define OPENAT_WRAP(name)                                                        \
    int name(int dfd, const char *path, int flags, ...) {                        \
        va_list ap;                                                              \
        va_start(ap, flags);                                                     \
        int mode = va_arg(ap, int);                                              \
        va_end(ap);                                                              \
        REAL(int, #name, int, const char *, int, int);                           \
        return open_common(#name, real(dfd, path, flags, mode), path, flags);    \
    }
OPENAT_WRAP(openat)
OPENAT_WRAP(openat64)

void *mmap(void *a, size_t len, int prot, int flags, int fd, off_t off) {
    REAL(void *, "mmap", void *, size_t, int, int, int, off_t);
    void *r = real(a, len, prot, flags, fd, off);
    if (r == MAP_FAILED) {
        int e = errno;
        char m[96], k[256];
        snprintf(m, sizeof m, ",prot=0x%x,flags=0x%x", prot, flags);
        if (fd >= 0) fdkey(k, sizeof k, fd, m); else snprintf(k, sizeof k, "anon%s", m);
        note("mmap", k, e);
        errno = e;
    }
    return r;
}
void *mmap64(void *a, size_t len, int prot, int flags, int fd, off_t off) { return mmap(a, len, prot, flags, fd, off); }

#define INT_WRAP(name, fmt, ...)                       \
    {                                                  \
        if (r < 0) {                                   \
            int e = errno;                             \
            char k[128];                               \
            snprintf(k, sizeof k, fmt, __VA_ARGS__);   \
            note(name, k, e);                          \
            errno = e;                                 \
        }                                              \
        return r;                                      \
    }

int mprotect(void *a, size_t len, int prot) {
    REAL(int, "mprotect", void *, size_t, int);
    int r = real(a, len, prot);
    INT_WRAP("mprotect", "prot=0x%x", prot)
}
int madvise(void *a, size_t len, int adv) {
    REAL(int, "madvise", void *, size_t, int);
    int r = real(a, len, adv);
    INT_WRAP("madvise", "advice=%d", adv)
}
int memfd_create(const char *name, unsigned flags) {
    REAL(int, "memfd_create", const char *, unsigned);
    int r = real(name, flags);
    INT_WRAP("memfd_create", "flags=0x%x", flags)
}
int fcntl(int fd, int cmd, ...) {
    va_list ap;
    va_start(ap, cmd);
    long arg = va_arg(ap, long);
    va_end(ap);
    REAL(int, "fcntl", int, int, long);
    int r = real(fd, cmd, arg);
    INT_WRAP("fcntl", "cmd=%d,arg=0x%lx", cmd, arg)
}
int fcntl64(int fd, int cmd, ...) {
    va_list ap;
    va_start(ap, cmd);
    long arg = va_arg(ap, long);
    va_end(ap);
    REAL(int, "fcntl64", int, int, long);
    int r = real(fd, cmd, arg);
    INT_WRAP("fcntl", "cmd=%d,arg=0x%lx", cmd, arg)
}
int prctl(int op, ...) {
    va_list ap;
    va_start(ap, op);
    unsigned long a2 = va_arg(ap, unsigned long), a3 = va_arg(ap, unsigned long), a4 = va_arg(ap, unsigned long),
                  a5 = va_arg(ap, unsigned long);
    va_end(ap);
    REAL(int, "prctl", int, unsigned long, unsigned long, unsigned long, unsigned long);
    int r = real(op, a2, a3, a4, a5);
    INT_WRAP("prctl", "op=%d,arg2=0x%lx", op, a2)
}
long syscall(long nr, ...) {
    va_list ap;
    va_start(ap, nr);
    long a[6];
    for (int i = 0; i < 6; i++) a[i] = va_arg(ap, long);
    va_end(ap);
    REAL(long, "syscall", long, long, long, long, long, long, long);
    long r = real(nr, a[0], a[1], a[2], a[3], a[4], a[5]);
    // futex waits time out and get woken (ETIMEDOUT, EAGAIN): their normal ends.
    if (r < 0 && !(nr == SYS_futex && errno == ETIMEDOUT)) {
        int e = errno;
        char k[96];
        snprintf(k, sizeof k, "nr=%ld,arg1=0x%lx", nr, nr == SYS_futex ? a[1] : a[0]);
        note("syscall", k, e);
        errno = e;
    }
    return r;
}
int flock(int fd, int op) {
    REAL(int, "flock", int, int);
    int r = real(fd, op);
    INT_WRAP("flock", "op=%d", op)
}
int setpriority(int which, unsigned who, int prio) {
    REAL(int, "setpriority", int, unsigned, int);
    int r = real(which, who, prio);
    INT_WRAP("setpriority", "which=%d,prio=%d", which, prio)
}
int sched_setaffinity(pid_t pid, size_t n, const cpu_set_t *set) {
    REAL(int, "sched_setaffinity", pid_t, size_t, const cpu_set_t *);
    int r = real(pid, n, set);
    INT_WRAP("sched_setaffinity", "size=%zu", n)
}
int sched_getaffinity(pid_t pid, size_t n, cpu_set_t *set) {
    REAL(int, "sched_getaffinity", pid_t, size_t, cpu_set_t *);
    int r = real(pid, n, set);
    INT_WRAP("sched_getaffinity", "size=%zu", n)
}
int pthread_setaffinity_np(pthread_t t, size_t n, const cpu_set_t *set) {
    REAL(int, "pthread_setaffinity_np", pthread_t, size_t, const cpu_set_t *);
    int r = real(t, n, set);
    if (r != 0) { note("pthread_setaffinity_np", "", r); }
    return r;
}
int pthread_setname_np(pthread_t t, const char *name) {
    REAL(int, "pthread_setname_np", pthread_t, const char *);
    int r = real(t, name);
    if (r != 0) { note("pthread_setname_np", name, r); }
    return r;
}
int socket(int dom, int type, int proto) {
    REAL(int, "socket", int, int, int);
    int r = real(dom, type, proto);
    INT_WRAP("socket", "domain=%d,type=0x%x", dom, type)
}
int eventfd(unsigned v, int flags) {
    REAL(int, "eventfd", unsigned, int);
    int r = real(v, flags);
    INT_WRAP("eventfd", "flags=0x%x", flags)
}
int mlock(const void *a, size_t len) {
    REAL(int, "mlock", const void *, size_t);
    int r = real(a, len);
    INT_WRAP("mlock", "len=%zu", len)
}
int posix_fallocate(int fd, off_t off, off_t len) {
    REAL(int, "posix_fallocate", int, off_t, off_t);
    int r = real(fd, off, len);
    if (r != 0) { char k[256]; fdkey(k, sizeof k, fd, ""); note("posix_fallocate", k, r); }
    return r;
}
int ftruncate(int fd, off_t len) {
    REAL(int, "ftruncate", int, off_t);
    int r = real(fd, len);
    if (r < 0) { int e = errno; char k[256]; fdkey(k, sizeof k, fd, ""); note("ftruncate", k, e); errno = e; }
    return r;
}

// Probes of device and sysfs paths (libdrm, the loaders): their ENOENT is a missing interface.
#define PATH_WRAP(name, sig, args, path)                     \
    int name sig {                                           \
        REAL(int, #name, PATHARGS_##name);                   \
        int r = real args;                                   \
        if (r < 0) {                                         \
            int e = errno;                                   \
            note(#name, path, e);            \
            errno = e;                                       \
        }                                                    \
        return r;                                            \
    }
#define PATHARGS_stat const char *, struct stat *
#define PATHARGS_lstat const char *, struct stat *
#define PATHARGS_access const char *, int
#define PATHARGS_fstatat int, const char *, struct stat *, int
PATH_WRAP(stat, (const char *p, struct stat *st), (p, st), p)
PATH_WRAP(lstat, (const char *p, struct stat *st), (p, st), p)
PATH_WRAP(access, (const char *p, int m), (p, m), p)
PATH_WRAP(fstatat, (int d, const char *p, struct stat *st, int f), (d, p, st, f), p)

ssize_t readlink(const char *p, char *buf, size_t n) {
    REAL(ssize_t, "readlink", const char *, char *, size_t);
    ssize_t r = real(p, buf, n);
    // EINVAL is "not a link": the normal answer for a plain file.
    if (r < 0 && errno != EINVAL) { int e = errno; note("readlink", p, e); errno = e; }
    return r;
}

DIR *opendir(const char *p) {
    REAL(DIR *, "opendir", const char *);
    DIR *r = real(p);
    if (!r) { int e = errno; note("opendir", p, e); errno = e; }
    return r;
}
