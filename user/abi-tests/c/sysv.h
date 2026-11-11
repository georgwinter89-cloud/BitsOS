/* SX-IPC: helpers for the SysV IPC ABI programs (abi-sysv-*). Most run inside a fresh user and IPC namespace, so ids
 * and tables start the same on CachyOS and on V2, with no capabilities (`no_caps`) unless a line says otherwise. */
#ifndef SYSV_H
#define SYSV_H
#include "ns.h"
#include <linux/capability.h>
#include <pthread.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/resource.h>

#define K 0x51c0de00
#define PAGE 4096l

#ifndef SHM_HUGETLB
#define SHM_HUGETLB 04000
#endif
#ifndef SHM_NORESERVE
#define SHM_NORESERVE 010000
#endif
#define SHM_RDONLY_ 010000
#define SHM_RND_ 020000
#define SHM_REMAP_ 040000
#define SHM_EXEC_ 0100000
#define SHM_DEST_ 01000
#define SHM_LOCKED_ 02000
#define SHM_LOCK_ 11
#define SHM_UNLOCK_ 12
#define SHM_STAT_ 13
#define SHM_INFO_ 14
#define SHM_STAT_ANY_ 15
#define MSG_STAT_ 11
#define MSG_INFO_ 12
#define MSG_STAT_ANY_ 13
#define MSG_NOERROR_ 010000
#define MSG_EXCEPT_ 020000
#define MSG_COPY_ 040000
#define SEM_UNDO_ 0x1000
#define GETPID_ 11
#define GETVAL_ 12
#define GETALL_ 13
#define GETNCNT_ 14
#define GETZCNT_ 15
#define SETVAL_ 16
#define SETALL_ 17
#define SEM_STAT_ 18
#define SEM_INFO_ 19
#define SEM_STAT_ANY_ 20

struct sb { unsigned short num; short op; short flg; };
struct perm64 { int key; unsigned uid, gid, cuid, cgid, mode; unsigned short seq, pad; unsigned long a, b; };
struct sem64 { struct perm64 p; long otime; unsigned long u1; long ctime; unsigned long u2; unsigned long nsems, u3, u4; };
struct shm64 { struct perm64 p; size_t segsz; long atime, dtime, ctime; int cpid, lpid; unsigned long nattch, u4, u5; };
struct msq64 { struct perm64 p; long stime, rtime, ctime; unsigned long cbytes, qnum, qbytes; int lspid, lrpid; unsigned long u4, u5; };

static inline const char *sysv_err(int e) {
    switch (e) {
    case 42: return "ENOMSG";
    case 43: return "EIDRM";
    case EFBIG: return "EFBIG";
    case ENOSPC: return "ENOSPC";
    }
    return en(e);
}

static inline long srep(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, sysv_err(errno));
    else
        printf("%s = %ld\n", label, r);
    fflush(stdout);
    return r;
}
#define S(label, ...) srep(label, syscall(__VA_ARGS__))

/* A new user namespace (the caller is its root) and IPC namespace: a table that starts empty. */
static inline void fresh_ns(void) {
    char text[32];
    unsigned uid = geteuid(), gid = getegid();
    if (unshare(CLONE_NEWUSER | CLONE_NEWIPC)) {
        printf("fresh namespace: %s\n", en(errno));
        exit(9);
    }
    snprintf(text, sizeof text, "0 %u 1", uid);
    int fd = open("/proc/self/uid_map", O_WRONLY);
    if (fd >= 0) { write(fd, text, strlen(text)); close(fd); }
    fd = open("/proc/self/setgroups", O_WRONLY);
    if (fd >= 0) { write(fd, "deny", 4); close(fd); }
    snprintf(text, sizeof text, "0 %u 1", gid);
    fd = open("/proc/self/gid_map", O_WRONLY);
    if (fd >= 0) { write(fd, text, strlen(text)); close(fd); }
}

/* Every capability gone: permissions by the mode bits alone. */
static inline void no_caps(void) {
    struct __user_cap_header_struct h = { _LINUX_CAPABILITY_VERSION_3, 0 };
    struct __user_cap_data_struct d[2];
    memset(d, 0, sizeof d);
    if (syscall(SYS_capset, &h, d))
        printf("capset: %s\n", en(errno));
}

/* Runs fn in a forked child and answers how it ended ("exit N" or "signal N"), the parent's table unchanged. */
static inline void in_child(const char *label, void (*fn)(void)) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        fn();
        fflush(stdout);
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    if (WIFEXITED(st))
        printf("%s: exit %d\n", label, WEXITSTATUS(st));
    else
        printf("%s: signal %d\n", label, WTERMSIG(st));
    fflush(stdout);
}

/* The words of `line` sorted: the threads a wake-up released log in the order the scheduler runs them. */
static inline void sorted_words(char *line) {
    char *w[32], copy[512];
    int n = 0;
    strncpy(copy, line, sizeof copy - 1);
    copy[sizeof copy - 1] = 0;
    for (char *t = strtok(copy, " "); t && n < 32; t = strtok(0, " "))
        w[n++] = t;
    for (int i = 1; i < n; i++)
        for (int j = i; j > 0 && strcmp(w[j - 1], w[j]) > 0; j--) {
            char *x = w[j]; w[j] = w[j - 1]; w[j - 1] = x;
        }
    line[0] = 0;
    for (int i = 0; i < n; i++) {
        strcat(line, w[i]);
        strcat(line, " ");
    }
}

static inline void msleep(int ms) {
    struct timespec t = { ms / 1000, ms % 1000 * 1000000L };
    nanosleep(&t, 0);
}

/* Waits until the thread sleeps (state S in /proc), so the order of waiters does not depend on how busy the machine
 * is. Twice with a pause, so a short sleep on the way into the call does not count. */
static inline void wait_asleep(int tid) {
    char path[64], b[512];
    snprintf(path, sizeof path, "/proc/self/task/%d/stat", tid);
    int seen = 0;
    for (int i = 0; i < 20000 && seen < 2; i++) {
        if (rd(path, b, sizeof b) <= 0)
            return;
        char *p = strrchr(b, ')');
        if (p && p[2] == 'S') {
            if (++seen < 2)
                msleep(5);
        } else {
            seen = 0;
            msleep(1);
        }
    }
}

struct launch { void *(*fn)(void *); void *arg; volatile int tid; };

static inline void *launch_main(void *a) {
    struct launch *l = a;
    l->tid = syscall(SYS_gettid);
    return l->fn(l->arg);
}

/* A thread that has gone to sleep in its call when this returns. */
static inline void go(pthread_t *t, void *(*fn)(void *), void *arg) {
    struct launch *l = calloc(1, sizeof *l);
    l->fn = fn;
    l->arg = arg;
    pthread_create(t, 0, launch_main, l);
    while (!l->tid)
        msleep(1);
    wait_asleep(l->tid);
}

/* The log of finished threads, printed once `n` of them are in it (sorted: released threads race for the lock). */
static pthread_mutex_t log_lock = PTHREAD_MUTEX_INITIALIZER;
static char logline[512];

static inline void flush_log(const char *label, int n) {
    for (int i = 0; i < 10000; i++) {
        pthread_mutex_lock(&log_lock);
        int words = 0;
        for (char *c = logline; *c; c++)
            words += *c == ' ';
        pthread_mutex_unlock(&log_lock);
        if (words >= n)
            break;
        msleep(1);
    }
    msleep(20); /* One too many would show here. */
    pthread_mutex_lock(&log_lock);
    sorted_words(logline);
    printf("%s: %s\n", label, logline);
    logline[0] = 0;
    pthread_mutex_unlock(&log_lock);
    fflush(stdout);
}
#endif
