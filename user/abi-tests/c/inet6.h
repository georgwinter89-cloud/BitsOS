/* NW-v6: AF_INET6 parts of abi-inet (included by inet.c). Only ::1 and v4-mapped peers, so the transcript is
 * the same on a host with real IPv6 addresses and on V2 (which has none). Ports 24640-24649. */
#include <netinet/in.h>

static struct sockaddr_in6 at6(const char *ip, int port) {
    struct sockaddr_in6 a = { 0 };
    a.sin6_family = AF_INET6;
    a.sin6_port = htons(port);
    inet_pton(AF_INET6, ip, &a.sin6_addr);
    return a;
}

/* "<address> <port>" of a sockaddr_in6 (or the errno name). */
static const char *name6(int fd, int peer) {
    static char s[2][80];
    static int n;
    char *b = s[n++ & 1];
    struct sockaddr_in6 a;
    socklen_t l = sizeof a;
    if ((peer ? getpeername : getsockname)(fd, SA(a), &l) < 0)
        return sename(errno);
    char ip[64];
    inet_ntop(AF_INET6, &a.sin6_addr, ip, sizeof ip);
    snprintf(b, 80, "len %u family %d %s", l, a.sin6_family, ip);
    return b;
}

static int port6(int fd, int peer) {
    struct sockaddr_in6 a;
    socklen_t l = sizeof a;
    if ((peer ? getpeername : getsockname)(fd, SA(a), &l) < 0)
        return -1;
    return ntohs(a.sin6_port);
}

/* accept, then "<address>, port is the client's: yes/no". */
static int accept6(const char *label, int srv, int cli) {
    struct sockaddr_in6 a;
    socklen_t l = sizeof a;
    int s = accept(srv, SA(a), &l);
    if (s < 0) {
        printf("%s: %s\n", label, sename(errno));
        return -1;
    }
    char ip[64];
    inet_ntop(AF_INET6, &a.sin6_addr, ip, sizeof ip);
    printf("%s: len %u %s, port is the client's: %s\n", label, l, ip, ntohs(a.sin6_port) == port6(cli, 0) ? "yes" : "no");
    return s;
}

static void tcp6_part(void) {
    int s = socket(AF_INET6, SOCK_STREAM, 0);
    printf("SO_DOMAIN %d SO_TYPE %d SO_PROTOCOL %d\n", sopt(s, SOL_SOCKET, SO_DOMAIN), sopt(s, SOL_SOCKET, SO_TYPE), sopt(s, SOL_SOCKET, SO_PROTOCOL));
    printf("name of a fresh socket: %s port %d\n", name6(s, 0), port6(s, 0));
    R("getpeername fresh", getpeername(s, SA((struct sockaddr_in6){0}), &(socklen_t){ 28 }));
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in6 a = at6("::1", 24640);
    R("bind [::1]:24640", bind(s, SA(a), sizeof a));
    printf("name: %s port %d, IPV6_V6ONLY now %d\n", name6(s, 0), port6(s, 0), sopt(s, IPPROTO_IPV6, IPV6_V6ONLY));
    R("listen", listen(s, 4));
    int c4 = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in l4 = at("127.0.0.1", 24640);
    R("AF_INET connect 127.0.0.1:24640 (the listener is ::1 only)", connect(c4, SA(l4), sizeof l4));
    int c = socket(AF_INET6, SOCK_STREAM, 0);
    R("connect [::1]:24640", connect(c, SA(a), sizeof a));
    printf("client: %s, peer %s port %d\n", name6(c, 0), name6(c, 1), port6(c, 1));
    int t = accept6("accept", s, c);
    printf("accepted: %s port %d, SO_DOMAIN %d\n", name6(t, 0), port6(t, 0), sopt(t, SOL_SOCKET, SO_DOMAIN));
    char b[16] = { 0 };
    R("send ping", send(c, "ping", 4, 0));
    R("recv", recv(t, b, sizeof b, 0));
    R("send pong", send(t, "pong", 4, 0));
    R("recv", recv(c, b, sizeof b, 0));
    int n = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 none = at6("::1", 24641);
    R("connect [::1]:24641 (nobody)", connect(n, SA(none), sizeof none));
    close(n), close(t), close(c), close(c4), close(s);

    /* A dual-stack listener on ::. */
    int d = socket(AF_INET6, SOCK_STREAM, 0);
    setsockopt(d, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in6 any = at6("::", 24642);
    R("bind [::]:24642", bind(d, SA(any), sizeof any));
    R("listen", listen(d, 4));
    int w = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in w4 = at("0.0.0.0", 24642);
    R("AF_INET bind 0.0.0.0:24642", bind(w, SA(w4), sizeof w4));
    c4 = socket(AF_INET, SOCK_STREAM, 0);
    l4 = at("127.0.0.1", 24642);
    R("AF_INET connect 127.0.0.1:24642", connect(c4, SA(l4), sizeof l4));
    t = accept6("accept the IPv4 peer", d, c4);
    printf("accepted: %s\n", name6(t, 0));
    close(t);
    c = socket(AF_INET6, SOCK_STREAM, 0);
    R("connect [::]:24642", connect(c, SA(any), sizeof any));
    t = accept6("accept the IPv6 peer", d, c);
    close(t);
    int m = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 mp = at6("::ffff:127.0.0.1", 24642);
    R("connect [::ffff:127.0.0.1]:24642", connect(m, SA(mp), sizeof mp));
    printf("client: %s\n", name6(m, 0));
    t = accept6("accept the mapped peer", d, m);
    close(t), close(m), close(c), close(c4), close(w), close(d);

    /* IPV6_V6ONLY: :: beside 0.0.0.0 on one port. */
    int o = socket(AF_INET6, SOCK_STREAM, 0);
    R("IPV6_V6ONLY 1", setsockopt(o, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one));
    struct sockaddr_in6 any3 = at6("::", 24643);
    R("bind [::]:24643", bind(o, SA(any3), sizeof any3));
    int zero = 0;
    R("IPV6_V6ONLY 0 after bind", setsockopt(o, IPPROTO_IPV6, IPV6_V6ONLY, &zero, sizeof zero));
    w = socket(AF_INET, SOCK_STREAM, 0);
    w4 = at("0.0.0.0", 24643);
    R("AF_INET bind 0.0.0.0:24643", bind(w, SA(w4), sizeof w4));
    struct sockaddr_in6 mp3 = at6("::ffff:127.0.0.1", 24643);
    int o2 = socket(AF_INET6, SOCK_STREAM, 0);
    setsockopt(o2, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    R("v6-only bind [::ffff:127.0.0.1]:24643", bind(o2, SA(mp3), sizeof mp3));
    R("v6-only connect [::ffff:127.0.0.1]:24643", connect(o2, SA(mp3), sizeof mp3));
    close(o2), close(w), close(o);
}

static void udp6_part(void) {
    int a = socket(AF_INET6, SOCK_DGRAM, 0), b = socket(AF_INET6, SOCK_DGRAM, 0), c = socket(AF_INET, SOCK_DGRAM, 0);
    printf("SO_DOMAIN %d SO_TYPE %d SO_PROTOCOL %d\n", sopt(a, SOL_SOCKET, SO_DOMAIN), sopt(a, SOL_SOCKET, SO_TYPE), sopt(a, SOL_SOCKET, SO_PROTOCOL));
    struct sockaddr_in6 la = at6("::1", 24644);
    R("bind [::1]:24644", bind(a, SA(la), sizeof la));
    R("sendto [::1]:24644", sendto(b, "hello", 5, 0, SA(la), sizeof la));
    char buf[16];
    struct sockaddr_in6 from;
    socklen_t fl = sizeof from;
    R("recvfrom", recvfrom(a, buf, sizeof buf, 0, SA(from), &fl));
    char ip[64];
    inet_ntop(AF_INET6, &from.sin6_addr, ip, sizeof ip);
    printf("  from len %u %s, port is the sender's: %s\n", fl, ip, ntohs(from.sin6_port) == port6(b, 0) ? "yes" : "no");
    struct sockaddr_in l4 = at("127.0.0.1", 24644);
    R("AF_INET sendto 127.0.0.1:24644", sendto(c, "v4", 2, 0, SA(l4), sizeof l4));
    R("recv (::1 hears no IPv4)", recv(a, buf, sizeof buf, MSG_DONTWAIT));

    int d = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 da = at6("::", 24645);
    R("bind [::]:24645", bind(d, SA(da), sizeof da));
    struct sockaddr_in d4 = at("127.0.0.1", 24645);
    struct sockaddr_in6 d6 = at6("::1", 24645);
    const char *labels[] = { "AF_INET sender", "AF_INET6 sender to ::1", "AF_INET6 sender with a sockaddr_in" };
    for (int i = 0; i < 3; i++) {
        long r = i == 0 ? sendto(c, "x", 1, 0, SA(d4), sizeof d4) : i == 1 ? sendto(b, "x", 1, 0, SA(d6), sizeof d6) : sendto(b, "x", 1, 0, SA(d4), sizeof d4);
        fl = sizeof from;
        long g = recvfrom(d, buf, sizeof buf, 0, SA(from), &fl);
        inet_ntop(AF_INET6, &from.sin6_addr, ip, sizeof ip);
        printf("%s: sent %ld, got %ld from len %u %s\n", labels[i], r, g, fl, ip);
    }

    /* musl's AI_ADDRCONFIG probe, then a refusal on a connected socket. */
    int m = socket(AF_INET6, SOCK_DGRAM | SOCK_CLOEXEC, IPPROTO_UDP);
    struct sockaddr_in6 probe = at6("::1", 65535);
    R("connect [::1]:65535", connect(m, SA(probe), sizeof probe));
    printf("  peer %s port %d, name %s\n", name6(m, 1), port6(m, 1), name6(m, 0));
    struct sockaddr_in6 nobody = at6("::1", 24646);
    R("connect [::1]:24646", connect(m, SA(nobody), sizeof nobody));
    R("send", send(m, "x", 1, 0));
    R("send again (port unreachable)", send(m, "x", 1, 0));

    int o = socket(AF_INET6, SOCK_DGRAM, 0);
    int one = 1;
    setsockopt(o, IPPROTO_IPV6, IPV6_V6ONLY, &one, sizeof one);
    R("v6-only sendto a sockaddr_in", sendto(o, "x", 1, 0, SA(d4), sizeof d4));
    R("v6-only connect a sockaddr_in", connect(o, SA(d4), sizeof d4));
    struct sockaddr_in6 zero = at6("::1", 0);
    R("sendto [::1]:0", sendto(o, "x", 1, 0, SA(zero), sizeof zero));
    R("sendto 23 bytes", sendto(o, "x", 1, 0, SA(d6), 23));
    struct sockaddr_in6 un = { 0 };
    un.sin6_family = AF_UNSPEC;
    R("sendto AF_UNSPEC unconnected", sendto(o, "x", 1, 0, SA(un), sizeof un));
    close(o), close(m), close(d), close(c), close(b), close(a);
}

static void v6errors_part(void) {
    R("socket(AF_INET6, SEQPACKET)", socket(AF_INET6, SOCK_SEQPACKET, 0));
    R("socket(AF_INET6, STREAM, UDP)", socket(AF_INET6, SOCK_STREAM, IPPROTO_UDP));
    int sv[2];
    R("socketpair(AF_INET6)", socketpair(AF_INET6, SOCK_STREAM, 0, sv));
    int s = socket(AF_INET6, SOCK_STREAM, 0);
    struct sockaddr_in6 a = at6("::1", 24647);
    R("bind 23 bytes", bind(s, SA(a), 23));
    struct sockaddr_in a4 = at("127.0.0.1", 24647);
    R("bind a sockaddr_in", bind(s, SA(a4), sizeof a4));
    struct sockaddr_in6 f = a;
    f.sin6_family = AF_INET;
    R("bind family AF_INET, 28 bytes", bind(s, SA(f), sizeof f));
    struct sockaddr_in6 two = at6("::2", 24647);
    R("bind [::2]", bind(s, SA(two), sizeof two));
    struct sockaddr_in6 ll = at6("fe80::1", 24647);
    R("bind [fe80::1] without scope", bind(s, SA(ll), sizeof ll));
    ll.sin6_scope_id = 9999;
    R("bind [fe80::1%9999]", bind(s, SA(ll), sizeof ll));
    struct sockaddr_in6 mc = at6("ff02::1", 24647);
    R("TCP bind [ff02::1]", bind(s, SA(mc), sizeof mc));
    R("TCP connect a sockaddr_in", connect(s, SA(a4), sizeof a4));
    R("TCP connect [ff02::1]", connect(s, SA(mc), sizeof mc));
    R("bind [::1]:24647", bind(s, SA(a), sizeof a));
    R("bind again", bind(s, SA(a), sizeof a));
    close(s);
}

static void v6opts_part(void) {
    int t = socket(AF_INET6, SOCK_STREAM, 0), u = socket(AF_INET6, SOCK_DGRAM, 0), v4 = socket(AF_INET, SOCK_DGRAM, 0);
    int one = 1;
    /* nplb's PosixSocketSetOptionsTest.TryThemAll on AF_INET6 (yt-YT-4a CR 3). */
    struct { int fd, level, name; const char *what; } nplb[] = {
        { t, SOL_SOCKET, SO_REUSEADDR, "tcp SO_REUSEADDR" }, { t, SOL_SOCKET, SO_RCVBUF, "tcp SO_RCVBUF" }, { t, SOL_SOCKET, SO_SNDBUF, "tcp SO_SNDBUF" },
        { t, SOL_SOCKET, SO_KEEPALIVE, "tcp SO_KEEPALIVE" }, { t, SOL_SOCKET, SO_BROADCAST, "tcp SO_BROADCAST" }, { t, IPPROTO_TCP, TCP_KEEPIDLE, "tcp TCP_KEEPIDLE" },
        { t, IPPROTO_TCP, TCP_KEEPINTVL, "tcp TCP_KEEPINTVL" }, { t, IPPROTO_TCP, TCP_NODELAY, "tcp TCP_NODELAY" },
        { u, SOL_SOCKET, SO_REUSEADDR, "udp SO_REUSEADDR" }, { u, SOL_SOCKET, SO_RCVBUF, "udp SO_RCVBUF" }, { u, SOL_SOCKET, SO_SNDBUF, "udp SO_SNDBUF" },
        { u, SOL_SOCKET, SO_BROADCAST, "udp SO_BROADCAST" },
    };
    for (unsigned i = 0; i < sizeof nplb / sizeof nplb[0]; i++) {
        int v = nplb[i].name == SO_RCVBUF || nplb[i].name == SO_SNDBUF ? 16384 : one;
        R(nplb[i].what, setsockopt(nplb[i].fd, nplb[i].level, nplb[i].name, &v, sizeof v));
    }
    struct { int name; const char *what; } dflt[] = {
        { IPV6_V6ONLY, "IPV6_V6ONLY" }, { IPV6_UNICAST_HOPS, "IPV6_UNICAST_HOPS" }, { IPV6_MULTICAST_HOPS, "IPV6_MULTICAST_HOPS" },
        { IPV6_MULTICAST_LOOP, "IPV6_MULTICAST_LOOP" }, { IPV6_MULTICAST_IF, "IPV6_MULTICAST_IF" }, { IPV6_TCLASS, "IPV6_TCLASS" },
        { IPV6_RECVPKTINFO, "IPV6_RECVPKTINFO" }, { IPV6_MTU_DISCOVER, "IPV6_MTU_DISCOVER" }, { IPV6_ADDR_PREFERENCES, "IPV6_ADDR_PREFERENCES" },
        { IPV6_MULTICAST_ALL, "IPV6_MULTICAST_ALL" }, { IPV6_DONTFRAG, "IPV6_DONTFRAG" }, { IPV6_RECVERR, "IPV6_RECVERR" },
        { IPV6_MINHOPCOUNT, "IPV6_MINHOPCOUNT" }, { IPV6_MTU, "IPV6_MTU" }, { IPV6_ADDRFORM, "IPV6_ADDRFORM" },
        { IPV6_ADD_MEMBERSHIP, "IPV6_ADD_MEMBERSHIP" }, { 999, "SOL_IPV6 999" },
    };
    for (unsigned i = 0; i < sizeof dflt / sizeof dflt[0]; i++) {
        int vt = sopt(t, IPPROTO_IPV6, dflt[i].name), vu = sopt(u, IPPROTO_IPV6, dflt[i].name);
        char a[24], b[24];
        snprintf(a, sizeof a, "%s", vt <= -100000 ? sename(-100000 - vt) : "");
        snprintf(b, sizeof b, "%s", vu <= -100000 ? sename(-100000 - vu) : "");
        printf("get %s: tcp %s%d udp %s%d\n", dflt[i].what, a, vt <= -100000 ? 0 : vt, b, vu <= -100000 ? 0 : vu);
    }
    struct { int fd, name, val; const char *what; } set[] = {
        { t, IPV6_UNICAST_HOPS, 256, "tcp IPV6_UNICAST_HOPS 256" }, { t, IPV6_UNICAST_HOPS, 7, "tcp IPV6_UNICAST_HOPS 7" },
        { t, IPV6_UNICAST_HOPS, -2, "tcp IPV6_UNICAST_HOPS -2" }, { t, IPV6_TCLASS, 0xbb, "tcp IPV6_TCLASS 0xbb" },
        { u, IPV6_TCLASS, 0xbb, "udp IPV6_TCLASS 0xbb" }, { u, IPV6_TCLASS, 256, "udp IPV6_TCLASS 256" },
        { t, IPV6_MULTICAST_HOPS, 5, "tcp IPV6_MULTICAST_HOPS 5" }, { u, IPV6_MULTICAST_HOPS, -1, "udp IPV6_MULTICAST_HOPS -1" },
        { u, IPV6_MULTICAST_LOOP, 2, "udp IPV6_MULTICAST_LOOP 2" }, { u, IPV6_MULTICAST_LOOP, 0, "udp IPV6_MULTICAST_LOOP 0" },
        { u, IPV6_MULTICAST_IF, 0, "udp IPV6_MULTICAST_IF 0" }, { u, IPV6_MULTICAST_IF, 9999, "udp IPV6_MULTICAST_IF 9999" },
        { t, IPV6_MULTICAST_IF, 0, "tcp IPV6_MULTICAST_IF 0" }, { u, IPV6_RECVPKTINFO, 1, "udp IPV6_RECVPKTINFO 1" },
        { u, IPV6_RECVTCLASS, 1, "udp IPV6_RECVTCLASS 1" }, { u, IPV6_RECVHOPLIMIT, 1, "udp IPV6_RECVHOPLIMIT 1" },
        { u, IPV6_MTU, 1000, "udp IPV6_MTU 1000" }, { u, IPV6_MTU, 1280, "udp IPV6_MTU 1280" },
        { u, IPV6_MTU_DISCOVER, 6, "udp IPV6_MTU_DISCOVER 6" }, { u, IPV6_ADDR_PREFERENCES, 3, "udp IPV6_ADDR_PREFERENCES TMP|PUBLIC" },
        { u, IPV6_ADDR_PREFERENCES, 2, "udp IPV6_ADDR_PREFERENCES PUBLIC" }, { u, IPV6_ADDRFORM, AF_INET, "udp IPV6_ADDRFORM unconnected" },
        { u, 999, 1, "udp SOL_IPV6 999" }, { v4, IPV6_V6ONLY, 1, "AF_INET socket IPV6_V6ONLY" },
    };
    for (unsigned i = 0; i < sizeof set / sizeof set[0]; i++)
        R(set[i].what, setsockopt(set[i].fd, IPPROTO_IPV6, set[i].name, &set[i].val, sizeof set[i].val));
    printf("now: tcp UNICAST_HOPS %d TCLASS %#x, udp TCLASS %#x MULTICAST_HOPS %d ADDR_PREFERENCES %#x RECVPKTINFO %d\n", sopt(t, IPPROTO_IPV6, IPV6_UNICAST_HOPS),
        sopt(t, IPPROTO_IPV6, IPV6_TCLASS), sopt(u, IPPROTO_IPV6, IPV6_TCLASS), sopt(u, IPPROTO_IPV6, IPV6_MULTICAST_HOPS), sopt(u, IPPROTO_IPV6, IPV6_ADDR_PREFERENCES), sopt(u, IPPROTO_IPV6, IPV6_RECVPKTINFO));
    R("IPV6_V6ONLY with 2 bytes", setsockopt(u, IPPROTO_IPV6, IPV6_V6ONLY, &one, 2));
    printf("SOL_IP on AF_INET6: IP_TTL %d IP_TOS %d\n", sopt(u, IPPROTO_IP, IP_TTL), sopt(u, IPPROTO_IP, IP_TOS));
    int tos = 0x10;
    R("IP_TOS on AF_INET6", setsockopt(u, IPPROTO_IP, IP_TOS, &tos, sizeof tos));
    /* IPV6_ADDRFORM: a socket connected to a v4-mapped peer becomes AF_INET. */
    struct sockaddr_in6 mp = at6("::ffff:127.0.0.1", 24648);
    R("connect [::ffff:127.0.0.1]:24648", connect(u, SA(mp), sizeof mp));
    printf("IPV6_ADDRFORM get: %d\n", sopt(u, IPPROTO_IPV6, IPV6_ADDRFORM));
    int inet = AF_INET;
    R("IPV6_ADDRFORM AF_INET", setsockopt(u, IPPROTO_IPV6, IPV6_ADDRFORM, &inet, sizeof inet));
    printf("after: SO_DOMAIN %d, peer %s port %d, IPV6_V6ONLY %d\n", sopt(u, SOL_SOCKET, SO_DOMAIN), ip_of(u, 1), port_of(u, 1), sopt(u, IPPROTO_IPV6, IPV6_V6ONLY));
    int l6 = socket(AF_INET6, SOCK_DGRAM, 0);
    struct sockaddr_in6 lo = at6("::1", 24648);
    connect(l6, SA(lo), sizeof lo);
    R("IPV6_ADDRFORM on a ::1 peer", setsockopt(l6, IPPROTO_IPV6, IPV6_ADDRFORM, &inet, sizeof inet));
    close(l6), close(v4), close(u), close(t);
}
