/* NW-6: AF_NETLINK NETLINK_ROUTE (link and address dumps, getifaddrs) and the SIOCGIF* interface ioctls against
 * Linux. argv[1] picks a part. Only the loopback interface's values are printed (index 1, "lo", 127.0.0.1/8 on
 * every Linux); other interfaces are checked by relations (well-formed, consistent), never by name or address,
 * and IPv6 entries (CachyOS has ::1, V2 has no IPv6) are skipped. Port ids are printed as relations only. */
#include "sock.h"
#include <ifaddrs.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <netpacket/packet.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/if_link.h>
#include <linux/if_addr.h>
#include <linux/sockios.h>
#include <linux/ethtool.h>

static const char *nename(int e) {
    switch (e) {
    case EBUSY: return "EBUSY";
    case EPFNOSUPPORT: return "EPFNOSUPPORT";
    case ERANGE: return "ERANGE";
    }
    return sename(e);
}

/* "label = <result>" with the netlink errno names. */
static long nres(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, nename(errno));
    else
        printf("%s = %ld\n", label, r);
    fflush(stdout);
    return r;
}
#define N(label, expr) nres(label, (long)(expr))

static int nl_open(int type) {
    return socket(AF_NETLINK, type | SOCK_CLOEXEC, NETLINK_ROUTE);
}

static unsigned own_pid(int fd) {
    struct sockaddr_nl a = { 0 };
    socklen_t l = sizeof a;
    getsockname(fd, (struct sockaddr *)&a, &l);
    return a.nl_pid;
}

/* One request: header + payload (+ attributes already in the payload). */
static long nl_req(int fd, int type, int flags, int seq, const void *pl, int len) {
    char b[512] = { 0 };
    struct nlmsghdr *h = (struct nlmsghdr *)b;
    h->nlmsg_len = NLMSG_LENGTH(len);
    h->nlmsg_type = type;
    h->nlmsg_flags = flags;
    h->nlmsg_seq = seq;
    memcpy(NLMSG_DATA(h), pl, len);
    return send(fd, b, NLMSG_ALIGN(h->nlmsg_len), 0);
}

static long nl_dump(int fd, int type, int family, int seq) {
    struct rtgenmsg g = { .rtgen_family = family };
    return nl_req(fd, type, NLM_F_REQUEST | NLM_F_DUMP, seq, &g, sizeof g);
}

static char rbuf[65536];

/* One datagram (non-blocking): its length, or -errno. */
static long nl_recv(int fd, int flags) {
    long r = recv(fd, rbuf, sizeof rbuf, flags | MSG_DONTWAIT);
    return r < 0 ? -errno : r;
}

static const char *tname(int t) {
    static char b[16];
    switch (t) {
    case NLMSG_NOOP: return "NOOP";
    case NLMSG_ERROR: return "ERROR";
    case NLMSG_DONE: return "DONE";
    case RTM_NEWLINK: return "NEWLINK";
    case RTM_NEWADDR: return "NEWADDR";
    case RTM_DELADDR: return "DELADDR";
    }
    snprintf(b, sizeof b, "type %d", t);
    return b;
}

/* Prints each message of the datagram in rbuf (length n): type, length, flags and, for ERROR and DONE, the
 * error; for others only the type. `who` is the receiving socket's port id (checked, not printed). */
static void show_msgs(const char *label, long n, unsigned who, int seq) {
    if (n < 0) {
        printf("%s: -%s\n", label, nename(-n));
        return;
    }
    int len = n, sized = 1;
    for (struct nlmsghdr *h = (struct nlmsghdr *)rbuf; NLMSG_OK(h, len); h = NLMSG_NEXT(h, len))
        if (h->nlmsg_type != NLMSG_ERROR && h->nlmsg_type != NLMSG_DONE)
            sized = 0;
    /* Link and address messages differ in size between kernels (attributes come and go): no lengths. */
    if (sized)
        printf("%s: %ld bytes\n", label, n);
    else
        printf("%s:\n", label);
    len = n;
    for (struct nlmsghdr *h = (struct nlmsghdr *)rbuf; NLMSG_OK(h, len); h = NLMSG_NEXT(h, len)) {
        printf("  %s", tname(h->nlmsg_type));
        if (sized)
            printf(" len %u", h->nlmsg_len);
        printf(" flags %#x seq-ok %s pid-ok %s", h->nlmsg_flags, (int)h->nlmsg_seq == seq ? "yes" : "no", h->nlmsg_pid == who ? "yes" : "no");
        if (h->nlmsg_type == NLMSG_ERROR) {
            struct nlmsgerr *e = NLMSG_DATA(h);
            printf(" error %s request-type %d", e->error ? nename(-e->error) : "0", e->msg.nlmsg_type);
        } else if (h->nlmsg_type == NLMSG_DONE) {
            int e = *(int *)NLMSG_DATA(h);
            printf(" done %s", e ? nename(-e) : "0");
        } else if (h->nlmsg_type == RTM_NEWLINK) {
            struct ifinfomsg *ifi = NLMSG_DATA(h);
            printf(" index %d", ifi->ifi_index);
        }
        printf("\n");
    }
    fflush(stdout);
}

/* Reads the answers of one dump to its DONE (non-blocking reads; Linux answers within the send). Returns the
 * number of `want` messages, or -1 without a DONE. *done_err gets DONE's error; *dgrams the datagram count;
 * *alone whether DONE came in a datagram of its own. */
struct dump {
    int msgs, dgrams, done_err, done_alone, done_flags, msg_flags, ok_seq, ok_pid;
};

static int read_dump(int fd, int want, int seq, unsigned who, struct dump *d, void (*each)(struct nlmsghdr *)) {
    memset(d, 0, sizeof *d);
    d->ok_seq = d->ok_pid = 1;
    for (;;) {
        long n = nl_recv(fd, 0);
        if (n <= 0)
            return -1;
        d->dgrams++;
        int len = n, inthis = 0;
        for (struct nlmsghdr *h = (struct nlmsghdr *)rbuf; NLMSG_OK(h, len); h = NLMSG_NEXT(h, len)) {
            inthis++;
            if ((int)h->nlmsg_seq != seq)
                d->ok_seq = 0;
            if (h->nlmsg_pid != who)
                d->ok_pid = 0;
            if (h->nlmsg_type == NLMSG_DONE) {
                d->done_err = *(int *)NLMSG_DATA(h);
                d->done_alone = inthis == 1 && NLMSG_ALIGN(h->nlmsg_len) >= (unsigned)len;
                d->done_flags = h->nlmsg_flags;
                return d->msgs;
            }
            if (h->nlmsg_type == want) {
                d->msgs++;
                d->msg_flags |= h->nlmsg_flags;
                if (each)
                    each(h);
            }
        }
    }
}

static void print_dump(const char *label, int r, struct dump *d) {
    printf("%s: %s, DONE error %s alone %s flags %#x, messages flags %#x, seq %s, pid %s\n", label,
        r < 0 ? "no DONE" : r > 0 ? "messages" : "no messages", d->done_err ? nename(-d->done_err) : "0",
        d->done_alone ? "yes" : "no", d->done_flags, d->msg_flags, d->ok_seq ? "ok" : "wrong", d->ok_pid ? "ok" : "wrong");
    fflush(stdout);
}

/* ---- part "sock": the socket itself ---------------------------------------------------------------------- */

static void print_name(const char *label, int fd, int peer, unsigned pid_ref) {
    struct sockaddr_nl a;
    memset(&a, 0x55, sizeof a);
    socklen_t l = 64;
    long r = (peer ? getpeername : getsockname)(fd, (struct sockaddr *)&a, &l);
    if (r < 0) {
        printf("%s: -%s\n", label, nename(errno));
        return;
    }
    printf("%s: len %u family %d pad %d pid %s groups %#x\n", label, l, a.nl_family, a.nl_pad,
        a.nl_pid == 0 ? "0" : a.nl_pid == pid_ref ? "own" : (int)a.nl_pid < 0 ? "negative" : "other", a.nl_groups);
}

static void sock_part(void) {
    N("socket(STREAM)", socket(AF_NETLINK, SOCK_STREAM, NETLINK_ROUTE));
    N("socket(SEQPACKET)", socket(AF_NETLINK, SOCK_SEQPACKET, NETLINK_ROUTE));
    N("socket(RAW, protocol 32)", socket(AF_NETLINK, SOCK_RAW, 32));
    N("socket(RAW, protocol -1)", socket(AF_NETLINK, SOCK_RAW, -1));
    int d = socket(AF_NETLINK, SOCK_DGRAM, NETLINK_ROUTE);
    yes("socket(DGRAM, ROUTE) works", d >= 0);
    close(d);
    int s = nl_open(SOCK_RAW);
    yes("socket(RAW, ROUTE) works", s >= 0);
    printf("SO_TYPE %d SO_PROTOCOL %d SO_DOMAIN %d SO_RCVBUF %d SO_SNDBUF %d SO_ERROR %d SO_PASSCRED %d\n",
        sopt(s, SOL_SOCKET, SO_TYPE), sopt(s, SOL_SOCKET, SO_PROTOCOL), sopt(s, SOL_SOCKET, SO_DOMAIN),
        sopt(s, SOL_SOCKET, SO_RCVBUF), sopt(s, SOL_SOCKET, SO_SNDBUF), sopt(s, SOL_SOCKET, SO_ERROR), sopt(s, SOL_SOCKET, SO_PASSCRED));
    revents("fresh socket", s, POLLIN | POLLOUT);
    unsigned me = getpid();
    print_name("getsockname fresh", s, 0, me);
    print_name("getpeername fresh", s, 1, me);
    struct sockaddr_nl a = { .nl_family = AF_NETLINK };
    N("bind len 11", bind(s, (struct sockaddr *)&a, 11));
    a.nl_family = AF_INET;
    N("bind family AF_INET", bind(s, (struct sockaddr *)&a, sizeof a));
    a.nl_family = AF_NETLINK;
    N("bind pid 0", bind(s, (struct sockaddr *)&a, sizeof a));
    print_name("getsockname after bind", s, 0, me);
    N("bind pid 0 again", bind(s, (struct sockaddr *)&a, sizeof a));
    a.nl_pid = own_pid(s);
    a.nl_groups = RTMGRP_LINK;
    N("bind own pid with RTMGRP_LINK", bind(s, (struct sockaddr *)&a, sizeof a));
    print_name("getsockname with a group", s, 0, me);
    int t = nl_open(SOCK_RAW);
    N("bind another socket to the same pid", bind(t, (struct sockaddr *)&a, sizeof a));
    struct sockaddr_nl k = { .nl_family = AF_NETLINK };
    N("connect the other to the kernel", connect(t, (struct sockaddr *)&k, sizeof k));
    print_name("the other's name (autobind)", t, 0, me);
    print_name("the other's peer", t, 1, me);
    N("connect len 1", connect(t, (struct sockaddr *)&k, 1));
    N("connect AF_UNSPEC", connect(t, (struct sockaddr *)&(struct sockaddr){ .sa_family = AF_UNSPEC }, sizeof(struct sockaddr)));
    k.nl_family = AF_INET;
    N("connect family AF_INET", connect(t, (struct sockaddr *)&k, sizeof k));
    k.nl_family = AF_NETLINK;
    N("connect len 11", connect(t, (struct sockaddr *)&k, 11));
    k.nl_pid = own_pid(s);
    N("connect to a user socket (not root)", connect(t, (struct sockaddr *)&k, sizeof k));
    N("sendto a user socket (not root)", sendto(t, "x", 1, 0, (struct sockaddr *)&k, sizeof k));
    k.nl_pid = 0;
    k.nl_groups = RTMGRP_LINK;
    N("sendto a group (not root)", sendto(t, "x", 1, 0, (struct sockaddr *)&k, sizeof k));
    k.nl_groups = 0;
    N("sendto len-11 address", sendto(t, "x", 1, 0, (struct sockaddr *)&k, 11));
    close(t);

    /* Options of SOL_NETLINK. */
    int v = 0;
    N("ADD_MEMBERSHIP 0", setsockopt(s, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &v, sizeof v));
    v = 1000;
    N("ADD_MEMBERSHIP 1000", setsockopt(s, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &v, sizeof v));
    v = RTNLGRP_IPV4_IFADDR;
    N("ADD_MEMBERSHIP IPV4_IFADDR", setsockopt(s, SOL_NETLINK, NETLINK_ADD_MEMBERSHIP, &v, sizeof v));
    print_name("getsockname with two groups", s, 0, me);
    unsigned w[4] = { 0 };
    socklen_t wl = sizeof w;
    N("LIST_MEMBERSHIPS", getsockopt(s, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, w, &wl));
    printf("  length %u word0 %#x\n", wl, w[0]);
    wl = 2;
    N("LIST_MEMBERSHIPS into 2 bytes", getsockopt(s, SOL_NETLINK, NETLINK_LIST_MEMBERSHIPS, w, &wl));
    printf("  length %u\n", wl);
    v = RTNLGRP_LINK;
    N("DROP_MEMBERSHIP LINK", setsockopt(s, SOL_NETLINK, NETLINK_DROP_MEMBERSHIP, &v, sizeof v));
    print_name("getsockname after the drop", s, 0, me);
    int opts[] = { NETLINK_PKTINFO, NETLINK_BROADCAST_ERROR, NETLINK_NO_ENOBUFS, NETLINK_CAP_ACK, NETLINK_EXT_ACK, NETLINK_GET_STRICT_CHK };
    const char *on[] = { "PKTINFO", "BROADCAST_ERROR", "NO_ENOBUFS", "CAP_ACK", "EXT_ACK", "GET_STRICT_CHK" };
    for (int i = 0; i < 6; i++) {
        int before = sopt(s, SOL_NETLINK, opts[i]);
        v = 7;
        long r = setsockopt(s, SOL_NETLINK, opts[i], &v, sizeof v);
        printf("%s: default %d, set 7 -> %ld, now %d", on[i], before, r, sopt(s, SOL_NETLINK, opts[i]));
        v = 0;
        setsockopt(s, SOL_NETLINK, opts[i], &v, sizeof v);
        printf(", off %d\n", sopt(s, SOL_NETLINK, opts[i]));
    }
    N("setsockopt with optlen 2 (value read as 0)", setsockopt(s, SOL_NETLINK, NETLINK_EXT_ACK, &v, 2));
    socklen_t sl = 2;
    N("getsockopt EXT_ACK into 2 bytes", getsockopt(s, SOL_NETLINK, NETLINK_EXT_ACK, &v, &sl));
    sl = -1;
    N("getsockopt negative length", getsockopt(s, SOL_NETLINK, NETLINK_EXT_ACK, &v, &sl));
    sl = 4;
    N("getsockopt option 99", getsockopt(s, SOL_NETLINK, 99, &v, &sl));
    N("setsockopt option 99", setsockopt(s, SOL_NETLINK, 99, &v, 4));
    N("getsockopt SOL_IP", getsockopt(s, SOL_IP, 1, &v, &sl));
    N("setsockopt SOL_IP", setsockopt(s, SOL_IP, 1, &v, 4));
    N("setsockopt LISTEN_ALL_NSID (not root)", setsockopt(s, SOL_NETLINK, NETLINK_LISTEN_ALL_NSID, &(int){ 1 }, 4));

    /* Calls netlink does not have. */
    int n = 0;
    N("ioctl(FIONREAD)", ioctl(s, FIONREAD, &n));
    N("shutdown", shutdown(s, SHUT_RDWR));
    N("listen", listen(s, 1));
    N("accept", accept(s, 0, 0));
    int sv[2];
    N("socketpair(AF_NETLINK)", socketpair(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE, sv));
    N("send 0 bytes", send(s, "", 0, 0));
    N("send MSG_OOB", send(s, "xxxxxxxxxxxxxxxxxxxx", 20, MSG_OOB));
    N("recv MSG_OOB", recv(s, rbuf, 16, MSG_OOB));
    N("recv empty, non-blocking", recv(s, rbuf, 16, MSG_DONTWAIT));
    static char big[300000];
    N("send a too large message", send(s, big, sizeof big, 0));
    close(s);
}

/* ---- part "msgs": the request protocol -------------------------------------------------------------------- */

static void msgs_part(void) {
    int s = nl_open(SOCK_RAW);
    long r = nl_dump(s, RTM_GETLINK, AF_UNSPEC, 1);
    printf("send GETLINK dump = %ld\n", r);
    unsigned me = own_pid(s);
    yes("send bound the socket (autobind)", me != 0);
    struct dump d;
    int links = read_dump(s, RTM_NEWLINK, 1, me, &d, 0);
    print_dump("GETLINK dump", links, &d);
    yes("at least one link", links >= 1);

    /* A datagram too small for the reader: MSG_TRUNC, the rest of that datagram is gone. */
    nl_dump(s, RTM_GETLINK, AF_UNSPEC, 2);
    struct msghdr m = { 0 };
    struct iovec io = { rbuf, 16 };
    m.msg_iov = &io;
    m.msg_iovlen = 1;
    long t = recvmsg(s, &m, MSG_DONTWAIT);
    printf("recvmsg into 16 bytes = %ld, MSG_TRUNC %s\n", t, m.msg_flags & MSG_TRUNC ? "yes" : "no");
    int sawdone = 0;
    for (int i = 0; i < 50 && !sawdone; i++) {
        long n = nl_recv(s, 0);
        if (n <= 0)
            break;
        int len = n;
        for (struct nlmsghdr *h = (struct nlmsghdr *)rbuf; NLMSG_OK(h, len); h = NLMSG_NEXT(h, len))
            if (h->nlmsg_type == NLMSG_DONE)
                sawdone = 1;
    }
    yes("the dump still ends with DONE", sawdone);
    nl_dump(s, RTM_GETLINK, AF_UNSPEC, 3);
    long peek = recv(s, rbuf, sizeof rbuf, MSG_PEEK | MSG_DONTWAIT);
    long full = recv(s, rbuf, 8, MSG_TRUNC | MSG_DONTWAIT);
    yes("MSG_PEEK then MSG_TRUNC into 8 bytes give the same length", peek == full && peek > 8);
    read_dump(s, RTM_NEWLINK, 3, me, &d, 0);

    /* Requests that are not requests, control messages, broken headers. */
    struct ifinfomsg ifi = { 0 };
    nl_req(s, RTM_GETLINK, 0, 10, &ifi, sizeof ifi);
    show_msgs("GETLINK without NLM_F_REQUEST", nl_recv(s, 0), me, 10);
    nl_req(s, RTM_GETLINK, NLM_F_ACK, 11, &ifi, sizeof ifi);
    show_msgs("GETLINK with only NLM_F_ACK", nl_recv(s, 0), me, 11);
    nl_req(s, NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 12, &ifi, sizeof ifi);
    show_msgs("NOOP with NLM_F_ACK", nl_recv(s, 0), me, 12);
    nl_req(s, NLMSG_NOOP, NLM_F_REQUEST, 13, &ifi, 0);
    show_msgs("NOOP without NLM_F_ACK", nl_recv(s, 0), me, 13);
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK | NLM_F_DUMP, 14, &ifi, 0);
    show_msgs("GETLINK dump without payload", nl_recv(s, 0), me, 14);
    nl_req(s, 0x7fff, NLM_F_REQUEST | NLM_F_ACK, 15, &ifi, sizeof ifi);
    show_msgs("type 0x7fff", nl_recv(s, 0), me, 15);
    nl_req(s, RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, 16, &ifi, sizeof ifi);
    show_msgs("NEWLINK (not root)", nl_recv(s, 0), me, 16);
    nl_req(s, RTM_DELADDR, NLM_F_REQUEST, 17, &ifi, sizeof ifi);
    show_msgs("DELADDR without NLM_F_ACK (not root)", nl_recv(s, 0), me, 17);
    char raw[64] = { 0 };
    struct nlmsghdr *h = (struct nlmsghdr *)raw;
    h->nlmsg_len = 8;
    h->nlmsg_type = RTM_GETLINK;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    N("send nlmsg_len 8", send(s, raw, 16, 0));
    show_msgs("answer to nlmsg_len 8", nl_recv(s, 0), me, 0);
    h->nlmsg_len = 60;
    N("send nlmsg_len 60 in 32 bytes", send(s, raw, 32, 0));
    show_msgs("answer to nlmsg_len 60", nl_recv(s, 0), me, 0);
    N("send 10 bytes", send(s, raw, 10, 0));
    show_msgs("answer to 10 bytes", nl_recv(s, 0), me, 0);

    /* Two requests in one datagram: answered in order. */
    char two[128] = { 0 };
    struct nlmsghdr *a = (struct nlmsghdr *)two;
    a->nlmsg_len = NLMSG_LENGTH(sizeof ifi);
    a->nlmsg_type = RTM_GETLINK;
    a->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    a->nlmsg_seq = 20;
    ((struct ifinfomsg *)NLMSG_DATA(a))->ifi_index = 1;
    struct nlmsghdr *b = (struct nlmsghdr *)(two + NLMSG_ALIGN(a->nlmsg_len));
    b->nlmsg_len = NLMSG_LENGTH(4);
    b->nlmsg_type = NLMSG_NOOP;
    b->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    b->nlmsg_seq = 20;
    N("send two requests", send(s, two, NLMSG_ALIGN(a->nlmsg_len) + b->nlmsg_len, 0));
    for (int i = 0; i < 3; i++)
        show_msgs("answer", nl_recv(s, 0), me, 20);

    /* GETLINK of one interface: by index, by name, unknown. */
    ifi.ifi_index = 9999;
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST, 21, &ifi, sizeof ifi);
    show_msgs("GETLINK index 9999", nl_recv(s, 0), me, 21);
    ifi.ifi_index = 0;
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST, 22, &ifi, sizeof ifi);
    show_msgs("GETLINK index 0 without a name", nl_recv(s, 0), me, 22);
    char nm[64] = { 0 };
    memcpy(nm, &ifi, sizeof ifi);
    struct rtattr *rta = (struct rtattr *)(nm + NLMSG_ALIGN(sizeof ifi));
    rta->rta_type = IFLA_IFNAME;
    rta->rta_len = RTA_LENGTH(3);
    memcpy(RTA_DATA(rta), "lo", 3);
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST, 23, nm, NLMSG_ALIGN(sizeof ifi) + RTA_ALIGN(rta->rta_len));
    show_msgs("GETLINK by name lo", nl_recv(s, 0), me, 23);
    memcpy(RTA_DATA(rta), "zz", 3);
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST, 24, nm, NLMSG_ALIGN(sizeof ifi) + RTA_ALIGN(rta->rta_len));
    show_msgs("GETLINK by name zz", nl_recv(s, 0), me, 24);

    /* A second dump while the first one is still running. */
    nl_dump(s, RTM_GETLINK, AF_UNSPEC, 30);
    nl_dump(s, RTM_GETADDR, AF_UNSPEC, 31);
    int busy = 0, done = 0, after_first = 0, n = 0;
    for (int i = 0; i < 50 && !done; i++) {
        long len = nl_recv(s, 0);
        if (len <= 0)
            break;
        n++;
        int l = len;
        for (struct nlmsghdr *x = (struct nlmsghdr *)rbuf; NLMSG_OK(x, l); x = NLMSG_NEXT(x, l)) {
            if (x->nlmsg_type == NLMSG_ERROR && ((struct nlmsgerr *)NLMSG_DATA(x))->error == -EBUSY && x->nlmsg_seq == 31) {
                busy = 1;
                after_first = n == 2;
            }
            if (x->nlmsg_type == NLMSG_DONE && x->nlmsg_seq == 30)
                done = 1;
        }
    }
    yes("second dump: EBUSY", busy);
    yes("EBUSY right after the first datagram", after_first);
    yes("first dump ends with DONE", done);

    /* Acks: capped with NETLINK_CAP_ACK, the request copied without. */
    int one = 1;
    setsockopt(s, SOL_NETLINK, NETLINK_CAP_ACK, &one, sizeof one);
    nl_req(s, RTM_NEWLINK, NLM_F_REQUEST, 40, &ifi, sizeof ifi);
    show_msgs("NEWLINK with CAP_ACK", nl_recv(s, 0), me, 40);
    one = 0;
    setsockopt(s, SOL_NETLINK, NETLINK_CAP_ACK, &one, sizeof one);

    /* recvmsg: the sender's address and NETLINK_PKTINFO. */
    one = 1;
    setsockopt(s, SOL_NETLINK, NETLINK_PKTINFO, &one, sizeof one);
    nl_req(s, NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 41, &ifi, 0);
    struct sockaddr_nl from;
    memset(&from, 0x55, sizeof from);
    char ctl[64];
    memset(&m, 0, sizeof m);
    io.iov_base = rbuf;
    io.iov_len = sizeof rbuf;
    m.msg_iov = &io;
    m.msg_iovlen = 1;
    m.msg_name = &from;
    m.msg_namelen = 64;
    m.msg_control = ctl;
    m.msg_controllen = sizeof ctl;
    long got = recvmsg(s, &m, MSG_DONTWAIT);
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    printf("recvmsg = %ld namelen %u family %d pid %u groups %#x controllen %zu", got, m.msg_namelen, from.nl_family, from.nl_pid, from.nl_groups, (size_t)m.msg_controllen);
    if (c)
        printf(" cmsg level %d type %d len %zu group %u", c->cmsg_level, c->cmsg_type, (size_t)c->cmsg_len, *(unsigned *)CMSG_DATA(c));
    printf("\n");
    one = 0;
    setsockopt(s, SOL_NETLINK, NETLINK_PKTINFO, &one, sizeof one);
    one = 1;
    setsockopt(s, SOL_SOCKET, SO_PASSCRED, &one, sizeof one);
    nl_req(s, NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 42, &ifi, 0);
    m.msg_controllen = sizeof ctl;
    got = recvmsg(s, &m, MSG_DONTWAIT);
    c = CMSG_FIRSTHDR(&m);
    printf("recvmsg with SO_PASSCRED = %ld", got);
    if (c) {
        struct ucred *u = (struct ucred *)CMSG_DATA(c);
        printf(" cmsg level %d type %d len %zu pid %d uid %d gid %d", c->cmsg_level, c->cmsg_type, (size_t)c->cmsg_len, u->pid, u->uid, u->gid);
    }
    printf("\n");
    /* read(2) and write(2) work like recv and send. */
    struct { struct nlmsghdr h; struct rtgenmsg g; } q = { { NLMSG_LENGTH(1), NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 43, 0 }, { 0 } };
    N("write a request", write(s, &q, sizeof q));
    N("read its ack", read(s, rbuf, sizeof rbuf));
    revents("after reading everything", s, POLLIN | POLLOUT);
    nl_req(s, NLMSG_NOOP, NLM_F_REQUEST | NLM_F_ACK, 44, &ifi, 0);
    revents("with an answer queued", s, POLLIN | POLLOUT);
    close(s);

    /* Strict checking (NETLINK_GET_STRICT_CHK): headers are checked, the address dump can filter by index. */
    s = nl_open(SOCK_RAW);
    one = 1;
    setsockopt(s, SOL_NETLINK, NETLINK_GET_STRICT_CHK, &one, sizeof one);
    me = (nl_dump(s, RTM_GETLINK, AF_UNSPEC, 50), own_pid(s));
    r = read_dump(s, RTM_NEWLINK, 50, me, &d, 0);
    print_dump("strict GETLINK with a 1-byte header", r, &d);
    struct ifinfomsg li = { 0 };
    li.ifi_index = 1;
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 51, &li, sizeof li);
    r = read_dump(s, RTM_NEWLINK, 51, me, &d, 0);
    print_dump("strict GETLINK with an index", r, &d);
    li.ifi_index = 0;
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 52, &li, sizeof li);
    r = read_dump(s, RTM_NEWLINK, 52, me, &d, 0);
    print_dump("strict GETLINK", r, &d);
    yes("as many links as without strict checking", r == links);
    struct ifaddrmsg ia = { .ifa_family = AF_INET, .ifa_index = 1 };
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 53, &ia, sizeof ia);
    r = read_dump(s, RTM_NEWADDR, 53, me, &d, 0);
    print_dump("strict GETADDR AF_INET index 1", r, &d);
    printf("  addresses: %ld\n", r);
    ia.ifa_index = 9999;
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 54, &ia, sizeof ia);
    r = read_dump(s, RTM_NEWADDR, 54, me, &d, 0);
    print_dump("strict GETADDR index 9999", r, &d);
    ia.ifa_index = 0;
    ia.ifa_prefixlen = 8;
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 55, &ia, sizeof ia);
    r = read_dump(s, RTM_NEWADDR, 55, me, &d, 0);
    print_dump("strict GETADDR with a prefix length", r, &d);
    close(s);
}

/* ---- part "link": the loopback link and the shape of the others ---------------------------------------- */

static int ether_ok = 1, lo_seen;

static void link_each(struct nlmsghdr *h) {
    struct ifinfomsg *ifi = NLMSG_DATA(h);
    int len = IFLA_PAYLOAD(h);
    if (ifi->ifi_index == 1) {
        lo_seen = 1;
        printf("lo: family %d type %d index %d flags %#x change %#x\n", ifi->ifi_family, ifi->ifi_type, ifi->ifi_index, ifi->ifi_flags, ifi->ifi_change);
        /* In attribute-number order: kernels put them in different orders. */
        struct rtattr *at[IFLA_MAX + 1] = { 0 };
        for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len); a = RTA_NEXT(a, len))
            if (a->rta_type <= IFLA_MAX)
                at[a->rta_type] = a;
        static const struct { int t; const char *n; char kind; } want[] = {
            { IFLA_ADDRESS, "ADDRESS", 'm' }, { IFLA_BROADCAST, "BROADCAST", 'm' }, { IFLA_IFNAME, "IFNAME", 's' },
            { IFLA_MTU, "MTU", 'u' }, { IFLA_QDISC, "QDISC", 's' }, { IFLA_STATS, "STATS", 'l' }, { IFLA_TXQLEN, "TXQLEN", 'u' },
            { IFLA_MAP, "MAP", 'z' }, { IFLA_OPERSTATE, "OPERSTATE", 'b' }, { IFLA_LINKMODE, "LINKMODE", 'b' },
            { IFLA_STATS64, "STATS64", 'l' }, { IFLA_GROUP, "GROUP", 'u' }, { IFLA_PROMISCUITY, "PROMISCUITY", 'u' },
            { IFLA_NUM_TX_QUEUES, "NUM_TX_QUEUES", 'u' }, { IFLA_NUM_RX_QUEUES, "NUM_RX_QUEUES", 'u' }, { IFLA_CARRIER, "CARRIER", 'b' },
            { IFLA_PROTO_DOWN, "PROTO_DOWN", 'b' }, { IFLA_GSO_MAX_SEGS, "GSO_MAX_SEGS", 'u' }, { IFLA_GSO_MAX_SIZE, "GSO_MAX_SIZE", 'u' },
            { IFLA_MIN_MTU, "MIN_MTU", 'u' }, { IFLA_MAX_MTU, "MAX_MTU", 'u' }, { IFLA_GRO_MAX_SIZE, "GRO_MAX_SIZE", 'u' },
            { IFLA_TSO_MAX_SIZE, "TSO_MAX_SIZE", 'u' }, { IFLA_TSO_MAX_SEGS, "TSO_MAX_SEGS", 'u' }, { IFLA_ALLMULTI, "ALLMULTI", 'u' },
            { IFLA_GSO_IPV4_MAX_SIZE, "GSO_IPV4_MAX_SIZE", 'u' }, { IFLA_GRO_IPV4_MAX_SIZE, "GRO_IPV4_MAX_SIZE", 'u' },
        };
        for (unsigned i = 0; i < sizeof want / sizeof want[0]; i++) {
            struct rtattr *a = at[want[i].t];
            if (!a) {
                printf("  %s missing\n", want[i].n);
                continue;
            }
            unsigned char *p = RTA_DATA(a);
            int pl = RTA_PAYLOAD(a);
            switch (want[i].kind) {
            case 's': printf("  %s %s\n", want[i].n, (char *)p); break;
            case 'u': printf("  %s %u\n", want[i].n, *(unsigned *)p); break;
            case 'b': printf("  %s %d\n", want[i].n, p[0]); break;
            case 'l': printf("  %s length %d\n", want[i].n, pl); break;
            case 'z': printf("  %s length %d all zero %s\n", want[i].n, pl, memcmp(p, (char[32]){ 0 }, 32) ? "no" : "yes"); break;
            case 'm': printf("  %s length %d %02x:%02x:%02x:%02x:%02x:%02x\n", want[i].n, pl, p[0], p[1], p[2], p[3], p[4], p[5]); break;
            }
        }
        return;
    }
    if (ifi->ifi_type != ARPHRD_ETHER)
        return;
    int addr = 0, brd = 0, name = 0, mtu = 0;
    for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        unsigned char *p = RTA_DATA(a);
        if (a->rta_type == IFLA_ADDRESS)
            addr = RTA_PAYLOAD(a) == 6;
        if (a->rta_type == IFLA_BROADCAST)
            brd = RTA_PAYLOAD(a) == 6 && !memcmp(p, "\xff\xff\xff\xff\xff\xff", 6);
        if (a->rta_type == IFLA_IFNAME)
            name = strlen((char *)p) > 0 && strlen((char *)p) < IFNAMSIZ;
        if (a->rta_type == IFLA_MTU)
            mtu = *(unsigned *)p >= 68;
    }
    int f = ifi->ifi_flags;
    int flags = (f & IFF_BROADCAST) && (f & IFF_MULTICAST) && !(f & IFF_LOOPBACK) && (!(f & IFF_LOWER_UP) || (f & IFF_UP));
    if (!(addr && brd && name && mtu && flags && ifi->ifi_family == AF_UNSPEC && ifi->ifi_index > 1))
        ether_ok = 0;
}

static void addr_each(struct nlmsghdr *h) {
    struct ifaddrmsg *ifa = NLMSG_DATA(h);
    if (ifa->ifa_family != AF_INET || ifa->ifa_index != 1)
        return;
    int len = IFA_PAYLOAD(h);
    printf("lo address: family %d prefixlen %d flags %#x scope %d index %d\n", ifa->ifa_family, ifa->ifa_prefixlen, ifa->ifa_flags, ifa->ifa_scope, ifa->ifa_index);
    struct rtattr *at[IFA_MAX + 1] = { 0 };
    for (struct rtattr *a = IFA_RTA(ifa); RTA_OK(a, len); a = RTA_NEXT(a, len))
        if (a->rta_type <= IFA_MAX)
            at[a->rta_type] = a;
    unsigned char *p;
    if ((p = at[IFA_ADDRESS] ? RTA_DATA(at[IFA_ADDRESS]) : 0))
        printf("  ADDRESS %d.%d.%d.%d\n", p[0], p[1], p[2], p[3]);
    if ((p = at[IFA_LOCAL] ? RTA_DATA(at[IFA_LOCAL]) : 0))
        printf("  LOCAL %d.%d.%d.%d\n", p[0], p[1], p[2], p[3]);
    if ((p = at[IFA_BROADCAST] ? RTA_DATA(at[IFA_BROADCAST]) : 0))
        printf("  BROADCAST %d.%d.%d.%d\n", p[0], p[1], p[2], p[3]);
    if ((p = at[IFA_LABEL] ? RTA_DATA(at[IFA_LABEL]) : 0))
        printf("  LABEL %s\n", (char *)p);
    if ((p = at[IFA_CACHEINFO] ? RTA_DATA(at[IFA_CACHEINFO]) : 0))
        printf("  CACHEINFO preferred %#x valid %#x\n", ((struct ifa_cacheinfo *)p)->ifa_prefered, ((struct ifa_cacheinfo *)p)->ifa_valid);
    if ((p = at[IFA_FLAGS] ? RTA_DATA(at[IFA_FLAGS]) : 0))
        printf("  FLAGS %#x\n", *(unsigned *)p);
    /* The address attributes come in this order (musl's getifaddrs reads ADDRESS before LOCAL). */
    int order = 0, seen = 0;
    len = IFA_PAYLOAD(h);
    for (struct rtattr *a = IFA_RTA(ifa); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        if (a->rta_type == IFA_ADDRESS && !seen)
            order = 1;
        if (a->rta_type == IFA_ADDRESS || a->rta_type == IFA_LOCAL)
            seen = 1;
    }
    yes("  ADDRESS before LOCAL", order);
}

static int addr_v4, addr_other, addr_links_ok;
static int known[256];

static void addr_count(struct nlmsghdr *h) {
    struct ifaddrmsg *ifa = NLMSG_DATA(h);
    if (ifa->ifa_family == AF_INET)
        addr_v4++;
    else
        addr_other++;
    if (ifa->ifa_index >= 256 || !known[ifa->ifa_index])
        addr_links_ok = 0;
}

static void note_link(struct nlmsghdr *h) {
    struct ifinfomsg *ifi = NLMSG_DATA(h);
    if (ifi->ifi_index > 0 && ifi->ifi_index < 256)
        known[ifi->ifi_index] = 1;
}

static void link_part(void) {
    int s = nl_open(SOCK_RAW);
    nl_dump(s, RTM_GETLINK, AF_UNSPEC, 1);
    unsigned me = own_pid(s);
    struct dump d;
    int n = read_dump(s, RTM_NEWLINK, 1, me, &d, link_each);
    print_dump("GETLINK", n, &d);
    yes("lo seen", lo_seen);
    yes("every Ethernet link well-formed", ether_ok);
    nl_dump(s, RTM_GETLINK, AF_INET, 2);
    int m = read_dump(s, RTM_NEWLINK, 2, me, &d, note_link);
    print_dump("GETLINK with family AF_INET", m, &d);
    yes("same links for AF_INET", m == n);
    nl_dump(s, RTM_GETADDR, AF_INET, 3);
    n = read_dump(s, RTM_NEWADDR, 3, me, &d, addr_each);
    print_dump("GETADDR AF_INET", n, &d);
    addr_links_ok = 1;
    nl_dump(s, RTM_GETADDR, AF_INET, 4);
    read_dump(s, RTM_NEWADDR, 4, me, &d, addr_count);
    yes("AF_INET dump: only AF_INET", addr_v4 > 0 && addr_other == 0);
    yes("every address on a dumped link", addr_links_ok);
    addr_v4 = addr_other = 0;
    nl_dump(s, RTM_GETADDR, AF_INET6, 5);
    n = read_dump(s, RTM_NEWADDR, 5, me, &d, addr_count);
    yes("GETADDR AF_INET6 ends with DONE", n >= 0 && d.done_err == 0);
    yes("AF_INET6 dump: no AF_INET", addr_v4 == 0);
    addr_v4 = addr_other = 0;
    nl_dump(s, RTM_GETADDR, AF_UNSPEC, 6);
    n = read_dump(s, RTM_NEWADDR, 6, me, &d, addr_count);
    print_dump("GETADDR AF_UNSPEC", n, &d);
    yes("AF_UNSPEC dump has the AF_INET ones", addr_v4 > 0);
    nl_dump(s, RTM_GETADDR, AF_PACKET, 7);
    n = read_dump(s, RTM_NEWADDR, 7, me, &d, 0);
    print_dump("GETADDR AF_PACKET (no such dump: all families)", n, &d);
    close(s);
}

/* ---- part "ifaddrs": what libc makes of it ------------------------------------------------------------------ */

static void ifaddrs_part(void) {
    struct ifaddrs *l;
    if (getifaddrs(&l) < 0) {
        printf("getifaddrs = -%s\n", nename(errno));
        return;
    }
    printf("getifaddrs = 0\n");
    int lo_packet = 0, lo_inet = 0, wellformed = 1;
    for (struct ifaddrs *i = l; i; i = i->ifa_next) {
        if (!i->ifa_name || !*i->ifa_name)
            wellformed = 0;
        if (strcmp(i->ifa_name, "lo") || !i->ifa_addr)
            continue;
        if (i->ifa_addr->sa_family == AF_PACKET && !lo_packet++) {
            struct sockaddr_ll *ll = (struct sockaddr_ll *)i->ifa_addr;
            printf("lo AF_PACKET: flags %#x ifindex %d hatype %d halen %d addr %02x%02x%02x%02x%02x%02x data %s\n", i->ifa_flags,
                ll->sll_ifindex, ll->sll_hatype, ll->sll_halen, ll->sll_addr[0], ll->sll_addr[1], ll->sll_addr[2], ll->sll_addr[3],
                ll->sll_addr[4], ll->sll_addr[5], i->ifa_data ? "yes" : "no");
        }
        if (i->ifa_addr->sa_family == AF_INET && !lo_inet++) {
            char a[32], nm[32];
            inet_ntop(AF_INET, &((struct sockaddr_in *)i->ifa_addr)->sin_addr, a, sizeof a);
            inet_ntop(AF_INET, &((struct sockaddr_in *)i->ifa_netmask)->sin_addr, nm, sizeof nm);
            printf("lo AF_INET: flags %#x addr %s netmask %s broadaddr %s\n", i->ifa_flags, a, nm, i->ifa_broadaddr ? "set" : "none");
        }
    }
    yes("every entry has a name", wellformed);
    yes("lo AF_PACKET once", lo_packet == 1);
    yes("lo AF_INET once", lo_inet == 1);
    freeifaddrs(l);
    struct if_nameindex *ni = if_nameindex();
    printf("if_nameindex first: %u %s\n", ni ? ni[0].if_index : 0, ni ? ni[0].if_name : "-");
    if (ni)
        if_freenameindex(ni);
}

/* ---- part "devbind": SO_BINDTODEVICE with interface names -------------------------------------------------- */

static void devbind_part(void) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    char nm[32];
    socklen_t l = sizeof nm;
    N("SO_BINDTODEVICE get, not bound", getsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, nm, &l));
    printf("  length %u\n", l);
    N("SO_BINDTODEVICE nope", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "nope", 5));
    N("SO_BINDTODEVICE lo", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "lo", 3));
    memset(nm, 0x55, sizeof nm);
    l = sizeof nm;
    N("SO_BINDTODEVICE get", getsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, nm, &l));
    printf("  length %u name %s byte3 %#x\n", l, nm, (unsigned char)nm[3]);
    l = 15;
    N("SO_BINDTODEVICE get into 15 bytes", getsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, nm, &l));
    N("SO_BINDTODEVICE lo, name without NUL (2 bytes)", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "lox", 2));
    N("SO_BINDTODEVICE \"\" (unbind, not root, while bound)", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "", 1));
    close(s);
    s = socket(AF_UNIX, SOCK_DGRAM, 0);
    N("SO_BINDTODEVICE lo on AF_UNIX", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "lo", 3));
    N("SO_BINDTODEVICE length 0 (unbind, not root, while bound)", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "lo", 0));
    close(s);
    s = nl_open(SOCK_RAW);
    N("SO_BINDTODEVICE lo on netlink", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "lo", 3));
    close(s);

    /* SO_BINDTOIFINDEX (`sock_bindtoindex_locked`): any index >= 0, read back as an int. */
#ifndef SO_BINDTOIFINDEX
#define SO_BINDTOIFINDEX 62
#endif
    s = socket(AF_INET, SOCK_DGRAM, 0);
    int ix = -7;
    l = sizeof ix;
    N("SO_BINDTOIFINDEX get, not bound", getsockopt(s, SOL_SOCKET, SO_BINDTOIFINDEX, &ix, &l));
    printf("  value %d length %u\n", ix, l);
    ix = -1;
    N("SO_BINDTOIFINDEX -1", setsockopt(s, SOL_SOCKET, SO_BINDTOIFINDEX, &ix, sizeof ix));
    N("SO_BINDTOIFINDEX with 2 bytes", setsockopt(s, SOL_SOCKET, SO_BINDTOIFINDEX, &ix, 2));
    ix = 99999;
    N("SO_BINDTOIFINDEX 99999 (no such interface)", setsockopt(s, SOL_SOCKET, SO_BINDTOIFINDEX, &ix, sizeof ix));
    ix = 0;
    l = sizeof ix;
    N("SO_BINDTOIFINDEX get", getsockopt(s, SOL_SOCKET, SO_BINDTOIFINDEX, &ix, &l));
    printf("  value %d length %u\n", ix, l);
    l = sizeof nm;
    N("SO_BINDTODEVICE get while bound to 99999", getsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, nm, &l));
    ix = 1;
    N("SO_BINDTOIFINDEX 1 (not root, while bound)", setsockopt(s, SOL_SOCKET, SO_BINDTOIFINDEX, &ix, sizeof ix));
    close(s);
    s = socket(AF_INET, SOCK_DGRAM, 0);
    N("SO_BINDTOIFINDEX 1", setsockopt(s, SOL_SOCKET, SO_BINDTOIFINDEX, &ix, sizeof ix));
    memset(nm, 0, sizeof nm);
    l = sizeof nm;
    N("SO_BINDTODEVICE get after SO_BINDTOIFINDEX 1", getsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, nm, &l));
    printf("  length %u name %s\n", l, nm);
    close(s);
}

/* ---- part "ioctl": SIOCGIF* and friends ------------------------------------------------------------------- */

static void ifr_show(const char *label, int fd, unsigned long cmd, const char *name, int index) {
    struct ifreq r;
    memset(&r, 0x55, sizeof r);
    memset(r.ifr_name, 0, sizeof r.ifr_name);
    if (name)
        strncpy(r.ifr_name, name, sizeof r.ifr_name);
    if (index)
        r.ifr_ifindex = index;
    if (cmd == SIOCGIFADDR || cmd == SIOCGIFNETMASK || cmd == SIOCGIFBRDADDR || cmd == SIOCGIFDSTADDR)
        r.ifr_addr.sa_family = 0x5555;
    long x = ioctl(fd, cmd, &r);
    if (x < 0) {
        printf("%s: -%s\n", label, nename(errno));
        return;
    }
    unsigned char *u = (unsigned char *)&r.ifr_ifru;
    printf("%s: name %.16s byte15 %#x", label, r.ifr_name, (unsigned char)r.ifr_name[15]);
    switch (cmd) {
    case SIOCGIFNAME: case SIOCGIFINDEX: case SIOCGIFMTU: case SIOCGIFMETRIC: case SIOCGIFTXQLEN:
        printf(" int %d", r.ifr_ifindex);
        break;
    case SIOCGIFFLAGS:
        printf(" flags %#x", (unsigned short)r.ifr_flags);
        break;
    case SIOCGIFHWADDR:
        printf(" family %d addr %02x%02x%02x%02x%02x%02x rest %#x", r.ifr_hwaddr.sa_family, u[2], u[3], u[4], u[5], u[6], u[7], u[8]);
        break;
    case SIOCGIFMAP:
        printf(" map %llx %llx %x %x %x %x tail %#x", (unsigned long long)r.ifr_map.mem_start, (unsigned long long)r.ifr_map.mem_end,
            r.ifr_map.base_addr, r.ifr_map.irq, r.ifr_map.dma, r.ifr_map.port, u[23]);
        break;
    default: {
        struct sockaddr_in *a = (struct sockaddr_in *)&r.ifr_addr;
        char b[32];
        inet_ntop(AF_INET, &a->sin_addr, b, sizeof b);
        printf(" family %d port %d addr %s zero %#x tail %#x", a->sin_family, a->sin_port, b, u[8], u[16]);
    }
    }
    printf("\n");
}

static void ioctl_part(void) {
    int in = socket(AF_INET, SOCK_DGRAM, 0), un = socket(AF_UNIX, SOCK_DGRAM, 0), nl = nl_open(SOCK_RAW), tcp = socket(AF_INET, SOCK_STREAM, 0);
    int fds[] = { in, un, nl, tcp };
    const char *fn[] = { "udp", "unix", "netlink", "tcp" };
    for (int i = 0; i < 4; i++) {
        char l[64];
        printf("-- on %s\n", fn[i]);
        int f = fds[i];
#define T(cmd, nm, ix) snprintf(l, sizeof l, "%s %s", #cmd, nm ? nm : "-"), ifr_show(l, f, cmd, nm, ix)
        T(SIOCGIFNAME, (char *)0, 1);
        T(SIOCGIFNAME, (char *)0, 9999);
        T(SIOCGIFINDEX, "lo", 0);
        T(SIOCGIFINDEX, "lo:7", 0);
        T(SIOCGIFINDEX, "nope", 0);
        T(SIOCGIFFLAGS, "lo", 0);
        T(SIOCGIFMTU, "lo", 0);
        T(SIOCGIFMETRIC, "lo", 0);
        T(SIOCGIFTXQLEN, "lo", 0);
        T(SIOCGIFHWADDR, "lo", 0);
        T(SIOCGIFHWADDR, "nope", 0);
        T(SIOCGIFMAP, "lo", 0);
        T(SIOCGIFSLAVE, "lo", 0);
        T(SIOCGIFSLAVE, "nope", 0);
        T(SIOCGIFADDR, "lo", 0);
        T(SIOCGIFADDR, "lo:7", 0);
        T(SIOCGIFADDR, "nope", 0);
        T(SIOCGIFNETMASK, "lo", 0);
        T(SIOCGIFBRDADDR, "lo", 0);
        T(SIOCGIFDSTADDR, "lo", 0);
        T(SIOCGIFPFLAGS, "lo", 0);
        T(SIOCGIFMEM, "lo", 0);
        T(SIOCGIFINDEX, "0123456789abcdefgh", 0);
        T(SIOCSIFFLAGS, "lo", 0);
        T(SIOCSIFMTU, "lo", 0);
        T(SIOCSIFADDR, "lo", 0);
        T(SIOCSIFNAME, "lo", 0);
        T(SIOCSIFTXQLEN, "lo", 0);
        T(SIOCADDMULTI, "lo", 0);
        T(SIOCGMIIPHY, "lo", 0);
        T(SIOCWANDEV, "lo", 0);
        T(SIOCWANDEV, "nope", 0);
        T(SIOCDEVPRIVATE, "lo", 0);
        T(SIOCDEVPRIVATE, "nope", 0);
        T(SIOCBONDINFOQUERY, "lo", 0);
        T(SIOCRTMSG, "lo", 0);
        T(SIOCGIFCOUNT, "lo", 0);
        T(0x89ff, "nope", 0);
        N("SIOCGIFINDEX to a kernel address", ioctl(f, SIOCGIFINDEX, KERNEL_ADDR));
        N("SIOCGIFADDR to a kernel address", ioctl(f, SIOCGIFADDR, KERNEL_ADDR));
        N("SIOCGIFMEM to a kernel address", ioctl(f, SIOCGIFMEM, KERNEL_ADDR));
        N("SIOCGIFCONF to a kernel address", ioctl(f, SIOCGIFCONF, KERNEL_ADDR));
        N("SIOCETHTOOL to a kernel address", ioctl(f, SIOCETHTOOL, KERNEL_ADDR));
        N("0x8999 to a kernel address", ioctl(f, 0x8999, KERNEL_ADDR));
        N("0x89a9 to a kernel address", ioctl(f, 0x89a9, KERNEL_ADDR));
        /* SIOCGIFCONF: count, then the entries; lo's entry. */
        struct ifconf c = { 0 };
        N("SIOCGIFCONF count", ioctl(f, SIOCGIFCONF, &c));
        int total = c.ifc_len;
        printf("  multiple of 40: %s, at least one: %s\n", total % 40 == 0 ? "yes" : "no", total >= 40 ? "yes" : "no");
        struct ifreq rs[64];
        memset(rs, 0x55, sizeof rs);
        c.ifc_len = sizeof rs;
        c.ifc_req = rs;
        N("SIOCGIFCONF", ioctl(f, SIOCGIFCONF, &c));
        yes("  same length as the count", c.ifc_len == total);
        printf("  first: %s family %d addr %s byte14 %#x\n", rs[0].ifr_name, rs[0].ifr_addr.sa_family,
            inet_ntoa(((struct sockaddr_in *)&rs[0].ifr_addr)->sin_addr), (unsigned char)rs[0].ifr_name[14]);
        c.ifc_len = 39;
        N("SIOCGIFCONF into 39 bytes", ioctl(f, SIOCGIFCONF, &c));
        printf("  ifc_len %d\n", c.ifc_len);
        c.ifc_len = -5;
        N("SIOCGIFCONF negative length", ioctl(f, SIOCGIFCONF, &c));
        printf("  ifc_len %d\n", c.ifc_len);
        c.ifc_len = 80;
        c.ifc_req = KERNEL_ADDR;
        N("SIOCGIFCONF entries to a kernel address", ioctl(f, SIOCGIFCONF, &c));
        /* ethtool: link state of lo. */
        struct ethtool_value ev = { ETHTOOL_GLINK, 77 };
        struct ifreq er = { 0 };
        strcpy(er.ifr_name, "lo");
        er.ifr_data = (void *)&ev;
        N("SIOCETHTOOL GLINK lo", ioctl(f, SIOCETHTOOL, &er));
        printf("  data %u\n", ev.data);
        strcpy(er.ifr_name, "nope");
        N("SIOCETHTOOL GLINK nope", ioctl(f, SIOCETHTOOL, &er));
        strcpy(er.ifr_name, "lo");
        er.ifr_data = KERNEL_ADDR;
        N("SIOCETHTOOL lo, data at a kernel address", ioctl(f, SIOCETHTOOL, &er));
    }
    /* libc's name/index calls (SIOCGIFINDEX, SIOCGIFNAME on an AF_UNIX socket). */
    printf("if_nametoindex(lo) = %u\n", if_nametoindex("lo"));
    errno = 0;
    printf("if_nametoindex(nope) = %u errno %s\n", if_nametoindex("nope"), nename(errno));
    char nm[IF_NAMESIZE];
    printf("if_indextoname(1) = %s\n", if_indextoname(1, nm) ? nm : "NULL");
    errno = 0;
    char *x = if_indextoname(9999, nm);
    printf("if_indextoname(9999) = %s errno %s\n", x ? x : "NULL", nename(errno));
}


/* ---- part "show": every interface and address (a diagnostic for the Mini-PC check, not compared) ------------ */

static void show_link(struct nlmsghdr *h) {
    struct ifinfomsg *ifi = NLMSG_DATA(h);
    int len = IFLA_PAYLOAD(h);
    const char *name = "?";
    unsigned mtu = 0, carrier = 0;
    unsigned char *mac = 0;
    for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        if (a->rta_type == IFLA_IFNAME)
            name = RTA_DATA(a);
        if (a->rta_type == IFLA_MTU)
            mtu = *(unsigned *)RTA_DATA(a);
        if (a->rta_type == IFLA_CARRIER)
            carrier = *(unsigned char *)RTA_DATA(a);
        if (a->rta_type == IFLA_ADDRESS && RTA_PAYLOAD(a) == 6)
            mac = RTA_DATA(a);
    }
    printf("link %d %s type %d flags %#x mtu %u carrier %u", ifi->ifi_index, name, ifi->ifi_type, ifi->ifi_flags, mtu, carrier);
    if (mac && ifi->ifi_type == ARPHRD_ETHER)
        printf(" mac %02x:%02x:%02x:%02x:%02x:%02x", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    printf("\n");
}

static void show_addr(struct nlmsghdr *h) {
    struct ifaddrmsg *ifa = NLMSG_DATA(h);
    int len = IFA_PAYLOAD(h);
    char ip[32] = "?", brd[32] = "", label[IFNAMSIZ + 1] = "?";
    unsigned valid = 0;
    for (struct rtattr *a = IFA_RTA(ifa); RTA_OK(a, len); a = RTA_NEXT(a, len)) {
        if (a->rta_type == IFA_LOCAL)
            inet_ntop(AF_INET, RTA_DATA(a), ip, sizeof ip);
        if (a->rta_type == IFA_BROADCAST)
            inet_ntop(AF_INET, RTA_DATA(a), brd, sizeof brd);
        if (a->rta_type == IFA_LABEL)
            snprintf(label, sizeof label, "%s", (char *)RTA_DATA(a));
        if (a->rta_type == IFA_CACHEINFO)
            valid = ((struct ifa_cacheinfo *)RTA_DATA(a))->ifa_valid;
    }
    printf("%s %d %s %s/%d", h->nlmsg_type == RTM_DELADDR ? "deladdr" : "addr", ifa->ifa_index, label, ip, ifa->ifa_prefixlen);
    if (*brd)
        printf(" brd %s", brd);
    if (valid == 0xffffffffu)
        printf(" scope %d flags %#x valid forever\n", ifa->ifa_scope, ifa->ifa_flags);
    else
        printf(" scope %d flags %#x valid %u\n", ifa->ifa_scope, ifa->ifa_flags, valid);
}

static char names[64][IFNAMSIZ + 1];
static int nnames;

static void keep_name(struct nlmsghdr *h) {
    struct ifinfomsg *ifi = NLMSG_DATA(h);
    int len = IFLA_PAYLOAD(h);
    for (struct rtattr *a = IFLA_RTA(ifi); RTA_OK(a, len); a = RTA_NEXT(a, len))
        if (a->rta_type == IFLA_IFNAME && nnames < 64)
            snprintf(names[nnames++], IFNAMSIZ + 1, "%s", (char *)RTA_DATA(a));
}

static void show_part(void) {
    int s = nl_open(SOCK_RAW);
    struct dump d;
    nl_dump(s, RTM_GETLINK, AF_UNSPEC, 1);
    unsigned me = own_pid(s);
    read_dump(s, RTM_NEWLINK, 1, me, &d, show_link);
    nl_dump(s, RTM_GETLINK, AF_UNSPEC, 2);
    read_dump(s, RTM_NEWLINK, 2, me, &d, keep_name);
    nl_dump(s, RTM_GETADDR, AF_INET, 3);
    read_dump(s, RTM_NEWADDR, 3, me, &d, show_addr);
    close(s);
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    for (int i = 0; i < nnames; i++) {
        struct ifreq r = { 0 };
        snprintf(r.ifr_name, sizeof r.ifr_name, "%s", names[i]);
        printf("if_nametoindex(%s) = %u", names[i], if_nametoindex(names[i]));
        if (ioctl(u, SIOCGIFADDR, &r) == 0)
            printf(", SIOCGIFADDR %s = %s\n", names[i], inet_ntoa(((struct sockaddr_in *)&r.ifr_addr)->sin_addr));
        else
            printf(", SIOCGIFADDR %s = -%s\n", names[i], nename(errno));
    }
    close(u);
}

/* ---- part "watch [s]": the kernel's link and address notifications for a while (Mini-PC check) ------------- */

static void watch_part(int secs) {
    int s = nl_open(SOCK_RAW);
    struct sockaddr_nl a = { .nl_family = AF_NETLINK, .nl_groups = RTMGRP_LINK | RTMGRP_IPV4_IFADDR };
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0) {
        printf("bind = -%s\n", nename(errno));
        return;
    }
    printf("watching RTMGRP_LINK and RTMGRP_IPV4_IFADDR for %d s\n", secs);
    fflush(stdout);
    int seen = 0;
    uint64_t end = ns_of(CLOCK_MONOTONIC) + (uint64_t)secs * 1000000000u;
    for (;;) {
        uint64_t now = ns_of(CLOCK_MONOTONIC);
        if (now >= end)
            break;
        struct pollfd p = { s, POLLIN, 0 };
        if (poll(&p, 1, (int)((end - now) / 1000000u) + 1) <= 0)
            continue;
        long n = nl_recv(s, 0);
        if (n == -ENOBUFS) {
            printf("ENOBUFS (notifications lost)\n");
            continue;
        }
        if (n <= 0)
            continue;
        int len = n;
        for (struct nlmsghdr *h = (struct nlmsghdr *)rbuf; NLMSG_OK(h, len); h = NLMSG_NEXT(h, len)) {
            seen++;
            if (h->nlmsg_type == RTM_NEWLINK) {
                struct ifinfomsg *ifi = NLMSG_DATA(h);
                printf("NEWLINK %d flags %#x\n", ifi->ifi_index, ifi->ifi_flags);
            } else if (h->nlmsg_type == RTM_NEWADDR || h->nlmsg_type == RTM_DELADDR) {
                printf("%s ", h->nlmsg_type == RTM_NEWADDR ? "NEWADDR" : "DELADDR");
                show_addr(h);
            } else {
                printf("%s\n", tname(h->nlmsg_type));
            }
            fflush(stdout);
        }
    }
    if (!seen)
        printf("no notification in %d s\n", secs);
    close(s);
}

/* ---- part "hostile": broken headers and attributes, IPv6 GETADDR, strict checking with texts --------------- */

/* The first message's error and NLMSGERR_ATTR_MSG text (NLMSG_ERROR or NLMSG_DONE), or the receive error. */
static void show_err(const char *label, int fd) {
    long n = nl_recv(fd, 0);
    if (n < 0) {
        printf("%s: -%s\n", label, nename(-n));
        return;
    }
    struct nlmsghdr *h = (struct nlmsghdr *)rbuf;
    if (!NLMSG_OK(h, (int)n) || (h->nlmsg_type != NLMSG_ERROR && h->nlmsg_type != NLMSG_DONE)) {
        printf("%s: %s\n", label, tname(h->nlmsg_type));
        return;
    }
    int e = *(int *)NLMSG_DATA(h);
    unsigned off = NLMSG_HDRLEN + 4;
    if (h->nlmsg_type == NLMSG_ERROR)
        off += (h->nlmsg_flags & NLM_F_CAPPED) ? NLMSG_HDRLEN : ((struct nlmsgerr *)NLMSG_DATA(h))->msg.nlmsg_len;
    off = NLMSG_ALIGN(off);
    const char *text = "";
    while (off + 4 <= h->nlmsg_len) {
        struct nlattr *a = (struct nlattr *)(rbuf + off);
        if (a->nla_len < 4)
            break;
        if ((a->nla_type & NLA_TYPE_MASK) == NLMSGERR_ATTR_MSG)
            text = (const char *)(a + 1);
        off += NLA_ALIGN(a->nla_len);
    }
    printf("%s: %s error %s text \"%s\"\n", label, tname(h->nlmsg_type), e ? nename(-e) : "0", text);
    while (nl_recv(fd, 0) > 0)
        ;
}

/* An ifaddrmsg for AF_INET6 plus attributes. */
static int ifa6(char *b, int prefixlen, const void *attrs, int alen) {
    struct ifaddrmsg *m = (struct ifaddrmsg *)b;
    memset(m, 0, sizeof *m);
    m->ifa_family = AF_INET6;
    m->ifa_prefixlen = prefixlen;
    memcpy(b + sizeof *m, attrs, alen);
    return sizeof *m + alen;
}

static int put_attr(char *b, int type, const void *data, int len) {
    struct nlattr *a = (struct nlattr *)b;
    a->nla_len = NLA_HDRLEN + len;
    a->nla_type = type;
    memcpy(a + 1, data, len);
    return NLA_ALIGN(a->nla_len);
}

static void hostile_part(void) {
    int s = nl_open(SOCK_RAW);
    unsigned me = (nl_dump(s, RTM_GETLINK, AF_UNSPEC, 1), own_pid(s));
    struct dump d;
    read_dump(s, RTM_NEWLINK, 1, me, &d, 0);

    /* A request, then a header whose length runs past the datagram. */
    char b[256] = { 0 };
    struct nlmsghdr *h = (struct nlmsghdr *)b;
    h->nlmsg_len = NLMSG_LENGTH(sizeof(struct ifinfomsg));
    h->nlmsg_type = RTM_GETLINK;
    h->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    h->nlmsg_seq = 2;
    ((struct ifinfomsg *)NLMSG_DATA(h))->ifi_index = 1;
    struct nlmsghdr *h2 = (struct nlmsghdr *)(b + NLMSG_ALIGN(h->nlmsg_len));
    *h2 = *h;
    h2->nlmsg_len = 200;
    N("send a request and a broken one", send(s, b, NLMSG_ALIGN(h->nlmsg_len) + 32, 0));
    for (int i = 0; i < 3; i++)
        show_msgs("answer", nl_recv(s, 0), me, 2);
    unsigned lens[] = { 0, 15, 0xffffffffu };
    for (int i = 0; i < 3; i++) {
        h->nlmsg_len = lens[i];
        char label[64];
        snprintf(label, sizeof label, "nlmsg_len %#x", lens[i]);
        N(label, send(s, b, 32, 0));
        show_msgs("  answer", nl_recv(s, 0), me, 2);
    }

    /* Payloads and attributes without strict checking. */
    char p[128] = { 0 };
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 3, p, 1);
    show_msgs("GETLINK with a 1-byte payload", nl_recv(s, 0), me, 3);
    struct ifinfomsg *ifi = (struct ifinfomsg *)p;
    ifi->ifi_index = 1;
    struct nlattr *a = (struct nlattr *)(p + sizeof *ifi);
    a->nla_len = 20;
    a->nla_type = IFLA_IFNAME;
    memcpy(a + 1, "lo", 3);
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 4, p, sizeof *ifi + 8);
    show_msgs("GETLINK lo, attribute longer than the message", nl_recv(s, 0), me, 4);
    show_msgs("  then", nl_recv(s, 0), me, 4);
    a->nla_len = 2;
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 5, p, sizeof *ifi + 4);
    show_msgs("GETLINK lo, attribute shorter than its header", nl_recv(s, 0), me, 5);
    show_msgs("  then", nl_recv(s, 0), me, 5);
    a->nla_len = 4;
    a->nla_type = 0;
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 6, p, sizeof *ifi + 4);
    show_msgs("GETLINK lo, attribute type 0", nl_recv(s, 0), me, 6);
    show_msgs("  then", nl_recv(s, 0), me, 6);
    ifi->ifi_index = 0;
    a->nla_type = IFLA_IFNAME;
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 7, p, sizeof *ifi + 4);
    show_msgs("GETLINK by an empty IFLA_IFNAME", nl_recv(s, 0), me, 7);

    /* GETADDR of one address: IPv6 has it (`inet6_rtm_getaddr`), IPv4 dumps only. Never an address V2 or a
     * Linux machine has: 2001:db8::/32 is for documentation. */
    struct in6_addr doc;
    inet_pton(AF_INET6, "2001:db8::1", &doc);
    char at[64];
    int n;
    n = put_attr(at, IFA_ADDRESS, &doc, 16);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 8, p, ifa6(p, 0, at, n));
    show_msgs("GETADDR AF_INET6 2001:db8::1", nl_recv(s, 0), me, 8);
    n = put_attr(at, IFA_LOCAL, &doc, 16);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 9, p, ifa6(p, 0, at, n));
    show_msgs("GETADDR AF_INET6 IFA_LOCAL 2001:db8::1", nl_recv(s, 0), me, 9);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 10, p, ifa6(p, 0, at, 0));
    show_msgs("GETADDR AF_INET6 without an address", nl_recv(s, 0), me, 10);
    n = put_attr(at, IFA_ADDRESS, "\1\2\3\4", 4);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 11, p, ifa6(p, 0, at, n));
    show_msgs("GETADDR AF_INET6 with a 4-byte address", nl_recv(s, 0), me, 11);
    n = put_attr(at, IFA_LABEL, "lo", 3);
    n += put_attr(at + n, IFA_ADDRESS, &doc, 16);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 12, p, ifa6(p, 64, at, n));
    show_msgs("GETADDR AF_INET6 with a label and a prefix length", nl_recv(s, 0), me, 12);
    int nsid = 5;
    n = put_attr(at, IFA_TARGET_NETNSID, &nsid, 4);
    n += put_attr(at + n, IFA_ADDRESS, &doc, 16);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 13, p, ifa6(p, 0, at, n));
    show_msgs("GETADDR AF_INET6 in namespace 5", nl_recv(s, 0), me, 13);
    n = put_attr(at, IFA_ADDRESS, &doc, 16);
    ifa6(p, 0, at, n);
    ((struct ifaddrmsg *)p)->ifa_index = 99999;
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 14, p, sizeof(struct ifaddrmsg) + n);
    show_msgs("GETADDR AF_INET6 on interface 99999", nl_recv(s, 0), me, 14);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 15, p, 1);
    show_msgs("GETADDR AF_INET6 with a 1-byte payload", nl_recv(s, 0), me, 15);
    struct ifaddrmsg v4 = { .ifa_family = AF_INET };
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 16, &v4, sizeof v4);
    show_msgs("GETADDR AF_INET (dump only)", nl_recv(s, 0), me, 16);
    v4.ifa_family = AF_UNSPEC;
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_ACK, 17, &v4, sizeof v4);
    show_msgs("GETADDR AF_UNSPEC (dump only)", nl_recv(s, 0), me, 17);
    close(s);

    /* Strict checking with extended acks: the texts. */
    s = nl_open(SOCK_RAW);
    int one = 1;
    setsockopt(s, SOL_NETLINK, NETLINK_GET_STRICT_CHK, &one, sizeof one);
    setsockopt(s, SOL_NETLINK, NETLINK_EXT_ACK, &one, sizeof one);
    memset(p, 0, sizeof p);
    ifi->ifi_index = 1;
    a->nla_len = 20;
    a->nla_type = IFLA_IFNAME;
    memcpy(a + 1, "lo", 3);
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST, 20, p, sizeof *ifi + 8);
    show_err("strict GETLINK lo, attribute longer than the message", s);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST, 21, p, ifa6(p, 0, at, 0) - 7);
    show_err("strict GETADDR AF_INET6 with a 1-byte payload", s);
    n = put_attr(at, IFA_ADDRESS, &doc, 16);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST, 22, p, ifa6(p, 64, at, n));
    show_err("strict GETADDR AF_INET6 with a prefix length", s);
    n = put_attr(at, IFA_LABEL, "lo", 3);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST, 23, p, ifa6(p, 0, at, n));
    show_err("strict GETADDR AF_INET6 with a label", s);
    n = put_attr(at, IFA_ADDRESS, "\1\2\3\4", 4);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST, 24, p, ifa6(p, 0, at, n));
    show_err("strict GETADDR AF_INET6 with a 4-byte address", s);
    n = put_attr(at, 0, "", 0);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST, 25, p, ifa6(p, 0, at, n));
    show_err("strict GETADDR AF_INET6 with attribute type 0", s);
    n = put_attr(at, 40, "", 0);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST, 26, p, ifa6(p, 0, at, n));
    show_err("strict GETADDR AF_INET6 with attribute type 40", s);
    n = put_attr(at, IFA_ADDRESS, &doc, 16);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST, 27, p, ifa6(p, 0, at, n));
    show_err("strict GETADDR AF_INET6 2001:db8::1", s);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 28, p, 1);
    show_err("strict GETADDR AF_INET6 dump with a 1-byte payload", s);
    ifa6(p, 64, at, 0);
    nl_req(s, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 29, p, sizeof(struct ifaddrmsg));
    show_err("strict GETADDR AF_INET6 dump with a prefix length", s);
    /* Still answering. */
    memset(p, 0, sizeof p);
    ifi->ifi_index = 1;
    nl_req(s, RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 30, p, sizeof *ifi);
    show_msgs("strict GETLINK lo afterwards", nl_recv(s, 0), own_pid(s), 30);
    close(s);
}

int main(int argc, char **argv) {
    const char *part = argc > 1 ? argv[1] : "all";
    setvbuf(stdout, 0, _IOLBF, 0);
    int all = !strcmp(part, "all");
    if (all || !strcmp(part, "sock"))
        sock_part();
    if (all || !strcmp(part, "msgs"))
        msgs_part();
    if (all || !strcmp(part, "link"))
        link_part();
    if (all || !strcmp(part, "ifaddrs"))
        ifaddrs_part();
    if (all || !strcmp(part, "devbind"))
        devbind_part();
    if (all || !strcmp(part, "ioctl"))
        ioctl_part();
    if (all || !strcmp(part, "hostile"))
        hostile_part();
    /* Diagnostics for the Mini-PC, not compared with Linux (interface names and addresses differ). */
    if (!strcmp(part, "show"))
        show_part();
    if (!strcmp(part, "watch"))
        watch_part(argc > 2 ? atoi(argv[2]) : 10);
    return 0;
}
