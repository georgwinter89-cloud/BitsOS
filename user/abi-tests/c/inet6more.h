/* NW-v6 fixes: the abi-inet part "v6more" (included by inet.c after inet6.h): a failed bind gives IPV6_V6ONLY
 * back, IPv6-only and AF_INET sockets share a port, multicast binds, receive ancillary data, short values and
 * bad pointers. Only ::1, 127.0.0.1 and multicast groups on lo, so CachyOS and V2 print the same. Ports
 * 24650-24659. */
#include <netinet/in.h>

static void dump_cmsgs(const char *label, struct msghdr *m) {
    printf("%s: flags%s\n", label, m->msg_flags & MSG_CTRUNC ? " MSG_CTRUNC" : "");
    for (struct cmsghdr *c = CMSG_FIRSTHDR(m); c; c = CMSG_NXTHDR(m, c)) {
        unsigned char *d = CMSG_DATA(c);
        size_t n = c->cmsg_len - CMSG_LEN(0);
        char ip[64] = "";
        if (c->cmsg_level == IPPROTO_IPV6 && c->cmsg_type == IPV6_PKTINFO) {
            struct in6_pktinfo pi;
            memcpy(&pi, d, sizeof pi);
            inet_ntop(AF_INET6, &pi.ipi6_addr, ip, sizeof ip);
            printf("  IPV6_PKTINFO len %zu %s ifindex %d\n", n, ip, pi.ipi6_ifindex);
        } else if (c->cmsg_level == IPPROTO_IP && c->cmsg_type == IP_PKTINFO) {
            struct in_pktinfo pi;
            char spec[32];
            memcpy(&pi, d, sizeof pi);
            inet_ntop(AF_INET, &pi.ipi_addr, ip, sizeof ip);
            inet_ntop(AF_INET, &pi.ipi_spec_dst, spec, sizeof spec);
            printf("  IP_PKTINFO len %zu ifindex %d spec_dst %s addr %s\n", n, pi.ipi_ifindex, spec, ip);
        } else if (n == 1) {
            printf("  level %d type %d byte %d\n", c->cmsg_level, c->cmsg_type, d[0]);
        } else {
            int v;
            memcpy(&v, d, sizeof v);
            printf("  level %d type %d len %zu int %d\n", c->cmsg_level, c->cmsg_type, n, v);
        }
    }
}

/* A receive waits 2 s at most (loopback delivery may finish after sendto returns). */
static void timeout(int fd) {
    struct timeval tv = { 2, 0 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
}

/* recvmsg with `ctl` bytes of control space, then its control messages. */
static void recv_ctl(const char *label, int fd, size_t ctl) {
    char buf[16], cbuf[256];
    struct iovec iov = { buf, sizeof buf };
    struct msghdr m = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = ctl ? cbuf : NULL, .msg_controllen = ctl };
    timeout(fd);
    long r = recvmsg(fd, &m, 0);
    if (r < 0) {
        printf("%s: %s\n", label, sename(errno));
        return;
    }
    dump_cmsgs(label, &m);
}

static void v6more_part(void) {
    int one = 1;
    /* __inet6_bind gives the old IPV6_V6ONLY back when the port is taken. */
    for (int ty = 0; ty < 2; ty++) {
        int st = ty ? SOCK_DGRAM : SOCK_STREAM;
        const char *k = ty ? "udp" : "tcp";
        int a = socket(AF_INET6, st, 0), b = socket(AF_INET6, st, 0), c = socket(AF_INET, st, 0);
        struct sockaddr_in6 l1 = at6("::1", 24650), any = at6("::", 24651);
        struct sockaddr_in v4 = at("127.0.0.1", 24651);
        printf("%s: bind [::1]:24650 %d, again %s\n", k, bind(a, SA(l1), sizeof l1), bind(b, SA(l1), sizeof l1) < 0 ? sename(errno) : "ok");
        printf("%s: IPV6_V6ONLY after the failed bind %d\n", k, sopt(b, IPPROTO_IPV6, IPV6_V6ONLY));
        R("bind [::]:24651", bind(b, SA(any), sizeof any));
        printf("%s: IPV6_V6ONLY %d\n", k, sopt(b, IPPROTO_IPV6, IPV6_V6ONLY));
        R("AF_INET bind 127.0.0.1:24651", bind(c, SA(v4), sizeof v4));
        close(a), close(b), close(c);
    }
    /* An IPv6-only listener beside an AF_INET one on one port; ::1 beside 127.0.0.1. */
    int l6 = socket(AF_INET6, SOCK_STREAM, 0), l4 = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in6 any2 = at6("::", 24652);
    struct sockaddr_in any4 = at("0.0.0.0", 24652);
    setsockopt(l6, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    R("v6-only bind [::]:24652", bind(l6, SA(any2), sizeof any2));
    R("listen", listen(l6, 4));
    R("AF_INET bind 0.0.0.0:24652", bind(l4, SA(any4), sizeof any4));
    R("listen", listen(l4, 4));
    int c4 = socket(AF_INET, SOCK_STREAM, 0), c6 = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in to4 = at("127.0.0.1", 24652);
    struct sockaddr_in6 to6 = at6("::1", 24652);
    R("AF_INET connect 127.0.0.1:24652", connect(c4, SA(to4), sizeof to4));
    R("AF_INET6 connect [::1]:24652", connect(c6, SA(to6), sizeof to6));
    int s4 = accept(l4, NULL, NULL), s6 = accept6("v6-only listener accepts", l6, c6);
    printf("AF_INET listener accepts: %s\n", s4 >= 0 ? ip_of(s4, 1) : sename(errno));
    int a = socket(AF_INET6, SOCK_STREAM, 0), b = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in6 lo6 = at6("::1", 24653);
    struct sockaddr_in lo4 = at("127.0.0.1", 24653);
    R("bind [::1]:24653", bind(a, SA(lo6), sizeof lo6));
    R("listen", listen(a, 4));
    R("AF_INET bind 127.0.0.1:24653", bind(b, SA(lo4), sizeof lo4));
    R("listen", listen(b, 4));
    int l = socket(AF_INET6, SOCK_STREAM, 0), x = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in6 lo6b = at6("::1", 24654);
    struct sockaddr_in alt = at("127.0.6.1", 24654);
    bind(l, SA(lo6b), sizeof lo6b);
    listen(l, 4);
    R("AF_INET connect 127.0.6.1:24654 (only ::1 listens)", connect(x, SA(alt), sizeof alt));
    close(l6), close(l4), close(c4), close(c6), close(s4), close(s6), close(a), close(b), close(l), close(x);
    /* UDP: the same per side. */
    int u6 = socket(AF_INET6, SOCK_DGRAM, 0), u4 = socket(AF_INET, SOCK_DGRAM, 0), snd = socket(AF_INET6, SOCK_DGRAM, 0), snd4 = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in6 uany = at6("::", 24655), ulo = at6("::1", 24655), umap = at6("::ffff:127.0.0.1", 24655);
    struct sockaddr_in u4any = at("0.0.0.0", 24655), ualt = at("127.0.6.1", 24655);
    setsockopt(u6, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    R("udp v6-only bind [::]:24655", bind(u6, SA(uany), sizeof uany));
    R("udp AF_INET bind 0.0.0.0:24655", bind(u4, SA(u4any), sizeof u4any));
    sendto(snd, "six", 3, 0, SA(ulo), sizeof ulo);
    sendto(snd, "four", 4, 0, SA(umap), sizeof umap);
    sendto(snd4, "alt", 3, 0, SA(ualt), sizeof ualt);
    char buf[8];
    long got;
    timeout(u6), timeout(u4);
    got = recv(u6, buf, sizeof buf, 0);
    printf("v6-only gets %.*s\n", got > 0 ? (int)got : 1, got > 0 ? buf : "-");
    for (int i = 0; i < 2; i++) {
        got = recv(u4, buf, sizeof buf, 0);
        printf("AF_INET gets %.*s\n", got > 0 ? (int)got : 1, got > 0 ? buf : "-");
    }
    printf("v6-only gets more: %s\n", recv(u6, buf, sizeof buf, MSG_DONTWAIT) > 0 ? "yes" : "no");
    close(u6), close(u4);
    /* Multicast binds: a link-scope group needs an interface; getsockname shows the group. */
    int g = socket(AF_INET6, SOCK_DGRAM, 0), h = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 ff02 = at6("ff02::fb", 24656), ff05 = at6("ff05::fb", 24657), lo1 = at6("::1", 24657);
    R("bind [ff02::fb]:24656", bind(g, SA(ff02), sizeof ff02));
    ff02.sin6_scope_id = 1;
    R("bind [ff02::fb%lo]:24656", bind(g, SA(ff02), sizeof ff02));
    struct sockaddr_in6 n;
    socklen_t nl = sizeof n;
    getsockname(g, SA(n), &nl);
    printf("getsockname: %s scope %u, IPV6_V6ONLY %d\n", name6(g, 0), n.sin6_scope_id, sopt(g, IPPROTO_IPV6, IPV6_V6ONLY));
    R("bind [ff05::fb]:24657", bind(h, SA(ff05), sizeof ff05));
    printf("getsockname: %s\n", name6(h, 0));
    sendto(snd, "x", 1, 0, SA(lo1), sizeof lo1);
    printf("unicast to ::1 reaches the group socket: %s\n", recv(h, buf, sizeof buf, MSG_DONTWAIT) > 0 ? "yes" : "no");
    close(g), close(h);
    /* Receive ancillary data. */
    int r = socket(AF_INET6, SOCK_DGRAM, 0), p = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in6 rany = at6("::", 24658), rlo = at6("::1", 24658);
    struct sockaddr_in r4 = at("127.0.0.1", 24658), p4 = at("127.0.0.1", 24659);
    bind(r, SA(rany), sizeof rany);
    setsockopt(r, IPPROTO_IPV6, IPV6_RECVPKTINFO, &one, sizeof one);
    setsockopt(r, IPPROTO_IPV6, IPV6_RECVHOPLIMIT, &one, sizeof one);
    setsockopt(r, IPPROTO_IPV6, IPV6_RECVTCLASS, &one, sizeof one);
    sendto(snd, "six", 3, 0, SA(rlo), sizeof rlo);
    recv_ctl("from ::1", r, 256);
    setsockopt(r, IPPROTO_IP, IP_PKTINFO, &one, sizeof one);
    sendto(snd4, "four", 4, 0, SA(r4), sizeof r4);
    recv_ctl("from 127.0.0.1", r, 256);
    bind(p, SA(p4), sizeof p4);
    setsockopt(p, IPPROTO_IP, IP_PKTINFO, &one, sizeof one);
    setsockopt(p, IPPROTO_IP, IP_RECVTTL, &one, sizeof one);
    setsockopt(p, IPPROTO_IP, IP_RECVTOS, &one, sizeof one);
    sendto(snd4, "x", 1, 0, SA(p4), sizeof p4);
    sendto(snd4, "y", 1, 0, SA(p4), sizeof p4);
    recv_ctl("AF_INET", p, 256);
    recv_ctl("AF_INET without control space", p, 0);
    close(r), close(p), close(snd), close(snd4);
    /* Short values and bad pointers. */
    int u = socket(AF_INET6, SOCK_DGRAM, 0);
    R("IPV6_UNICAST_HOPS with 2 bytes", setsockopt(u, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &one, 2));
    R("IPV6_TCLASS with 1 byte", setsockopt(u, IPPROTO_IPV6, IPV6_TCLASS, &one, 1));
    R("IPV6_UNICAST_HOPS from a kernel address", setsockopt(u, IPPROTO_IPV6, IPV6_UNICAST_HOPS, KERNEL_ADDR, 4));
    R("bind from a kernel address", bind(u, KERNEL_ADDR, sizeof(struct sockaddr_in6)));
    R("connect from a kernel address", connect(u, KERNEL_ADDR, sizeof(struct sockaddr_in6)));
    close(u);
}
