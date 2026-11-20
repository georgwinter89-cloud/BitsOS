/* L7-NET: socket behaviour where Linux 7.3 answers differently from Linux 6.6 (plan/spec/net.md §10.3).
 *   opts   - the receive options by family (linux-7.3.1/net/core/sock.c:1560-1586, :1882-1900; af_unix.c:932)
 *   rights - SO_PASSRIGHTS 0 refuses SCM_RIGHTS with EPERM (af_unix.c:2245, :2485), and a connection takes its
 *            listener's receive options (af_unix.c:1750)
 *   inq    - SO_INQ puts SCM_INQ after a stream read (af_unix.c:3110)
 *   pidfd  - SO_PASSPIDFD gives SCM_PIDFD, the sender's pidfd (net/core/scm.c:489)
 *   rtnl   - RTM_NEWLINK and RTM_NEWADDR in a new network namespace (rtnetlink.c:4045, devinet.c:960); run
 *            unprivileged inside unshare(CLONE_NEWUSER|CLONE_NEWNET)
 *   gcbench N M - timing of the unix collector's triggers (prints times: never in cases.txt)
 * Prints results, errno names and yes/no relations only. */
#include "ns.h"
#include <arpa/inet.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <poll.h>
#include <stddef.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/un.h>

#ifndef SO_PASSSEC
#define SO_PASSSEC 34
#endif
#ifndef SO_PASSPIDFD
#define SO_PASSPIDFD 76
#endif
#ifndef SO_PASSRIGHTS
#define SO_PASSRIGHTS 83
#endif
#ifndef SO_INQ
#define SO_INQ 84
#endif
#ifndef SCM_INQ
#define SCM_INQ SO_INQ
#endif
#ifndef SO_RIGHTS_NOTRUNC
#define SO_RIGHTS_NOTRUNC 85
#endif
#ifndef SCM_PIDFD
#define SCM_PIDFD 4
#endif
#ifndef IFA_PROTO
#define IFA_PROTO 11
#endif

static long seti(int fd, int name, int v) {
    return setsockopt(fd, SOL_SOCKET, name, &v, sizeof v);
}

/* getsockopt as "= value" or "= -ERRNO". */
static void geti(const char *label, int fd, int name) {
    int v = -1;
    socklen_t l = sizeof v;
    if (getsockopt(fd, SOL_SOCKET, name, &v, &l) < 0)
        printf("%s = -%s\n", label, en(errno));
    else
        printf("%s = %d\n", label, v);
}

static const struct { const char *name; int opt; } OPTS[] = {
    { "SO_PASSCRED", SO_PASSCRED }, { "SO_PASSSEC", SO_PASSSEC }, { "SO_PASSPIDFD", SO_PASSPIDFD },
    { "SO_PASSRIGHTS", SO_PASSRIGHTS }, { "SO_INQ", SO_INQ }, { "SO_RIGHTS_NOTRUNC", SO_RIGHTS_NOTRUNC },
};

static void opts_of(const char *what, int fd) {
    char l[96];
    for (unsigned i = 0; i < sizeof OPTS / sizeof OPTS[0]; i++) {
        snprintf(l, sizeof l, "%s get %s", what, OPTS[i].name);
        geti(l, fd, OPTS[i].opt);
        snprintf(l, sizeof l, "%s set %s 1", what, OPTS[i].name);
        R(l, seti(fd, OPTS[i].opt, 1));
        snprintf(l, sizeof l, "%s get %s after", what, OPTS[i].name);
        geti(l, fd, OPTS[i].opt);
    }
}

static int opts(void) {
    int udp = socket(AF_INET, SOCK_DGRAM, 0), tcp = socket(AF_INET, SOCK_STREAM, 0);
    int nl = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    int sv[2], dv[2], qv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    socketpair(AF_UNIX, SOCK_DGRAM, 0, dv);
    socketpair(AF_UNIX, SOCK_SEQPACKET, 0, qv);
    opts_of("udp", udp);
    opts_of("tcp", tcp);
    opts_of("netlink", nl);
    opts_of("unix stream", sv[0]);
    opts_of("unix dgram", dv[0]);
    opts_of("unix seqpacket", qv[0]);
    /* AF_UNIX's own options: exactly an int, 0 or 1 (af_unix.c:944-966). */
    long two = 2;
    R("stream SO_INQ 2", seti(sv[1], SO_INQ, 2));
    R("stream SO_INQ -1", seti(sv[1], SO_INQ, -1));
    R("stream SO_INQ 0", seti(sv[1], SO_INQ, 0));
    R("stream SO_INQ len 8", setsockopt(sv[1], SOL_SOCKET, SO_INQ, &two, 8));
    R("stream SO_INQ len 2", setsockopt(sv[1], SOL_SOCKET, SO_INQ, &two, 2));
    R("dgram SO_INQ len 2", setsockopt(dv[1], SOL_SOCKET, SO_INQ, &two, 2));
    R("stream SO_INQ bad pointer", setsockopt(sv[1], SOL_SOCKET, SO_INQ, KERNEL_ADDR, 4));
    R("dgram SO_RIGHTS_NOTRUNC 2", seti(dv[1], SO_RIGHTS_NOTRUNC, 2));
    R("dgram SO_RIGHTS_NOTRUNC len 8", setsockopt(dv[1], SOL_SOCKET, SO_RIGHTS_NOTRUNC, &two, 8));
    R("udp SO_INQ len 2", setsockopt(udp, SOL_SOCKET, SO_INQ, &two, 2));
    R("unix level SOL_IP", setsockopt(sv[1], SOL_IP, SO_INQ, &two, 4));
    R("udp SO_PASSCRED 0", seti(udp, SO_PASSCRED, 0));
    R("netlink SO_PASSPIDFD 0", seti(nl, SO_PASSPIDFD, 0));
    return 0;
}

/* sendmsg of one byte with `fd` in SCM_RIGHTS (fd < 0: no control data). */
static long send_fd(int s, int fd) {
    char c = 'F';
    struct iovec iov = { &c, 1 };
    union { struct cmsghdr h; char b[CMSG_SPACE(sizeof(int))]; } u;
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1 };
    if (fd >= 0) {
        memset(&u, 0, sizeof u);
        m.msg_control = u.b;
        m.msg_controllen = sizeof u.b;
        struct cmsghdr *h = CMSG_FIRSTHDR(&m);
        h->cmsg_level = SOL_SOCKET;
        h->cmsg_type = SCM_RIGHTS;
        h->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(h), &fd, sizeof fd);
    }
    return sendmsg(s, &m, MSG_DONTWAIT);
}

/* recvmsg into `n` bytes with `ctl` bytes of control space: prints the result, MSG_CTRUNC and each control
 * message's level/type (and SCM_INQ's value, whether an SCM_PIDFD/SCM_RIGHTS fd is open and close-on-exec). */
static void show_recv(const char *label, int s, size_t n, size_t ctl, int flags) {
    char buf[256], cb[512];
    struct iovec iov = { buf, n };
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = ctl ? cb : 0, .msg_controllen = ctl };
    memset(cb, 0, sizeof cb);
    long r = recvmsg(s, &m, flags | MSG_DONTWAIT);
    if (r < 0) {
        printf("%s = -%s\n", label, en(errno));
        return;
    }
    printf("%s = %ld ctrunc %s\n", label, r, yn(m.msg_flags & MSG_CTRUNC));
    for (struct cmsghdr *h = CMSG_FIRSTHDR(&m); h; h = CMSG_NXTHDR(&m, h)) {
        printf("  cmsg level %d type %d len %zu", h->cmsg_level, h->cmsg_type, (size_t)h->cmsg_len);
        int v;
        memcpy(&v, CMSG_DATA(h), sizeof v);
        if (h->cmsg_type == SCM_INQ)
            printf(" inq %d", v);
        if (h->cmsg_type == SCM_PIDFD || h->cmsg_type == SCM_RIGHTS) {
            if (v < 0) {
                printf(" error -%s", en(-v));
            } else {
                int fl = fcntl(v, F_GETFD);
                printf(" fd open %s cloexec %s", yn(fl >= 0), yn(fl >= 0 && (fl & FD_CLOEXEC)));
                close(v);
            }
        }
        printf("\n");
    }
}

static int rights(void) {
    static const int types[] = { SOCK_DGRAM, SOCK_SEQPACKET, SOCK_STREAM };
    static const char *names[] = { "dgram", "seqpacket", "stream" };
    int file = open("/", O_RDONLY | O_DIRECTORY);
    char l[96];
    for (int i = 0; i < 3; i++) {
        int sv[2];
        socketpair(AF_UNIX, types[i], 0, sv);
        snprintf(l, sizeof l, "%s SO_PASSRIGHTS 0", names[i]);
        R(l, seti(sv[1], SO_PASSRIGHTS, 0));
        snprintf(l, sizeof l, "%s send with a file", names[i]);
        R(l, send_fd(sv[0], file));
        snprintf(l, sizeof l, "%s send without", names[i]);
        R(l, send_fd(sv[0], -1));
        snprintf(l, sizeof l, "%s SO_PASSRIGHTS 1", names[i]);
        R(l, seti(sv[1], SO_PASSRIGHTS, 1));
        snprintf(l, sizeof l, "%s send with a file again", names[i]);
        R(l, send_fd(sv[0], file));
        snprintf(l, sizeof l, "%s recv", names[i]);
        show_recv(l, sv[1], 1, 64, 0);
        show_recv(l, sv[1], 1, 64, 0);
        close(sv[0]);
        close(sv[1]);
    }
    /* A listener's options reach a connection as it is made, a later change does not. */
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    snprintf(a.sun_path + 1, sizeof a.sun_path - 1, "l7net-%d", getpid());
    socklen_t al = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(a.sun_path + 1);
    int lst = socket(AF_UNIX, SOCK_STREAM, 0);
    R("bind", bind(lst, (void *)&a, al));
    R("listen", listen(lst, 4));
    R("listener SO_PASSRIGHTS 0", seti(lst, SO_PASSRIGHTS, 0));
    R("listener SO_PASSCRED 1", seti(lst, SO_PASSCRED, 1));
    R("listener SO_INQ 1", seti(lst, SO_INQ, 1));
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    R("connect", connect(c, (void *)&a, al));
    R("listener SO_PASSCRED 0", seti(lst, SO_PASSCRED, 0));
    R("send a file before accept", send_fd(c, file));
    R("send without before accept", send_fd(c, -1));
    int s = accept(lst, 0, 0);
    geti("accepted SO_PASSRIGHTS", s, SO_PASSRIGHTS);
    geti("accepted SO_PASSCRED", s, SO_PASSCRED);
    show_recv("accepted recv", s, 8, 128, 0);
    return 0;
}

static int inq(void) {
    int sv[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    R("SO_INQ 1", seti(sv[1], SO_INQ, 1));
    R("write hello", write(sv[0], "hello", 5));
    R("write world", write(sv[0], "world", 5));
    show_recv("recv 3", sv[1], 3, 64, 0);
    show_recv("recv 3 no control", sv[1], 3, 0, 0);
    show_recv("peek all", sv[1], 64, 64, MSG_PEEK);
    show_recv("recv all", sv[1], 64, 64, 0);
    show_recv("recv empty", sv[1], 64, 64, 0);
    R("shutdown", shutdown(sv[0], SHUT_WR));
    show_recv("recv after shutdown", sv[1], 64, 64, 0);
    R("SO_INQ 0", seti(sv[1], SO_INQ, 0));
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    R("write x", write(sv[0], "x", 1));
    show_recv("recv without SO_INQ", sv[1], 64, 64, 0);
    return 0;
}

static int pidfd(void) {
    int dv[2], sv[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, dv);
    R("SO_PASSPIDFD 1", seti(dv[1], SO_PASSPIDFD, 1));
    R("send", write(dv[0], "x", 1));
    show_recv("recv", dv[1], 4, 64, 0);
    R("send", write(dv[0], "x", 1));
    show_recv("recv room for less than an int", dv[1], 4, CMSG_LEN(sizeof(int)) - 1, 0);
    R("send", write(dv[0], "x", 1));
    show_recv("recv no control", dv[1], 4, 0, 0);
    R("SO_PASSCRED 1", seti(dv[1], SO_PASSCRED, 1));
    R("send", write(dv[0], "x", 1));
    show_recv("recv with SO_PASSCRED", dv[1], 4, 256, 0);
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    R("stream SO_PASSPIDFD 1", seti(sv[1], SO_PASSPIDFD, 1));
    R("stream send", write(sv[0], "ab", 2));
    show_recv("stream recv", sv[1], 4, 64, 0);
    /* The sender ends and is reaped before the receive: the message held it from the send on, so the pidfd
     * names that process, now gone (PIDFD_STALE; signal 0 ESRCH, poll readable), not whoever has its number. */
    socketpair(AF_UNIX, SOCK_DGRAM, 0, dv);
    R("reaped sender: SO_PASSPIDFD 1", seti(dv[1], SO_PASSPIDFD, 1));
    pid_t c = fork();
    if (c == 0) {
        _exit(write(dv[0], "x", 1) == 1 ? 0 : 1);
    }
    int st = 0;
    R("reaped sender: waitpid", waitpid(c, &st, 0) == c ? 0 : -1);
    char buf[4], cb[64];
    struct iovec iov = { buf, sizeof buf };
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cb, .msg_controllen = sizeof cb };
    long r = recvmsg(dv[1], &m, MSG_DONTWAIT);
    struct cmsghdr *h = r == 1 ? CMSG_FIRSTHDR(&m) : 0;
    int fd = -1;
    if (h && h->cmsg_type == SCM_PIDFD)
        memcpy(&fd, CMSG_DATA(h), sizeof fd);
    printf("reaped sender: recv = %ld, pidfd %s\n", r, fd >= 0 ? "yes" : "no");
    if (fd >= 0) {
        printf("reaped sender: signal 0 = %s\n", syscall(SYS_pidfd_send_signal, fd, 0, 0, 0) < 0 ? en(errno) : "0");
        struct pollfd pf = { .fd = fd, .events = POLLIN };
        printf("reaped sender: poll readable %s\n", yn(poll(&pf, 1, 0) == 1 && (pf.revents & POLLIN)));
        close(fd);
    }
    /* SO_PASSSEC: no label without a security module; without control space MSG_CTRUNC. */
    socketpair(AF_UNIX, SOCK_DGRAM, 0, dv);
    R("SO_PASSSEC 1", seti(dv[1], SO_PASSSEC, 1));
    R("send", write(dv[0], "x", 1));
    show_recv("passsec recv", dv[1], 4, 64, 0);
    R("send", write(dv[0], "x", 1));
    show_recv("passsec recv no control", dv[1], 4, 0, 0);
    return 0;
}

/* ---- rtnetlink in a new network namespace ---- */

static int nl;
static unsigned seq;

static int nl_ack(void) {
    char buf[8192];
    for (;;) {
        long n = recv(nl, buf, sizeof buf, 0);
        if (n <= 0)
            return 1;
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n))
            if (h->nlmsg_type == NLMSG_ERROR && h->nlmsg_seq == seq)
                return ((struct nlmsgerr *)NLMSG_DATA(h))->error;
    }
}

static void put_attr(struct nlmsghdr *h, int type, const void *d, int len) {
    struct rtattr *a = (void *)((char *)h + NLMSG_ALIGN(h->nlmsg_len));
    a->rta_type = type;
    a->rta_len = RTA_LENGTH(len);
    memcpy(RTA_DATA(a), d, len);
    h->nlmsg_len = NLMSG_ALIGN(h->nlmsg_len) + RTA_ALIGN(a->rta_len);
}

static void ack_line(const char *label) {
    int e = nl_ack();
    if (e < 0)
        printf("%s = -%s\n", label, en(-e));
    else
        printf("%s = %d\n", label, e);
}

static void link_req(const char *label, int type, int flags, int index, const char *name) {
    struct { struct nlmsghdr h; struct ifinfomsg m; char attrs[64]; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = NLMSG_LENGTH(sizeof q.m);
    q.h.nlmsg_type = type;
    q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    q.h.nlmsg_seq = ++seq;
    q.m.ifi_index = index;
    if (name)
        put_attr(&q.h, IFLA_IFNAME, name, strlen(name) + 1);
    send(nl, &q, q.h.nlmsg_len, 0);
    ack_line(label);
}

/* A link request with IFLA_LINKINFO { IFLA_INFO_KIND `kind`, and an empty IFLA_INFO_DATA when `data` }. The kind
 * is one no kernel knows, so the host's answer never depends on which link modules it has (rtnl_newlink then has
 * no ops, as V2 never has). */
static void kind_req(const char *label, int type, int flags, int index, const char *name, int data) {
    struct { struct nlmsghdr h; struct ifinfomsg m; char attrs[128]; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = NLMSG_LENGTH(sizeof q.m);
    q.h.nlmsg_type = type;
    q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    q.h.nlmsg_seq = ++seq;
    q.m.ifi_index = index;
    if (name)
        put_attr(&q.h, IFLA_IFNAME, name, strlen(name) + 1);
    char nest[64];
    struct rtattr *a = (void *)nest;
    const char *kind = "bitsosnokind";
    a->rta_type = IFLA_INFO_KIND;
    a->rta_len = RTA_LENGTH(strlen(kind) + 1);
    memcpy(RTA_DATA(a), kind, strlen(kind) + 1);
    int len = RTA_ALIGN(a->rta_len);
    if (data) {
        struct rtattr *d = (void *)(nest + len);
        d->rta_type = IFLA_INFO_DATA;
        d->rta_len = RTA_LENGTH(0);
        len += RTA_ALIGN(d->rta_len);
    }
    put_attr(&q.h, IFLA_LINKINFO, nest, len);
    send(nl, &q, q.h.nlmsg_len, 0);
    ack_line(label);
}

/* RTM_NEWADDR of `ip`/`prefix` on `index`; proto < 0: no IFA_PROTO; valid 0: no IFA_CACHEINFO. */
static void addr_req(const char *label, int flags, int index, const char *ip, int prefix, int proto, unsigned valid) {
    struct { struct nlmsghdr h; struct ifaddrmsg m; char attrs[96]; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = NLMSG_LENGTH(sizeof q.m);
    q.h.nlmsg_type = RTM_NEWADDR;
    q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    q.h.nlmsg_seq = ++seq;
    q.m.ifa_family = AF_INET;
    q.m.ifa_prefixlen = prefix;
    q.m.ifa_index = index;
    struct in_addr a;
    inet_pton(AF_INET, ip, &a);
    put_attr(&q.h, IFA_LOCAL, &a, 4);
    put_attr(&q.h, IFA_ADDRESS, &a, 4);
    if (proto >= 0) {
        unsigned char p = proto;
        put_attr(&q.h, IFA_PROTO, &p, 1);
    }
    if (valid) {
        struct ifa_cacheinfo ci = { .ifa_prefered = valid == 1 ? 5 : valid, .ifa_valid = valid == 1 ? 0 : valid };
        put_attr(&q.h, IFA_CACHEINFO, &ci, sizeof ci);
    }
    send(nl, &q, q.h.nlmsg_len, 0);
    ack_line(label);
}

/* The IPv4 addresses with their IFA_PROTO (-1: none). */
static void dump(const char *label) {
    struct { struct nlmsghdr h; struct ifaddrmsg m; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = sizeof q;
    q.h.nlmsg_type = RTM_GETADDR;
    q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    q.h.nlmsg_seq = ++seq;
    q.m.ifa_family = AF_INET;
    send(nl, &q, sizeof q, 0);
    printf("%s:\n", label);
    char buf[16384];
    for (;;) {
        long n = recv(nl, buf, sizeof buf, 0);
        if (n <= 0)
            return;
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            if (h->nlmsg_type == NLMSG_DONE || h->nlmsg_type == NLMSG_ERROR)
                return;
            if (h->nlmsg_type != RTM_NEWADDR)
                continue;
            struct ifaddrmsg *m = NLMSG_DATA(h);
            char a[64] = "-";
            int proto = -1;
            int len = IFA_PAYLOAD(h);
            for (struct rtattr *r = IFA_RTA(m); RTA_OK(r, len); r = RTA_NEXT(r, len)) {
                if (r->rta_type == IFA_ADDRESS)
                    inet_ntop(AF_INET, RTA_DATA(r), a, sizeof a);
                if (r->rta_type == IFA_PROTO)
                    proto = *(unsigned char *)RTA_DATA(r);
            }
            printf("  %s/%d proto %d\n", a, m->ifa_prefixlen, proto);
        }
    }
}

/* How many RTM_NEWADDR notifications wait on `s`, and whether each carries the request's sequence number. */
static void heard(const char *label, int s, unsigned want_seq) {
    char buf[16384];
    int n_new = 0, seq_ok = 1;
    for (;;) {
        long n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
        if (n <= 0)
            break;
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n))
            if (h->nlmsg_type == RTM_NEWADDR) {
                n_new++;
                seq_ok &= h->nlmsg_seq == want_seq;
            }
    }
    printf("%s: %d RTM_NEWADDR, request's seq %s\n", label, n_new, yn(seq_ok));
}

static int rtnl(void) {
    if (R("unshare(CLONE_NEWUSER|CLONE_NEWNET)", unshare(CLONE_NEWUSER | CLONE_NEWNET)) < 0)
        return 0;
    nl = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct timeval tv = { .tv_sec = 2 };
    setsockopt(nl, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    int mon = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct sockaddr_nl k = { .nl_family = AF_NETLINK, .nl_groups = RTMGRP_IPV4_IFADDR };
    R("bind monitor", bind(mon, (void *)&k, sizeof k));
    int lo = if_nametoindex("lo");

    link_req("NEWLINK index -1", RTM_NEWLINK, 0, -1, 0);
    link_req("NEWLINK index -1 with name lo", RTM_NEWLINK, 0, -1, "lo");
    link_req("NEWLINK index 77", RTM_NEWLINK, 0, 77, 0);
    link_req("NEWLINK index 77 CREATE", RTM_NEWLINK, NLM_F_CREATE, 77, 0);
    link_req("NEWLINK name nope CREATE", RTM_NEWLINK, NLM_F_CREATE, 0, "nope");
    link_req("NEWLINK name nope", RTM_NEWLINK, 0, 0, "nope");
    link_req("NEWLINK nothing", RTM_NEWLINK, 0, 0, 0);
    link_req("NEWLINK lo CREATE|EXCL", RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, lo, 0);
    link_req("NEWLINK lo CREATE", RTM_NEWLINK, NLM_F_CREATE, lo, 0);
    link_req("SETLINK index -1", RTM_SETLINK, 0, -1, 0);
    link_req("SETLINK index -1 with name lo", RTM_SETLINK, 0, -1, "lo");
    /* `ip link add NAME type KIND` always sends IFLA_LINKINFO: the index, the device and NLM_F_EXCL come first. */
    kind_req("NEWLINK index -1 CREATE|EXCL with a kind", RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, -1, 0, 0);
    kind_req("NEWLINK lo CREATE|EXCL with a kind", RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, lo, 0, 1);
    kind_req("NEWLINK name lo CREATE|EXCL with a kind", RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 0, "lo", 0);
    kind_req("NEWLINK name nope CREATE|EXCL with a kind", RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 0, "nope", 0);
    kind_req("NEWLINK name nope with a kind", RTM_NEWLINK, 0, 0, "nope", 1);
    kind_req("NEWLINK lo with kind data", RTM_NEWLINK, 0, lo, 0, 1);
    kind_req("NEWLINK lo with a kind, no data", RTM_NEWLINK, 0, lo, 0, 0);
    kind_req("SETLINK lo with kind data", RTM_SETLINK, 0, lo, 0, 1);

    addr_req("NEWADDR bad lifetime, index 77", NLM_F_CREATE, 77, "192.0.2.1", 24, -1, 1);
    addr_req("NEWADDR prefix 33, index 77", NLM_F_CREATE, 77, "192.0.2.1", 33, -1, 0);
    addr_req("NEWADDR 0.0.0.0, index 77", NLM_F_CREATE, 77, "0.0.0.0", 24, -1, 0);
    addr_req("NEWADDR index 77", NLM_F_CREATE, 77, "192.0.2.1", 24, -1, 0);
    addr_req("NEWADDR proto 4", NLM_F_CREATE, lo, "192.0.2.1", 24, 4, 0);
    heard("after add", mon, seq);
    dump("dump after add");
    addr_req("NEWADDR again", NLM_F_CREATE, lo, "192.0.2.1", 24, 4, 0);
    addr_req("NEWADDR replace proto 7", NLM_F_REPLACE, lo, "192.0.2.1", 24, 7, 0);
    heard("after replace", mon, seq);
    addr_req("NEWADDR replace proto 7 again", NLM_F_REPLACE, lo, "192.0.2.1", 24, 7, 0);
    heard("after the same replace", mon, seq);
    dump("dump after replace");
    addr_req("NEWADDR replace without proto", NLM_F_REPLACE, lo, "192.0.2.1", 24, -1, 0);
    heard("after replace without proto", mon, seq);
    dump("dump after replace without proto");
    addr_req("NEWADDR replace EXCL", NLM_F_REPLACE | NLM_F_EXCL, lo, "192.0.2.1", 24, 7, 0);
    addr_req("NEWADDR 0.0.0.0 on lo", NLM_F_CREATE, lo, "0.0.0.0", 8, -1, 0);
    /* lo's MTU below 68 takes IPv4 away: 0.0.0.0 is still 0, an address ENOBUFS. */
    struct { struct nlmsghdr h; struct ifinfomsg m; char attrs[16]; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = NLMSG_LENGTH(sizeof q.m);
    q.h.nlmsg_type = RTM_SETLINK;
    q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    q.h.nlmsg_seq = ++seq;
    q.m.ifi_index = lo;
    unsigned mtu = 60;
    put_attr(&q.h, IFLA_MTU, &mtu, 4);
    send(nl, &q, q.h.nlmsg_len, 0);
    ack_line("SETLINK mtu 60");
    addr_req("NEWADDR 0.0.0.0 without IPv4", NLM_F_CREATE, lo, "0.0.0.0", 8, -1, 0);
    addr_req("NEWADDR without IPv4", NLM_F_CREATE, lo, "192.0.2.5", 24, -1, 0);
    return 0;
}

/* ---- timing (not for cases.txt) ---- */

static double ms_since(uint64_t t0) {
    return (ns_of(CLOCK_MONOTONIC) - t0) / 1e6;
}

/* `inflight` unix sockets in flight in one receiver's queue (no cycle: the receiver itself is not in flight),
 * then `rounds` socketpairs made and closed (each close is a release that may run the collector). With
 * `cycle`, one more socket travels in its own queue while its fd stays open (a live cycle). */
static void release_round(int inflight, int rounds, int cycle) {
    int rv[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, rv);
    int big = 1 << 22;
    setsockopt(rv[1], SOL_SOCKET, SO_RCVBUF, &big, sizeof big);
    setsockopt(rv[0], SOL_SOCKET, SO_SNDBUF, &big, sizeof big);
    for (int i = 0; i < inflight; i++) {
        int p[2];
        socketpair(AF_UNIX, SOCK_DGRAM, 0, p);
        if (send_fd(rv[0], p[0]) != 1) {
            printf("send %d failed: %s\n", i, en(errno));
            break;
        }
        close(p[0]);
        close(p[1]);
    }
    int self[2] = { -1, -1 };
    if (cycle) {
        socketpair(AF_UNIX, SOCK_DGRAM, 0, self);
        send_fd(self[0], self[1]);
    }
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    for (int i = 0; i < rounds; i++) {
        int p[2];
        socketpair(AF_UNIX, SOCK_DGRAM, 0, p);
        close(p[0]);
        close(p[1]);
    }
    printf("release: %d in flight%s, %d pairs closed: %.2f ms\n", inflight, cycle ? " + a live cycle" : "", rounds, ms_since(t0));
    close(rv[0]);
    close(rv[1]);
    if (cycle) {
        close(self[0]);
        close(self[1]);
    }
}

/* `n` sockets sent in messages of one each by one user (every send has sockets, so Linux 7's per-user trigger
 * applies past 2024 in flight), with or without a live cycle. */
static void send_round(int n, int cycle) {
    int self[2] = { -1, -1 };
    if (cycle) {
        socketpair(AF_UNIX, SOCK_DGRAM, 0, self);
        send_fd(self[0], self[1]);
    }
    int rv[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, rv);
    int big = 1 << 22;
    setsockopt(rv[1], SOL_SOCKET, SO_RCVBUF, &big, sizeof big);
    setsockopt(rv[0], SOL_SOCKET, SO_SNDBUF, &big, sizeof big);
    int p[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, p);
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    int sent = 0;
    for (int i = 0; i < n; i++)
        sent += send_fd(rv[0], p[0]) == 1;
    printf("send: %d of %d sockets sent%s: %.2f ms\n", sent, n, cycle ? " with a live cycle" : "", ms_since(t0));
    close(p[0]);
    close(p[1]);
    close(rv[0]);
    close(rv[1]);
    if (cycle) {
        close(self[0]);
        close(self[1]);
    }
}

static int gcbench(int n, int m) {
    struct rlimit rl = { 65536, 65536 };
    setrlimit(RLIMIT_NOFILE, &rl);
    for (int rep = 0; rep < 3; rep++) {
        release_round(n, m, 0);
        release_round(n, m, 1);
        send_round(n, 0);
        send_round(n, 1);
    }
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    const char *mode = argc > 1 ? argv[1] : "opts";
    if (!strcmp(mode, "opts"))
        return opts();
    if (!strcmp(mode, "rights"))
        return rights();
    if (!strcmp(mode, "inq"))
        return inq();
    if (!strcmp(mode, "pidfd"))
        return pidfd();
    if (!strcmp(mode, "rtnl"))
        return rtnl();
    if (!strcmp(mode, "gcbench"))
        return gcbench(argc > 2 ? atoi(argv[2]) : 3000, argc > 3 ? atoi(argv[3]) : 2000);
    printf("unknown mode %s\n", mode);
    return 2;
}
