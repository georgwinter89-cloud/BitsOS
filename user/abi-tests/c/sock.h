/* Helpers for the socket programs of the ABI diff (NW-5): errno names beyond abi.h's, and printing. Every
 * line must be the same on CachyOS and V2: no pids, ports chosen by the kernel, addresses or times. */
#ifndef SOCK_H
#define SOCK_H
#include "abi.h"
#include <fcntl.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <stddef.h>
#include <sys/ioctl.h>

static inline const char *sename(int e) {
    switch (e) {
    case ENOTSOCK: return "ENOTSOCK";
    case EADDRINUSE: return "EADDRINUSE";
    case EADDRNOTAVAIL: return "EADDRNOTAVAIL";
    case ECONNREFUSED: return "ECONNREFUSED";
    case ECONNRESET: return "ECONNRESET";
    case ECONNABORTED: return "ECONNABORTED";
    case ENOTCONN: return "ENOTCONN";
    case EISCONN: return "EISCONN";
    case EPIPE: return "EPIPE";
    case EPROTOTYPE: return "EPROTOTYPE";
    case EAFNOSUPPORT: return "EAFNOSUPPORT";
    case ESOCKTNOSUPPORT: return "ESOCKTNOSUPPORT";
    case EPROTONOSUPPORT: return "EPROTONOSUPPORT";
    case EINPROGRESS: return "EINPROGRESS";
    case EALREADY: return "EALREADY";
    case EDESTADDRREQ: return "EDESTADDRREQ";
    case EMSGSIZE: return "EMSGSIZE";
    case ENOPROTOOPT: return "ENOPROTOOPT";
    case EDOM: return "EDOM";
    case ETOOMANYREFS: return "ETOOMANYREFS";
    case ENETUNREACH: return "ENETUNREACH";
    case EHOSTUNREACH: return "EHOSTUNREACH";
    case ETIMEDOUT: return "ETIMEDOUT";
    case ENOBUFS: return "ENOBUFS";
    case ENXIO: return "ENXIO";
    case ENAMETOOLONG: return "ENAMETOOLONG";
    case ENOSPC: return "ENOSPC";
    case EROFS: return "EROFS";
    case ENODATA: return "ENODATA";
    }
    return ename(e);
}

/* "label = <result>" or "label = -<ERRNO>" for a libc call returning -1 on failure. */
static inline long res(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, sename(errno));
    else
        printf("%s = %ld\n", label, r);
    fflush(stdout);
    return r;
}
#define R(label, expr) res(label, (long)(expr))

/* poll revents of one fd, without waiting. */
static inline void revents(const char *label, int fd, short events) {
    struct pollfd p = { fd, events, 0 };
    int r = poll(&p, 1, 0);
    printf("%s: poll %d revents %#x\n", label, r, (unsigned short)p.revents);
    fflush(stdout);
}

static inline int sopt(int fd, int level, int name) {
    int v = -12345;
    socklen_t l = sizeof v;
    if (getsockopt(fd, level, name, &v, &l) < 0)
        return -100000 - errno;
    return v;
}
#endif
