/* Helpers for the ABI diff programs (AB-3 harness; programs: UL). Every line a program prints must be the
 * same on CachyOS and on V2 when the kernels behave the same: print results, errno names and yes/no
 * relations, never pids, addresses, times or host-specific counts. */
#ifndef ABI_H
#define ABI_H
#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* A kernel-half address: every syscall must answer EFAULT for it and never write there. */
#define KERNEL_ADDR ((void *)0xffff888000000000ul)

static inline const char *ename(int e) {
    static char other[24];
    switch (e) {
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case ESRCH: return "ESRCH";
    case EINTR: return "EINTR";
    case EBADF: return "EBADF";
    case ECHILD: return "ECHILD";
    case EAGAIN: return "EAGAIN";
    case ENOMEM: return "ENOMEM";
    case EACCES: return "EACCES";
    case EFAULT: return "EFAULT";
    case EEXIST: return "EEXIST";
    case ENOTDIR: return "ENOTDIR";
    case EINVAL: return "EINVAL";
    case EMFILE: return "EMFILE";
    case ENOTTY: return "ENOTTY";
    case ESPIPE: return "ESPIPE";
    case ERANGE: return "ERANGE";
    case ENOSYS: return "ENOSYS";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case ENODEV: return "ENODEV";
    case E2BIG: return "E2BIG";
    case ENOEXEC: return "ENOEXEC";
    case ELOOP: return "ELOOP";
    case ETXTBSY: return "ETXTBSY";
    case ENAMETOOLONG: return "ENAMETOOLONG";
    }
    snprintf(other, sizeof other, "errno %d", e);
    return other;
}

/* "label = <result>" or "label = -<ERRNO>" for a raw syscall result (musl's syscall() sets errno). */
static inline long show(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, ename(errno));
    else
        printf("%s = %ld\n", label, r);
    return r;
}
#define SYS(label, ...) show(label, syscall(__VA_ARGS__))

static inline void yes(const char *label, int cond) {
    printf("%s: %s\n", label, cond ? "yes" : "no");
}

static inline uint64_t ns_of(clockid_t c) {
    struct timespec t;
    clock_gettime(c, &t);
    return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}

#endif
