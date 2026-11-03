/* NW-12: addresses, MTU and the /proc/net view of a new network namespace's lo (run unprivileged inside
 * unshare(CLONE_NEWUSER|CLONE_NEWNET), the manual cross-check is `unshare -rn; ip link set lo up; ip addr`).
 * lo comes up with 127.0.0.1/8 and ::1 (linux-6.6/net/ipv4/devinet.c inetdev_event, net/ipv6/addrconf.c
 * init_loopback); addresses come and go by SIOCSIFADDR and RTM_NEWADDR/RTM_DELADDR; an MTU below 68 takes IPv4
 * away; /proc/net/{dev,if_inet6,route,tcp,udp,unix} show the namespace (fs/proc/proc_net.c), /sys/class/net keeps its
 * mounter's (fs/sysfs/mount.c). Prints no inode numbers, pointers or host values; addresses are loopback and
 * RFC 5737 only. */
#include "ns.h"
#include <arpa/inet.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/in.h>
#include <stddef.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
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

static long set_addr(int fd, unsigned long cmd, const char *name, const char *addr) {
    struct ifreq r;
    memset(&r, 0, sizeof r);
    snprintf(r.ifr_name, sizeof r.ifr_name, "%s", name);
    struct sockaddr_in a = v4(addr, 0);
    memcpy(&r.ifr_addr, &a, sizeof a);
    return ioctl(fd, cmd, &r);
}

static long ifr_int(int fd, unsigned long cmd, const char *name, int v) {
    struct ifreq r;
    memset(&r, 0, sizeof r);
    snprintf(r.ifr_name, sizeof r.ifr_name, "%s", name);
    r.ifr_mtu = v;
    return ioctl(fd, cmd, &r);
}

static long set_flags(int fd, const char *name, short flags) {
    struct ifreq r;
    memset(&r, 0, sizeof r);
    snprintf(r.ifr_name, sizeof r.ifr_name, "%s", name);
    r.ifr_flags = flags;
    return ioctl(fd, SIOCSIFFLAGS, &r);
}

static void show_get(int fd, unsigned long cmd, const char *what, const char *name) {
    struct ifreq r;
    char label[96], a[32] = "-";
    snprintf(label, sizeof label, "%s %s", what, name);
    if (R(label, ifr(fd, cmd, name, &r)) == 0) {
        struct sockaddr_in *s = (void *)&r.ifr_addr;
        inet_ntop(AF_INET, &s->sin_addr, a, sizeof a);
        printf("  %s\n", a);
    }
}

/* ---- rtnetlink ---- */

static int nl;
static unsigned seq;

static long nl_send(void *m, size_t len) {
    struct sockaddr_nl k = { .nl_family = AF_NETLINK };
    return sendto(nl, m, len, 0, (void *)&k, sizeof k);
}

/* The error of the ack to the last request (0 = ack), or 1 when none came. */
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

/* RTM_NEWADDR / RTM_DELADDR of an IPv4 address on lo; prints the ack's errno. */
static void addr(const char *label, int type, int flags, const char *ip, int prefix, int scope) {
    struct { struct nlmsghdr h; struct ifaddrmsg m; char attrs[64]; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = NLMSG_LENGTH(sizeof q.m);
    q.h.nlmsg_type = type;
    q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    q.h.nlmsg_seq = ++seq;
    q.m.ifa_family = AF_INET;
    q.m.ifa_prefixlen = prefix;
    q.m.ifa_scope = scope;
    q.m.ifa_index = if_nametoindex("lo");
    struct in_addr a;
    inet_pton(AF_INET, ip, &a);
    put_attr(&q.h, IFA_LOCAL, &a, 4);
    put_attr(&q.h, IFA_ADDRESS, &a, 4);
    nl_send(&q, q.h.nlmsg_len);
    int e = nl_ack();
    if (e < 0)
        printf("%s = -%s\n", label, en(-e));
    else
        printf("%s = %d\n", label, e);
}

/* An RTM_GETADDR dump of `family`: one line per address. */
static void dump(const char *label, int family) {
    struct { struct nlmsghdr h; struct ifaddrmsg m; } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = sizeof q;
    q.h.nlmsg_type = RTM_GETADDR;
    q.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    q.h.nlmsg_seq = ++seq;
    q.m.ifa_family = family;
    nl_send(&q, sizeof q);
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
            char a[64] = "-", lab[32] = "-";
            unsigned fl = m->ifa_flags;
            int proto = -1;
            int len = IFA_PAYLOAD(h);
            for (struct rtattr *r = IFA_RTA(m); RTA_OK(r, len); r = RTA_NEXT(r, len)) {
                if (r->rta_type == IFA_ADDRESS)
                    inet_ntop(m->ifa_family, RTA_DATA(r), a, sizeof a);
                if (r->rta_type == IFA_LABEL)
                    snprintf(lab, sizeof lab, "%s", (char *)RTA_DATA(r));
                if (r->rta_type == IFA_FLAGS)
                    fl = *(unsigned *)RTA_DATA(r);
                if (r->rta_type == 11 /* IFA_PROTO */)
                    proto = *(unsigned char *)RTA_DATA(r);
            }
            printf("  %s/%d scope %d flags %#x label %s proto %d\n", a, m->ifa_prefixlen, m->ifa_scope, fl, lab, proto);
        }
    }
}

/* ---- /proc/net ---- */

/* The line count of a file and its first line without trailing blanks (or the open's error). */
static void head(const char *path) {
    char buf[16384];
    long n = rd(path, buf, sizeof buf);
    if (n < 0) {
        printf("%s: -%s\n", path, en(errno));
        return;
    }
    int lines = 0;
    for (char *p = buf; *p; p++)
        lines += *p == '\n';
    char *nl1 = strchr(buf, '\n');
    if (nl1)
        *nl1 = 0;
    for (long i = (long)strlen(buf) - 1; i >= 0 && buf[i] == ' '; i--)
        buf[i] = 0;
    printf("%s: %d lines, width %s: %s\n", path, lines, nl1 && (nl1 - buf == 149 || nl1 - buf == 127) ? "padded" : "plain", buf);
}

/* A whole small file, each line indented (or the open's error). */
static void cat(const char *path) {
    char buf[4096];
    long n = rd(path, buf, sizeof buf);
    if (n < 0) {
        printf("%s: -%s\n", path, en(errno));
        return;
    }
    printf("%s:\n", path);
    for (char *l = strtok(buf, "\n"); l; l = strtok(0, "\n"))
        printf("  %s\n", l);
}

/* The rows of /proc/net/tcp or udp: local, remote, state and uid (no slot, inode or pointer). */
static void inet_rows(const char *path) {
    char buf[16384];
    if (rd(path, buf, sizeof buf) < 0)
        return;
    char *line = strchr(buf, '\n');
    while (line && line[1]) {
        line++;
        char loc[32], rem[32], st[8];
        unsigned uid;
        if (sscanf(line, "%*s %31s %31s %7s %*s %*s %*s %u", loc, rem, st, &uid) == 4)
            printf("  %s %s st %s uid %u\n", loc, rem, st, uid);
        line = strchr(line, '\n');
    }
}

/* The rows of /proc/net/unix: flags, type, state and path (no pointer, refcount or inode). */
static void unix_rows(void) {
    char buf[16384];
    if (rd("/proc/net/unix", buf, sizeof buf) < 0)
        return;
    char *line = strchr(buf, '\n');
    while (line && line[1]) {
        line++;
        char flags[16], type[8], st[8], path[128] = "";
        if (sscanf(line, "%*s %*s %*s %15s %7s %7s %*s %127s", flags, type, st, path) >= 3)
            printf("  flags %s type %s st %s path %s\n", flags, type, st, path);
        line = strchr(line, '\n');
    }
}

/* /proc/net/dev: the interface names and whether all of lo's counters are 0. */
static void dev(void) {
    char buf[8192];
    if (rd("/proc/net/dev", buf, sizeof buf) < 0) {
        printf("/proc/net/dev: -%s\n", en(errno));
        return;
    }
    char *line = buf;
    for (int i = 0; line && i < 2; i++)
        line = strchr(line, '\n') ? strchr(line, '\n') + 1 : 0;
    while (line && *line) {
        char name[32];
        unsigned long long v[16] = { 0 };
        int n = sscanf(line, " %31[^:]: %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu %llu", name, &v[0], &v[1], &v[2], &v[3], &v[4], &v[5], &v[6], &v[7], &v[8], &v[9], &v[10], &v[11], &v[12], &v[13], &v[14], &v[15]);
        int zero = 1;
        for (int i = 0; i < 16; i++)
            zero &= v[i] == 0;
        printf("  dev %s: %d fields, counters zero %s\n", name, n - 1, yn(zero));
        line = strchr(line, '\n') ? strchr(line, '\n') + 1 : 0;
    }
}

static long udp_bind(const char *ip) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = v4(ip, 0);
    long r = bind(s, (void *)&a, sizeof a);
    int e = errno;
    close(s);
    errno = e;
    return r;
}

static long udp_send(const char *ip) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = v4(ip, 9);
    long r = sendto(s, "x", 1, 0, (void *)&a, sizeof a);
    int e = errno;
    close(s);
    errno = e;
    return r;
}

static int inside(void) {
    R("unshare(CLONE_NEWUSER|CLONE_NEWNET)", unshare(CLONE_NEWUSER | CLONE_NEWNET));
    head("/proc/net/dev");
    dev();
    head("/proc/net/route");
    head("/proc/net/tcp");
    head("/proc/net/udp");
    head("/proc/net/unix");
    head("/proc/net/dhcp");
    cat("/proc/net/if_inet6");
    char buf[64];
    printf("/sys/class/net/lo/mtu: %s", rd("/sys/class/net/lo/mtu", buf, sizeof buf) > 0 ? buf : "-\n");
    nl = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_ROUTE);
    struct timeval tv = { .tv_sec = 2 };
    setsockopt(nl, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    int u = socket(AF_INET, SOCK_DGRAM, 0);

    /* lo down: an address by RTM_NEWADDR makes only itself local. */
    addr("down: RTM_NEWADDR 198.51.100.1/24", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, "198.51.100.1", 24, 0);
    R("down: bind 198.51.100.1", udp_bind("198.51.100.1"));
    R("down: bind 198.51.100.7", udp_bind("198.51.100.7"));
    R("down: bind 127.0.0.1", udp_bind("127.0.0.1"));
    R("down: sendto 198.51.100.1", udp_send("198.51.100.1"));
    R("down: sendto 198.51.100.7", udp_send("198.51.100.7"));
    addr("RTM_NEWADDR 198.51.100.1/24 again", RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, "198.51.100.1", 24, 0);
    addr("RTM_NEWADDR 198.51.100.1/24 replace", RTM_NEWADDR, NLM_F_CREATE | NLM_F_REPLACE, "198.51.100.1", 24, 0);
    addr("RTM_NEWADDR prefix 33", RTM_NEWADDR, NLM_F_CREATE, "198.51.100.2", 33, 0);

    /* UP: 127.0.0.1/8 and ::1, the subnets become local. */
    R("SIOCSIFFLAGS lo up", set_flags(u, "lo", IFF_UP));
    dump("IPv4 after up", AF_INET);
    dump("IPv6 after up", AF_INET6);
    cat("/proc/net/if_inet6");
    show_get(u, SIOCGIFADDR, "SIOCGIFADDR", "lo");
    show_get(u, SIOCGIFNETMASK, "SIOCGIFNETMASK", "lo");
    R("up: bind 198.51.100.7", udp_bind("198.51.100.7"));
    R("up: bind 127.0.0.9", udp_bind("127.0.0.9"));
    R("up: bind 10.0.0.1", udp_bind("10.0.0.1"));

    /* Aliases by ioctl, a secondary by RTM_NEWADDR, deletions. */
    R("SIOCSIFADDR lo:1 192.0.2.9", set_addr(u, SIOCSIFADDR, "lo:1", "192.0.2.9"));
    R("SIOCSIFNETMASK lo:1 255.255.255.128", set_addr(u, SIOCSIFNETMASK, "lo:1", "255.255.255.128"));
    R("SIOCSIFNETMASK lo:1 255.0.255.0", set_addr(u, SIOCSIFNETMASK, "lo:1", "255.0.255.0"));
    show_get(u, SIOCGIFADDR, "SIOCGIFADDR", "lo:1");
    show_get(u, SIOCGIFNETMASK, "SIOCGIFNETMASK", "lo:1");
    R("SIOCSIFADDR lo:2 224.0.0.9", set_addr(u, SIOCSIFADDR, "lo:2", "224.0.0.9"));
    R("SIOCSIFBRDADDR lo:3 (no such alias)", set_addr(u, SIOCSIFBRDADDR, "lo:3", "192.0.2.255"));
    addr("RTM_NEWADDR 127.0.0.2/8 host", RTM_NEWADDR, NLM_F_CREATE, "127.0.0.2", 8, RT_SCOPE_HOST);
    addr("RTM_NEWADDR 127.0.0.3/8 global", RTM_NEWADDR, NLM_F_CREATE, "127.0.0.3", 8, 0);
    dump("IPv4 with aliases", AF_INET);
    /* SIOCSIFADDR onto an address lo has: the old one is deleted first and stays deleted (EEXIST). */
    R("SIOCSIFADDR lo:4 192.0.2.44", set_addr(u, SIOCSIFADDR, "lo:4", "192.0.2.44"));
    R("SIOCSIFADDR lo:4 127.0.0.2 (taken)", set_addr(u, SIOCSIFADDR, "lo:4", "127.0.0.2"));
    show_get(u, SIOCGIFADDR, "SIOCGIFADDR", "lo:4");
    /* UDP to another address of a subnet on lo arrives from the subnet's primary. */
    int rx = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in any = v4("0.0.0.0", 24601), to = v4("192.0.2.77", 24601), from;
    R("bind 0.0.0.0:24601", bind(rx, (void *)&any, sizeof any));
    int tx = socket(AF_INET, SOCK_DGRAM, 0);
    R("sendto 192.0.2.77:24601", sendto(tx, "lo", 2, 0, (void *)&to, sizeof to));
    socklen_t fl = sizeof from;
    char got[8], fa[32] = "-";
    R("recvfrom", recvfrom(rx, got, sizeof got, MSG_DONTWAIT, (void *)&from, &fl));
    inet_ntop(AF_INET, &from.sin_addr, fa, sizeof fa);
    printf("  from %s\n", fa);
    close(tx);
    int ls = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in l = v4("127.0.0.1", 24602);
    R("tcp bind 127.0.0.1:24602", bind(ls, (void *)&l, sizeof l));
    R("tcp listen", listen(ls, 4));
    int us = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un ua = { .sun_family = AF_UNIX };
    int un = snprintf(ua.sun_path + 1, sizeof ua.sun_path - 1, "abi-ns-lo");
    R("unix bind @abi-ns-lo", bind(us, (void *)&ua, offsetof(struct sockaddr_un, sun_path) + 1 + un));
    R("unix listen", listen(us, 1));
    printf("/proc/net/tcp rows:\n");
    inet_rows("/proc/net/tcp");
    printf("/proc/net/udp rows:\n");
    inet_rows("/proc/net/udp");
    printf("/proc/net/unix rows:\n");
    unix_rows();
    head("/proc/net/route");
    R("SIOCSIFFLAGS lo:1 down", set_flags(u, "lo:1", 0));
    R("SIOCSIFFLAGS lo:1 down again", set_flags(u, "lo:1", 0));
    addr("RTM_DELADDR 127.0.0.1/8", RTM_DELADDR, 0, "127.0.0.1", 8, 0);
    addr("RTM_DELADDR 127.0.0.1/8 again", RTM_DELADDR, 0, "127.0.0.1", 8, 0);
    dump("IPv4 after the deletions", AF_INET);
    R("bind 127.0.0.9 without 127.0.0.1/8", udp_bind("127.0.0.9"));

    /* MTU and queue length. */
    R("SIOCSIFTXQLEN lo 5", ifr_int(u, SIOCSIFTXQLEN, "lo", 5));
    struct ifreq r;
    R("SIOCGIFTXQLEN lo", ifr(u, SIOCGIFTXQLEN, "lo", &r));
    printf("  qlen %d\n", r.ifr_qlen);
    R("SIOCSIFMTU lo -1", ifr_int(u, SIOCSIFMTU, "lo", -1));
    R("SIOCSIFMTU lo 60", ifr_int(u, SIOCSIFMTU, "lo", 60));
    dump("IPv4 at MTU 60", AF_INET);
    dump("IPv6 at MTU 60", AF_INET6);
    addr("RTM_NEWADDR at MTU 60", RTM_NEWADDR, NLM_F_CREATE, "127.0.0.1", 8, RT_SCOPE_HOST);
    R("SIOCSIFMTU lo 1500", ifr_int(u, SIOCSIFMTU, "lo", 1500));
    R("SIOCGIFMTU lo", ifr(u, SIOCGIFMTU, "lo", &r));
    printf("  mtu %d\n", r.ifr_mtu);
    dump("IPv4 at MTU 1500", AF_INET);
    dump("IPv6 at MTU 1500", AF_INET6);
    R("SIOCSIFFLAGS lo down", set_flags(u, "lo", 0));
    dump("IPv6 down", AF_INET6);
    cat("/proc/net/if_inet6");
    R("SIOCSIFFLAGS lo up", set_flags(u, "lo", IFF_UP));
    dump("IPv4 up again", AF_INET);
    dump("IPv6 up again", AF_INET6);
    R("SIOCSIFFLAGS lo down", set_flags(u, "lo", 0));
    R("down again: bind 127.0.0.9", udp_bind("127.0.0.9"));
    R("down again: sendto 127.0.0.9", udp_send("127.0.0.9"));
    printf("/sys/class/net/lo/mtu: %s", rd("/sys/class/net/lo/mtu", buf, sizeof buf) > 0 ? buf : "-\n");
    /* INT-W21 (NW-12 CR 2): /proc/<pid>/net is that task's namespace (linux-6.6/fs/proc/proc_net.c
     * get_proc_task_net): ours equals /proc/net, the parent's (the initial namespace) lists its cards too. */
    char own[8192], mine[8192], parent[8192], path[64];
    long a = rd("/proc/self/net/dev", own, sizeof own), b = rd("/proc/net/dev", mine, sizeof mine);
    printf("/proc/self/net/dev is /proc/net/dev: %s\n", yn(a > 0 && a == b && !memcmp(own, mine, a)));
    snprintf(path, sizeof path, "/proc/%d/net/dev", (int)getppid());
    long c = rd(path, parent, sizeof parent);
    printf("the parent's net/dev lists more cards: %s\n", yn(c > a && a > 0));
    return 0;
}

int main(void) {
    setvbuf(stdout, 0, _IOLBF, 0);
    printf("initial namespace: /proc/net/tcp %s\n", yn(access("/proc/net/tcp", R_OK) == 0));
    pid_t p = fork();
    if (p == 0)
        _exit(inside());
    reap("inside", p);
    return 0;
}
