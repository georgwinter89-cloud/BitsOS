/* SX-2: what a new network namespace offers (linux-6.6/net/core/net_namespace.c setup_net,
 * drivers/net/loopback.c): only lo, and it is down, so no address and no route: connect/sendto ENETUNREACH,
 * bind to an address EADDRNOTAVAIL, the interface ioctls know only lo (net/core/dev_ioctl.c, net/ipv4/
 * devinet.c). AF_UNIX keeps working. `nw`: what NW-11's per-namespace views must add (see nw()). Uses only
 * loopback and RFC 5737 addresses. */
#include "ns.h"
#include <arpa/inet.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <stddef.h>
#include <sys/un.h>

static struct sockaddr_in v4(const char *a, int port) {
    struct sockaddr_in s = { 0 };
    s.sin_family = AF_INET;
    s.sin_port = htons(port);
    inet_pton(AF_INET, a, &s.sin_addr);
    return s;
}

static long ifr(int fd, unsigned long cmd, const char *name, struct ifreq *r) {
    memset(r, 0, sizeof *r);
    snprintf(r->ifr_name, sizeof r->ifr_name, "%s", name);
    return ioctl(fd, cmd, r);
}

static int isolated(void) {
    R("unshare(CLONE_NEWUSER|CLONE_NEWNET)", unshare(CLONE_NEWUSER | CLONE_NEWNET));
    int t = socket(AF_INET, SOCK_STREAM, 0);
    printf("socket(AF_INET, SOCK_STREAM) = %s\n", yn(t >= 0));
    struct sockaddr_in lo = v4("127.0.0.1", 9), doc = v4("192.0.2.1", 9), any = v4("0.0.0.0", 0), lo0 = v4("127.0.0.1", 0);
    R("tcp connect(127.0.0.1)", connect(t, (void *)&lo, sizeof lo));
    R("tcp connect(192.0.2.1)", connect(t, (void *)&doc, sizeof doc));
    R("tcp connect with a short address", connect(t, (void *)&lo, 8));
    R("tcp listen unbound", listen(t, 4));
    struct sockaddr_in lo2 = v4("127.0.0.2", 0);
    struct { const char *label; int type; struct sockaddr_in *a; } binds[] = {
        { "tcp bind(127.0.0.1)", SOCK_STREAM, &lo0 }, { "tcp bind(127.0.0.2)", SOCK_STREAM, &lo2 },
        { "tcp bind(0.0.0.0)", SOCK_STREAM, &any }, { "udp bind(127.0.0.1)", SOCK_DGRAM, &lo0 },
        { "udp bind(0.0.0.0)", SOCK_DGRAM, &any },
    };
    for (unsigned i = 0; i < sizeof binds / sizeof *binds; i++) {
        int b = socket(AF_INET, binds[i].type, 0);
        R(binds[i].label, bind(b, (void *)binds[i].a, sizeof *binds[i].a));
        close(b);
    }
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    R("udp sendto(127.0.0.1)", sendto(u, "x", 1, 0, (void *)&lo, sizeof lo));
    close(u);
    u = socket(AF_INET, SOCK_DGRAM, 0);
    R("udp sendto(192.0.2.1)", sendto(u, "x", 1, 0, (void *)&doc, sizeof doc));
    close(u);
    u = socket(AF_INET, SOCK_DGRAM, 0);
    struct iovec io = { "x", 1 };
    struct msghdr m = { .msg_name = &lo, .msg_namelen = sizeof lo, .msg_iov = &io, .msg_iovlen = 1 };
    R("udp sendmsg(127.0.0.1)", sendmsg(u, &m, 0));
    close(u);
    u = socket(AF_INET, SOCK_DGRAM, 0);
    R("udp send unconnected", send(u, "x", 1, 0));
    R("udp connect(127.0.0.1)", connect(u, (void *)&lo, sizeof lo));
    struct sockaddr_in6 l6 = { 0 }, any6 = { 0 };
    l6.sin6_family = AF_INET6;
    l6.sin6_port = htons(9);
    l6.sin6_addr = in6addr_loopback;
    any6.sin6_family = AF_INET6;
    struct { const char *label; int type, op; struct sockaddr_in6 *a; } six[] = {
        { "udp6 connect(::1)", SOCK_DGRAM, 0, &l6 }, { "tcp6 connect(::1)", SOCK_STREAM, 0, &l6 },
        { "udp6 sendto(::1)", SOCK_DGRAM, 1, &l6 }, { "udp6 bind(::1)", SOCK_DGRAM, 2, &l6 },
        { "tcp6 bind(::1)", SOCK_STREAM, 2, &l6 }, { "udp6 bind(::)", SOCK_DGRAM, 2, &any6 },
    };
    for (unsigned i = 0; i < sizeof six / sizeof *six; i++) {
        int b = socket(AF_INET6, six[i].type, 0);
        if (b < 0) {
            R(six[i].label, -1);
            continue;
        }
        if (six[i].op == 0)
            R(six[i].label, connect(b, (void *)six[i].a, sizeof *six[i].a));
        else if (six[i].op == 1)
            R(six[i].label, sendto(b, "x", 1, 0, (void *)six[i].a, sizeof *six[i].a));
        else
            R(six[i].label, bind(b, (void *)six[i].a, sizeof *six[i].a));
        close(b);
    }

    char buf[512];
    struct ifconf c = { .ifc_len = sizeof buf, .ifc_buf = buf };
    R("SIOCGIFCONF", ioctl(u, SIOCGIFCONF, &c));
    printf("SIOCGIFCONF length %d\n", c.ifc_len);
    struct ifreq r;
    R("SIOCGIFINDEX lo", ifr(u, SIOCGIFINDEX, "lo", &r));
    printf("lo index %d\n", r.ifr_ifindex);
    R("SIOCGIFINDEX eth0", ifr(u, SIOCGIFINDEX, "eth0", &r));
    R("SIOCGIFFLAGS lo", ifr(u, SIOCGIFFLAGS, "lo", &r));
    printf("lo flags %#x\n", (unsigned)(unsigned short)r.ifr_flags);
    R("SIOCGIFADDR lo", ifr(u, SIOCGIFADDR, "lo", &r));
    R("SIOCGIFFLAGS eth0", ifr(u, SIOCGIFFLAGS, "eth0", &r));
    memset(&r, 0, sizeof r);
    r.ifr_ifindex = 1;
    R("SIOCGIFNAME 1", ioctl(u, SIOCGIFNAME, &r));
    printf("index 1 is %s\n", r.ifr_name);
    r.ifr_ifindex = 2;
    R("SIOCGIFNAME 2", ioctl(u, SIOCGIFNAME, &r));
    printf("if_nametoindex(lo) = %u\n", if_nametoindex("lo"));

    int sp[2];
    R("socketpair(AF_UNIX)", socketpair(AF_UNIX, SOCK_STREAM, 0, sp));
    R("unix send", send(sp[0], "hi", 2, 0));
    R("unix recv", recv(sp[1], buf, sizeof buf, 0));
    return 0;
}

/* NW-11's part (KNOWN in abi-diff until NW gives namespaces their own views): an abstract AF_UNIX name of the
 * parent namespace is not reachable from a new one, and a new namespace binds any IPv4 address (CachyOS). */
static int nw(void) {
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    int n = snprintf(a.sun_path + 1, sizeof a.sun_path - 1, "bitsos-sx2-%d", (int)getpid());
    socklen_t len = offsetof(struct sockaddr_un, sun_path) + 1 + n;
    R("parent: bind abstract", bind(s, (void *)&a, len));
    R("parent: listen", listen(s, 4));
    pid_t p = fork();
    if (p == 0) {
        int c = socket(AF_UNIX, SOCK_STREAM, 0);
        R("same namespace: connect", connect(c, (void *)&a, len));
        close(c);
        R("unshare(CLONE_NEWUSER|CLONE_NEWNET)", unshare(CLONE_NEWUSER | CLONE_NEWNET));
        c = socket(AF_UNIX, SOCK_STREAM, 0);
        R("new namespace: connect abstract", connect(c, (void *)&a, len));
        struct sockaddr_in doc0 = v4("192.0.2.1", 0);
        int t = socket(AF_INET, SOCK_STREAM, 0);
        R("new namespace: tcp bind(192.0.2.1)", bind(t, (void *)&doc0, sizeof doc0));
        _exit(0);
    }
    reap("child", p);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IOLBF, 0);
    if (argc > 1 && !strcmp(argv[1], "nw"))
        return nw();
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq r;
    R("initial namespace: SIOCGIFINDEX lo", ifr(u, SIOCGIFINDEX, "lo", &r));
    printf("initial namespace: lo index %d\n", r.ifr_ifindex);
    pid_t p = fork();
    if (p == 0)
        _exit(isolated());
    reap("isolated", p);
    return 0;
}
