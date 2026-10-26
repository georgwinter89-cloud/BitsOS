/* NW-5: AF_UNIX sockets (stream, datagram, seqpacket; names; listen/accept; SCM_RIGHTS; SCM_CREDENTIALS;
 * options) against Linux. argv[1] picks a part; files are made under /tmp with the pid in their name and
 * removed again. "sigpipe" ends the program by SIGPIPE (write to a closed peer without MSG_NOSIGNAL). */
#include "sock.h"
#include <sys/stat.h>
#include <sys/uio.h>
#include <sys/resource.h>
#include <termios.h>

static char tmpdir_path[96];

/* A path under /tmp named with the pid (kept until the program ends). */
static const char *tpath(const char *leaf) {
    char b[128];
    snprintf(b, sizeof b, "/tmp/abi-unix-%d-%s", (int)getpid(), leaf);
    return strdup(b);
}

static socklen_t sun_of(struct sockaddr_un *a, const char *path, int abstract) {
    memset(a, 0, sizeof *a);
    a->sun_family = AF_UNIX;
    if (abstract) {
        size_t n = strlen(path);
        memcpy(a->sun_path + 1, path, n);
        return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 + n);
    }
    strncpy(a->sun_path, path, sizeof a->sun_path - 1);
    return (socklen_t)(offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1);
}

static void pair_part(void) {
    int sv[2];
    R("socketpair(STREAM)", socketpair(AF_UNIX, SOCK_STREAM, 0, sv));
    revents("fresh pair a", sv[0], POLLIN | POLLOUT);
    R("send hello", send(sv[0], "hello", 5, 0));
    R("send world", send(sv[0], " world", 6, 0));
    revents("b with data", sv[1], POLLIN | POLLOUT);
    int n = -1;
    R("ioctl(FIONREAD)", ioctl(sv[1], FIONREAD, &n));
    printf("  FIONREAD %d\n", n);
    char b[64] = {0};
    R("recv(3, MSG_PEEK)", recv(sv[1], b, 3, MSG_PEEK));
    printf("  peeked '%.3s'\n", b);
    R("recv(64)", recv(sv[1], b, 64, 0));
    printf("  got '%.11s'\n", b);
    R("recv(MSG_DONTWAIT) empty", recv(sv[1], b, 64, MSG_DONTWAIT));
    R("shutdown(a, SHUT_WR)", shutdown(sv[0], SHUT_WR));
    R("recv after peer SHUT_WR", recv(sv[1], b, 64, 0));
    revents("b after peer SHUT_WR", sv[1], POLLIN | POLLOUT | POLLRDHUP);
    revents("a after own SHUT_WR", sv[0], POLLIN | POLLOUT | POLLRDHUP);
    R("send after own SHUT_WR", send(sv[0], "x", 1, MSG_NOSIGNAL));
    R("shutdown(how 3)", shutdown(sv[0], 3));
    R("shutdown(how -1)", shutdown(sv[0], -1));
    R("send b->a", send(sv[1], "late", 4, 0));
    R("recv a", recv(sv[0], b, 64, 0));
    R("close(a)", close(sv[0]));
    R("recv after peer close", recv(sv[1], b, 64, 0));
    revents("b after peer close", sv[1], POLLIN | POLLOUT | POLLRDHUP);
    R("send to closed peer", send(sv[1], "x", 1, MSG_NOSIGNAL));
    close(sv[1]);

    R("socketpair(STREAM) again", socketpair(AF_UNIX, SOCK_STREAM, 0, sv));
    R("send unread", send(sv[1], "never read", 10, 0));
    close(sv[0]);
    R("recv: peer closed with our data unread", recv(sv[1], b, 64, MSG_DONTWAIT));
    R("recv again", recv(sv[1], b, 64, MSG_DONTWAIT));
    close(sv[1]);

    int types[2] = { SOCK_SEQPACKET, SOCK_DGRAM };
    const char *names[2] = { "SEQPACKET", "DGRAM" };
    for (int i = 0; i < 2; i++) {
        printf("-- %s pair\n", names[i]);
        R("socketpair", socketpair(AF_UNIX, types[i], 0, sv));
        R("send one", send(sv[0], "one", 3, 0));
        R("send second", send(sv[0], "second", 6, 0));
        struct iovec iov = { b, 2 };
        struct msghdr m = { 0 };
        m.msg_iov = &iov;
        m.msg_iovlen = 1;
        R("recvmsg(2 bytes)", recvmsg(sv[1], &m, 0));
        printf("  flags %#x data '%.2s'\n", m.msg_flags, b);
        R("recv(1, MSG_TRUNC)", recv(sv[1], b, 1, MSG_TRUNC));
        R("recv empty queue", recv(sv[1], b, 1, MSG_DONTWAIT));
        R("send empty", send(sv[0], "", 0, 0));
        R("recv empty message", recv(sv[1], b, 8, MSG_DONTWAIT));
        n = -1;
        R("send for FIONREAD", send(sv[0], "abcd", 4, 0));
        R("send another", send(sv[0], "efghij", 6, 0));
        R("ioctl(FIONREAD)", ioctl(sv[1], FIONREAD, &n));
        printf("  FIONREAD %d\n", n);
        recv(sv[1], b, 64, 0);
        recv(sv[1], b, 64, 0);
        R("close(a)", close(sv[0]));
        R("recv after close, non-blocking", recv(sv[1], b, 8, MSG_DONTWAIT));
        R("send to closed peer", send(sv[1], "x", 1, MSG_NOSIGNAL));
        revents("survivor", sv[1], POLLIN | POLLOUT | POLLRDHUP);
        close(sv[1]);
    }
    printf("-- nonblocking stream fills up\n");
    R("socketpair(STREAM|NONBLOCK)", socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, sv));
    printf("  F_GETFL nonblock: %s\n", (fcntl(sv[0], F_GETFL) & O_NONBLOCK) ? "yes" : "no");
    static char big[65536];
    long total = 0, r;
    while ((r = send(sv[0], big, sizeof big, 0)) > 0)
        total += r;
    printf("  last send = -%s, more than 64 KiB queued: %s\n", sename(errno), total > 65536 ? "yes" : "no");
    revents("full writer", sv[0], POLLOUT);
    long got = 0;
    while ((r = recv(sv[1], big, sizeof big, 0)) > 0)
        got += r;
    printf("  read back all: %s\n", got == total ? "yes" : "no");
    revents("drained writer", sv[0], POLLOUT);
    close(sv[0]);
    close(sv[1]);
}

static void names_part(void) {
    struct sockaddr_un a, got;
    socklen_t l;
    int s = socket(AF_UNIX, SOCK_DGRAM, 0);
    int t = socket(AF_UNIX, SOCK_DGRAM, 0);
    int st = socket(AF_UNIX, SOCK_STREAM, 0);
    char name[64];
    snprintf(name, sizeof name, "abi-unix-%d", (int)getpid());
    socklen_t al = sun_of(&a, name, 1);
    R("bind abstract", bind(s, (struct sockaddr *)&a, al));
    R("bind abstract twice", bind(s, (struct sockaddr *)&a, al));
    R("bind abstract taken", bind(t, (struct sockaddr *)&a, al));
    R("bind abstract, other type", bind(st, (struct sockaddr *)&a, al));
    l = sizeof got;
    R("getsockname", getsockname(s, (struct sockaddr *)&got, &l));
    printf("  len equal: %s, bytes equal: %s\n", l == al ? "yes" : "no", memcmp(&got, &a, al) ? "no" : "yes");
    int u = socket(AF_UNIX, SOCK_DGRAM, 0);
    l = sizeof got;
    R("getsockname unbound", getsockname(u, (struct sockaddr *)&got, &l));
    printf("  len %u family %d\n", l, got.sun_family);
    sa_family_t fam = AF_UNIX;
    R("bind autobind", bind(u, (struct sockaddr *)&fam, sizeof fam));
    l = sizeof got;
    getsockname(u, (struct sockaddr *)&got, &l);
    int hex = 1;
    for (int i = 1; i < 6; i++)
        hex &= strchr("0123456789abcdef", got.sun_path[i]) != NULL;
    printf("  autobind len %u, leading NUL %s, five hex digits %s\n", l, got.sun_path[0] == 0 ? "yes" : "no", hex ? "yes" : "no");
    R("sendto by name", sendto(u, "ping", 4, 0, (struct sockaddr *)&a, al));
    struct sockaddr_un from;
    socklen_t fl = sizeof from;
    char b[16];
    R("recvfrom", recvfrom(s, b, sizeof b, 0, (struct sockaddr *)&from, &fl));
    printf("  sender address is the autobind name: %s\n", fl == l && !memcmp(&from, &got, l) ? "yes" : "no");
    struct sockaddr_un nob;
    socklen_t nl = sun_of(&nob, "abi-unix-nobody-here", 1);
    R("sendto nobody", sendto(u, "x", 1, 0, (struct sockaddr *)&nob, nl));
    R("send unconnected", send(t, "x", 1, 0));
    R("connect s -> u", connect(s, (struct sockaddr *)&got, l));
    R("sendto s from t (s connected elsewhere)", sendto(t, "x", 1, 0, (struct sockaddr *)&a, al));
    R("sendto s from its peer", sendto(u, "ok", 2, 0, (struct sockaddr *)&a, al));
    struct sockaddr unspec = { AF_UNSPEC };
    R("connect AF_UNSPEC", connect(s, &unspec, sizeof unspec));
    l = sizeof got;
    R("getpeername after disconnect", getpeername(s, (struct sockaddr *)&got, &l));
    R("sendto s from t now", sendto(t, "x", 1, 0, (struct sockaddr *)&a, al));
    struct sockaddr_un bad;
    memset(&bad, 0, sizeof bad);
    bad.sun_family = AF_INET;
    strcpy(bad.sun_path, "/tmp/x");
    R("bind wrong family", bind(t, (struct sockaddr *)&bad, sizeof bad));
    R("bind length 1", bind(t, (struct sockaddr *)&a, 1));
    R("bind length 111", bind(t, (struct sockaddr *)&a, 111));
    close(s);
    close(t);
    close(u);
    close(st);

    printf("-- path names\n");
    const char *p = tpath("sock");
    int ps = socket(AF_UNIX, SOCK_STREAM, 0);
    al = sun_of(&a, p, 0);
    mode_t old = umask(022);
    R("bind path", bind(ps, (struct sockaddr *)&a, al));
    umask(old);
    struct stat sb;
    R("stat socket file", stat(p, &sb));
    printf("  socket: %s mode %o\n", S_ISSOCK(sb.st_mode) ? "yes" : "no", sb.st_mode & 07777);
    l = sizeof got;
    getsockname(ps, (struct sockaddr *)&got, &l);
    printf("  getsockname len is strlen+3: %s\n", l == strlen(p) + 3 ? "yes" : "no");
    int ps2 = socket(AF_UNIX, SOCK_STREAM, 0);
    R("bind existing path", bind(ps2, (struct sockaddr *)&a, al));
    struct sockaddr_un a2;
    socklen_t al2 = sun_of(&a2, tpath("second"), 0);
    R("bind a bound socket to a second path", bind(ps, (struct sockaddr *)&a2, al2));
    printf("  second file left behind: %s\n", access(tpath("second"), F_OK) == 0 ? "yes" : "no");
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    R("connect, not listening", connect(c, (struct sockaddr *)&a, al));
    int dg = socket(AF_UNIX, SOCK_DGRAM, 0);
    R("connect stream path from dgram", connect(dg, (struct sockaddr *)&a, al));
    struct sockaddr_un miss;
    socklen_t ml = sun_of(&miss, tpath("missing"), 0);
    R("connect missing path", connect(c, (struct sockaddr *)&miss, ml));
    struct sockaddr_un deep;
    socklen_t dl = sun_of(&deep, tpath("nodir/sock"), 0);
    R("bind in a missing directory", bind(ps2, (struct sockaddr *)&deep, dl));
    int f = open(tpath("plain"), O_CREAT | O_WRONLY, 0600);
    close(f);
    struct sockaddr_un pl;
    socklen_t pll = sun_of(&pl, tpath("plain"), 0);
    R("connect to a regular file", connect(c, (struct sockaddr *)&pl, pll));
    close(ps);
    R("connect after the bound socket closed", connect(c, (struct sockaddr *)&a, al));
    printf("  file still there: %s\n", access(p, F_OK) == 0 ? "yes" : "no");
    unlink(p);
    unlink(tpath("plain"));
    close(ps2);
    close(c);
    close(dg);
}

static void listen_part(void) {
    struct sockaddr_un a, got;
    socklen_t al = sun_of(&a, tpath("listen"), 0), l;
    int srv = socket(AF_UNIX, SOCK_STREAM, 0);
    R("listen unbound", listen(srv, 5));
    R("accept, not listening", accept(srv, 0, 0));
    int dg = socket(AF_UNIX, SOCK_DGRAM, 0);
    R("listen dgram", listen(dg, 5));
    R("accept dgram", accept(dg, 0, 0));
    close(dg);
    R("bind", bind(srv, (struct sockaddr *)&a, al));
    R("listen(1)", listen(srv, 1));
    printf("  SO_ACCEPTCONN %d\n", sopt(srv, SOL_SOCKET, SO_ACCEPTCONN));
    int c1 = socket(AF_UNIX, SOCK_STREAM, 0);
    R("connect first", connect(c1, (struct sockaddr *)&a, al));
    R("connect again", connect(c1, (struct sockaddr *)&a, al));
    int c2 = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    R("connect second, backlog 1, non-blocking", connect(c2, (struct sockaddr *)&a, al));
    int c4 = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
    R("connect third, backlog 1 full, non-blocking", connect(c4, (struct sockaddr *)&a, al));
    struct sockaddr_un nl;
    socklen_t nll = sun_of(&nl, tpath("nolisten"), 0);
    R("connect a connected socket to a missing path", connect(c1, (struct sockaddr *)&nl, nll));
    close(c4);
    R("send before accept", send(c1, "early", 5, 0));
    revents("listener with one waiting", srv, POLLIN | POLLOUT);
    struct sockaddr_un pa;
    socklen_t pal = sizeof pa;
    int s = accept4(srv, (struct sockaddr *)&pa, &pal, SOCK_CLOEXEC | SOCK_NONBLOCK);
    printf("accept4 ok: %s, peer len %u, cloexec %s, nonblock %s\n", s >= 0 ? "yes" : "no", pal,
        (fcntl(s, F_GETFD) & FD_CLOEXEC) ? "yes" : "no", (fcntl(s, F_GETFL) & O_NONBLOCK) ? "yes" : "no");
    char b[16] = {0};
    R("recv early data", recv(s, b, sizeof b, 0));
    l = sizeof got;
    getsockname(s, (struct sockaddr *)&got, &l);
    printf("  accepted socket's name is the listener's: %s\n", l == al && !memcmp(&got, &a, al) ? "yes" : "no");
    l = sizeof got;
    getpeername(c1, (struct sockaddr *)&got, &l);
    printf("  client's peer is the listener's path: %s\n", l == al && !memcmp(&got, &a, al) ? "yes" : "no");
    struct ucred cr;
    socklen_t crl = sizeof cr;
    R("SO_PEERCRED of accepted", getsockopt(s, SOL_SOCKET, SO_PEERCRED, &cr, &crl));
    printf("  len %u pid is mine %s uid is mine %s gid is mine %s\n", crl, cr.pid == getpid() ? "yes" : "no", cr.uid == getuid() ? "yes" : "no", cr.gid == getgid() ? "yes" : "no");
    crl = sizeof cr;
    R("SO_PEERCRED of client", getsockopt(c1, SOL_SOCKET, SO_PEERCRED, &cr, &crl));
    printf("  pid is mine %s\n", cr.pid == getpid() ? "yes" : "no");
    crl = sizeof cr;
    int lone = socket(AF_UNIX, SOCK_STREAM, 0);
    R("SO_PEERCRED unconnected", getsockopt(lone, SOL_SOCKET, SO_PEERCRED, &cr, &crl));
    printf("  pid %d uid %d gid %d\n", cr.pid, (int)cr.uid, (int)cr.gid);
    close(lone);
    int s2 = accept(srv, 0, 0);
    printf("accept the second: %s\n", s2 >= 0 ? "ok" : sename(errno));
    close(s2);
    int fl = fcntl(srv, F_GETFL);
    fcntl(srv, F_SETFL, fl | O_NONBLOCK);
    R("accept nothing waiting (non-blocking)", accept(srv, 0, 0));
    R("accept4 bad flags", accept4(srv, 0, 0, 0x100));
    int c3 = socket(AF_UNIX, SOCK_STREAM, 0);
    R("connect third", connect(c3, (struct sockaddr *)&a, al));
    R("close listener with one waiting", close(srv));
    R("recv on the never-accepted connection", recv(c3, b, sizeof b, MSG_DONTWAIT));
    R("send on it", send(c3, "x", 1, MSG_NOSIGNAL));
    unlink(tpath("listen"));
    close(c1);
    close(c2);
    close(c3);
    close(s);
}

static int send_fds(int sock, const int *fds, int n, const char *data) {
    char ctl[CMSG_SPACE(sizeof(int) * 256)];
    memset(ctl, 0, sizeof ctl);
    struct iovec iov = { (void *)data, strlen(data) };
    struct msghdr m = { 0 };
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = ctl;
    m.msg_controllen = CMSG_SPACE(sizeof(int) * n);
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int) * n);
    memcpy(CMSG_DATA(c), fds, sizeof(int) * n);
    return sendmsg(sock, &m, 0);
}

/* recvmsg with `ctl` bytes of control space; prints what came; returns the first fd or -1. */
static int recv_fds(const char *label, int sock, size_t ctl, int flags) {
    char buf[16] = {0};
    char space[CMSG_SPACE(sizeof(int) * 16)];
    memset(space, 0, sizeof space);
    struct iovec iov = { buf, sizeof buf };
    struct msghdr m = { 0 };
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = ctl ? space : NULL;
    m.msg_controllen = ctl;
    long r = recvmsg(sock, &m, flags);
    printf("%s = %ld%s%s\n", label, r, r < 0 ? " -" : "", r < 0 ? sename(errno) : "");
    printf("  msg_flags %#x controllen %zu\n", m.msg_flags, (size_t)m.msg_controllen);
    int first = -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&m); c; c = CMSG_NXTHDR(&m, c)) {
        printf("  cmsg level %d type %d len %zu\n", c->cmsg_level, c->cmsg_type, (size_t)c->cmsg_len);
        if (c->cmsg_type == SCM_RIGHTS) {
            int k = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
            for (int i = 0; i < k; i++) {
                int fd;
                memcpy(&fd, CMSG_DATA(c) + i * sizeof(int), sizeof fd);
                if (first < 0)
                    first = fd;
                else
                    close(fd);
                printf("  fd %d: valid %s cloexec %s\n", i, fcntl(fd, F_GETFD) >= 0 ? "yes" : "no", (fcntl(first, F_GETFD) & FD_CLOEXEC) ? "yes" : "no");
            }
        }
    }
    fflush(stdout);
    return first;
}

static void rights_part(void) {
    int sv[2];
    R("socketpair(STREAM)", socketpair(AF_UNIX, SOCK_STREAM, 0, sv));
    const char *p = tpath("payload");
    int f = open(p, O_CREAT | O_RDWR | O_TRUNC, 0600);
    write(f, "0123456789", 10);
    lseek(f, 2, SEEK_SET);
    int two[2] = { f, f };
    R("sendmsg 2 fds", send_fds(sv[0], two, 2, "F"));
    R("close the original", close(f));
    int g = recv_fds("recvmsg with room for 2, MSG_CMSG_CLOEXEC", sv[1], CMSG_SPACE(2 * sizeof(int)), MSG_CMSG_CLOEXEC);
    char b[8] = {0};
    R("read at the shared offset", read(g, b, 3));
    printf("  read '%.3s' (offset 2 came along)\n", b);
    R("offset now", lseek(g, 0, SEEK_CUR));
    R("sendmsg 1 fd", send_fds(sv[0], &g, 1, "G"));
    int gg[2] = { g, g };
    R("sendmsg 2 fds", send_fds(sv[0], gg, 2, "H"));
    recv_fds("recvmsg G", sv[1], CMSG_SPACE(sizeof(int)), 0);
    recv_fds("recvmsg H with room for one fd", sv[1], CMSG_LEN(sizeof(int)), 0);
    R("sendmsg 1 fd", send_fds(sv[0], &g, 1, "I"));
    recv_fds("recvmsg without control buffer", sv[1], 0, 0);
    R("sendmsg 1 fd", send_fds(sv[0], &g, 1, "J"));
    recv_fds("recvmsg control below one header", sv[1], 8, 0);
    R("sendmsg 1 fd", send_fds(sv[0], &g, 1, "K"));
    int pk = recv_fds("recvmsg MSG_PEEK", sv[1], CMSG_SPACE(sizeof(int)), MSG_PEEK);
    if (pk >= 0)
        close(pk);
    pk = recv_fds("recvmsg after peek", sv[1], CMSG_SPACE(sizeof(int)), 0);
    if (pk >= 0)
        close(pk);
    int bad = 999;
    R("sendmsg a closed fd", send_fds(sv[0], &bad, 1, "x"));
    static int many[254];
    for (int i = 0; i < 254; i++)
        many[i] = g;
    R("sendmsg 254 fds", send_fds(sv[0], many, 254, "x"));
    R("sendmsg 253 fds", send_fds(sv[0], many, 253, "x"));
    int k = recv_fds("recvmsg 253 into room for 16", sv[1], CMSG_SPACE(16 * sizeof(int)), 0);
    if (k >= 0)
        close(k);
    for (int fd = 3; fd < 512; fd++)
        if (fd != sv[0] && fd != sv[1] && fd != g)
            close(fd);
    printf("-- in flight while the receiver closes\n");
    R("sendmsg", send_fds(sv[0], &g, 1, "L"));
    R("close receiver", close(sv[1]));
    R("send to it", send(sv[0], "x", 1, MSG_NOSIGNAL));
    close(sv[0]);
    printf("-- a socket sent over itself\n");
    R("socketpair(DGRAM)", socketpair(AF_UNIX, SOCK_DGRAM, 0, sv));
    R("send own end over itself", send_fds(sv[0], &sv[1], 1, "s"));
    int self = recv_fds("recvmsg", sv[1], CMSG_SPACE(sizeof(int)), 0);
    R("the received end works", send(sv[0], "x", 1, 0));
    R("recv on it", recv(self, b, 1, 0));
    close(self);
    R("send own end again and close both", send_fds(sv[0], &sv[1], 1, "t"));
    close(sv[0]);
    close(sv[1]);
    R("another pair after the cycle", socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv));
    R("seqpacket fd passing", send_fds(sv[0], &g, 1, "seq"));
    k = recv_fds("recvmsg seqpacket", sv[1], CMSG_SPACE(sizeof(int)), 0);
    if (k >= 0)
        close(k);
    close(sv[0]);
    close(sv[1]);
    close(g);
    unlink(p);
}

static void creds_part(void) {
    int sv[2];
    R("socketpair(DGRAM)", socketpair(AF_UNIX, SOCK_DGRAM, 0, sv));
    int one = 1;
    R("SO_PASSCRED on b", setsockopt(sv[1], SOL_SOCKET, SO_PASSCRED, &one, sizeof one));
    printf("  SO_PASSCRED %d\n", sopt(sv[1], SOL_SOCKET, SO_PASSCRED));
    R("send", send(sv[0], "c", 1, 0));
    char buf[8];
    char space[CMSG_SPACE(sizeof(struct ucred)) + 8];
    struct iovec iov = { buf, sizeof buf };
    struct msghdr m = { 0 };
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = space;
    m.msg_controllen = sizeof space;
    R("recvmsg", recvmsg(sv[1], &m, 0));
    printf("  controllen %zu flags %#x\n", (size_t)m.msg_controllen, m.msg_flags);
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    if (c) {
        struct ucred u;
        memcpy(&u, CMSG_DATA(c), sizeof u);
        printf("  cmsg type %d len %zu pid mine %s uid mine %s gid mine %s\n", c->cmsg_type, (size_t)c->cmsg_len, u.pid == getpid() ? "yes" : "no", u.uid == getuid() ? "yes" : "no", u.gid == getgid() ? "yes" : "no");
    }
    R("send", send(sv[0], "d", 1, 0));
    m.msg_controllen = CMSG_LEN(4);
    R("recvmsg with a short control buffer", recvmsg(sv[1], &m, 0));
    printf("  controllen %zu flags %#x\n", (size_t)m.msg_controllen, m.msg_flags);
    R("send", send(sv[0], "e", 1, 0));
    m.msg_control = NULL;
    m.msg_controllen = 0;
    R("recvmsg without control", recvmsg(sv[1], &m, 0));
    printf("  flags %#x\n", m.msg_flags);
    char cs[CMSG_SPACE(sizeof(struct ucred))];
    memset(cs, 0, sizeof cs);
    struct msghdr sm = { 0 };
    struct iovec si = { "f", 1 };
    sm.msg_iov = &si;
    sm.msg_iovlen = 1;
    sm.msg_control = cs;
    sm.msg_controllen = sizeof cs;
    struct cmsghdr *h = CMSG_FIRSTHDR(&sm);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_CREDENTIALS;
    h->cmsg_len = CMSG_LEN(sizeof(struct ucred));
    struct ucred mine = { getpid(), getuid(), getgid() };
    memcpy(CMSG_DATA(h), &mine, sizeof mine);
    R("sendmsg own SCM_CREDENTIALS", sendmsg(sv[0], &sm, 0));
    struct ucred other = { getpid(), getuid() + 1, getgid() };
    memcpy(CMSG_DATA(h), &other, sizeof other);
    long r = sendmsg(sv[0], &sm, 0);
    printf("sendmsg another uid: %s\n", getuid() == 0 ? (r >= 0 ? "allowed (root)" : "refused") : (r < 0 && errno == EPERM ? "EPERM" : "allowed"));
    h->cmsg_len = CMSG_LEN(8);
    R("sendmsg SCM_CREDENTIALS of the wrong length", sendmsg(sv[0], &sm, 0));
    h->cmsg_len = CMSG_LEN(sizeof(struct ucred));
    h->cmsg_type = 77;
    R("sendmsg unknown SOL_SOCKET type", sendmsg(sv[0], &sm, 0));
    h->cmsg_level = 1234;
    R("sendmsg other level (ignored)", sendmsg(sv[0], &sm, 0));
    h->cmsg_len = 4;
    R("sendmsg cmsg_len below a header", sendmsg(sv[0], &sm, 0));
    close(sv[0]);
    close(sv[1]);
    printf("-- stream: messages of different senders are not glued with SO_PASSCRED\n");
    R("socketpair(STREAM)", socketpair(AF_UNIX, SOCK_STREAM, 0, sv));
    R("SO_PASSCRED", setsockopt(sv[1], SOL_SOCKET, SO_PASSCRED, &one, sizeof one));
    send(sv[0], "ab", 2, 0);
    send(sv[0], "cd", 2, 0);
    m.msg_control = space;
    m.msg_controllen = sizeof space;
    iov.iov_len = sizeof buf;
    R("recvmsg glues one sender's writes", recvmsg(sv[1], &m, 0));
    close(sv[0]);
    close(sv[1]);
}

static void opts_part(void) {
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    printf("SO_TYPE %d SO_DOMAIN %d SO_PROTOCOL %d\n", sopt(s, SOL_SOCKET, SO_TYPE), sopt(s, SOL_SOCKET, SO_DOMAIN), sopt(s, SOL_SOCKET, SO_PROTOCOL));
    printf("SO_SNDBUF %d SO_RCVBUF %d SO_ERROR %d SO_PASSCRED %d SO_ACCEPTCONN %d\n", sopt(s, SOL_SOCKET, SO_SNDBUF), sopt(s, SOL_SOCKET, SO_RCVBUF),
        sopt(s, SOL_SOCKET, SO_ERROR), sopt(s, SOL_SOCKET, SO_PASSCRED), sopt(s, SOL_SOCKET, SO_ACCEPTCONN));
    int v = 1000;
    R("SO_SNDBUF 1000", setsockopt(s, SOL_SOCKET, SO_SNDBUF, &v, sizeof v));
    printf("  SO_SNDBUF %d\n", sopt(s, SOL_SOCKET, SO_SNDBUF));
    v = 100000;
    R("SO_RCVBUF 100000", setsockopt(s, SOL_SOCKET, SO_RCVBUF, &v, sizeof v));
    printf("  SO_RCVBUF %d\n", sopt(s, SOL_SOCKET, SO_RCVBUF));
    v = -1;
    R("SO_SNDBUF -1", setsockopt(s, SOL_SOCKET, SO_SNDBUF, &v, sizeof v));
    printf("  SO_SNDBUF %d\n", sopt(s, SOL_SOCKET, SO_SNDBUF));
    R("SO_TYPE set", setsockopt(s, SOL_SOCKET, SO_TYPE, &v, sizeof v));
    R("unknown option", setsockopt(s, SOL_SOCKET, 999, &v, sizeof v));
    R("optlen 2", setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &v, 2));
    R("SOL_TCP on AF_UNIX", setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &v, sizeof v));
    socklen_t l = sizeof v;
    R("getsockopt SOL_TCP on AF_UNIX", getsockopt(s, IPPROTO_TCP, TCP_NODELAY, &v, &l));
    R("getsockopt unknown", getsockopt(s, SOL_SOCKET, 999, &v, &l));
    l = (socklen_t)-1;
    R("getsockopt negative len", getsockopt(s, SOL_SOCKET, SO_TYPE, &v, &l));
    short sh = 0;
    l = sizeof sh;
    R("getsockopt into 2 bytes", getsockopt(s, SOL_SOCKET, SO_TYPE, &sh, &l));
    printf("  len %u value %d\n", l, sh);
    l = 100;
    v = 0;
    R("getsockopt with a big len", getsockopt(s, SOL_SOCKET, SO_TYPE, &v, &l));
    printf("  len %u\n", l);
    struct timeval tv = { 1, 1500 }, back;
    R("SO_RCVTIMEO 1.0015 s", setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv));
    l = sizeof back;
    getsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &back, &l);
    printf("  back %ld s %ld us\n", (long)back.tv_sec, (long)back.tv_usec);
    tv.tv_usec = 1000000;
    R("SO_RCVTIMEO usec 1e6", setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv));
    R("SO_RCVTIMEO short optlen", setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &tv, 8));
    struct linger lg = { 1, 5 }, lb;
    R("SO_LINGER", setsockopt(s, SOL_SOCKET, SO_LINGER, &lg, sizeof lg));
    l = sizeof lb;
    getsockopt(s, SOL_SOCKET, SO_LINGER, &lb, &l);
    printf("  linger %d %d len %u\n", lb.l_onoff, lb.l_linger, l);
    int fds[2];
    socketpair(AF_UNIX, SOCK_DGRAM, 0, fds);
    tv.tv_sec = 0;
    tv.tv_usec = 30000;
    setsockopt(fds[1], SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    char b[4];
    R("recv with SO_RCVTIMEO 30 ms", recv(fds[1], b, 4, 0));
    yes("waited at least 30 ms", ns_of(CLOCK_MONOTONIC) - t0 >= 30000000);
    close(fds[0]);
    close(fds[1]);
    close(s);
    printf("-- errors\n");
    R("socket(AF_UNIX, 99)", socket(AF_UNIX, 99, 0));
    R("socket(AF_UNIX, SOCK_RDM)", socket(AF_UNIX, SOCK_RDM, 0));
    R("socket(AF_UNIX, STREAM, 7)", socket(AF_UNIX, SOCK_STREAM, 7));
    R("socket(AF_UNIX, STREAM|0x100)", socket(AF_UNIX, SOCK_STREAM | 0x100, 0));
    R("socket(46, STREAM)", socket(46, SOCK_STREAM, 0));
    int r = socket(AF_UNIX, SOCK_RAW, 0);
    printf("socket(AF_UNIX, SOCK_RAW): %s, SO_TYPE %d\n", r >= 0 ? "ok" : sename(errno), sopt(r, SOL_SOCKET, SO_TYPE));
    close(r);
    int f = open("/dev/null", O_RDWR);
    R("bind on /dev/null", bind(f, (struct sockaddr *)&(struct sockaddr_un){ AF_UNIX, "x" }, 4));
    R("listen on /dev/null", listen(f, 1));
    R("getsockopt on /dev/null", getsockopt(f, SOL_SOCKET, SO_TYPE, &v, &l));
    close(f);
    R("bind on fd 999", bind(999, 0, 0));
    int sv[2];
    R("socketpair into a kernel address", syscall(SYS_socketpair, AF_UNIX, SOCK_STREAM, 0, KERNEL_ADDR));
    R("socketpair(AF_UNIX, 99)", socketpair(AF_UNIX, 99, 0, sv));
    socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
    R("send from a kernel address", send(sv[0], KERNEL_ADDR, 4, 0));
    R("recv into a kernel address", recv(sv[1], KERNEL_ADDR, 4, MSG_DONTWAIT));
    send(sv[0], "abcd", 4, 0);
    R("recv into a kernel address with data", recv(sv[1], KERNEL_ADDR, 4, MSG_DONTWAIT));
    R("recv the data afterwards", recv(sv[1], b, 4, MSG_DONTWAIT));
    R("recvmsg MSG_CMSG_COMPAT", syscall(SYS_recvmsg, sv[1], &(struct msghdr){ 0 }, 0x80000000u));
    R("sendmsg bad msghdr", syscall(SYS_sendmsg, sv[0], KERNEL_ADDR, 0));
    struct msghdr m = { 0 };
    m.msg_iovlen = 1025;
    R("sendmsg 1025 iovecs", syscall(SYS_sendmsg, sv[0], &m, 0));
    close(sv[0]);
    close(sv[1]);
}

static void mmsg_part(void) {
    int sv[2];
    R("socketpair(DGRAM)", socketpair(AF_UNIX, SOCK_DGRAM, 0, sv));
    struct mmsghdr out[3];
    struct iovec oi[3] = { { "one", 3 }, { "two!", 4 }, { "three", 5 } };
    memset(out, 0, sizeof out);
    for (int i = 0; i < 3; i++) {
        out[i].msg_hdr.msg_iov = &oi[i];
        out[i].msg_hdr.msg_iovlen = 1;
    }
    R("sendmmsg 3", sendmmsg(sv[0], out, 3, 0));
    printf("  msg_len %u %u %u\n", out[0].msg_len, out[1].msg_len, out[2].msg_len);
    R("sendmmsg 0", sendmmsg(sv[0], out, 0, 0));
    struct mmsghdr in[4];
    char bufs[4][8];
    struct iovec ii[4];
    memset(in, 0, sizeof in);
    for (int i = 0; i < 4; i++) {
        ii[i].iov_base = bufs[i];
        ii[i].iov_len = 4;
        in[i].msg_hdr.msg_iov = &ii[i];
        in[i].msg_hdr.msg_iovlen = 1;
    }
    R("recvmmsg 4 (3 waiting), MSG_DONTWAIT", recvmmsg(sv[1], in, 4, MSG_DONTWAIT, 0));
    printf("  msg_len %u %u %u flags %#x %#x %#x\n", in[0].msg_len, in[1].msg_len, in[2].msg_len, in[0].msg_hdr.msg_flags, in[1].msg_hdr.msg_flags, in[2].msg_hdr.msg_flags);
    R("recvmmsg nothing waiting", recvmmsg(sv[1], in, 4, MSG_DONTWAIT, 0));
    send(sv[0], "x", 1, 0);
    R("recvmmsg MSG_WAITFORONE", recvmmsg(sv[1], in, 4, MSG_WAITFORONE, 0));
    struct timespec bad = { -1, 0 };
    R("recvmmsg bad timeout", recvmmsg(sv[1], in, 4, 0, &bad));
    R("recvmmsg timeout at a kernel address", syscall(SYS_recvmmsg, sv[1], in, 4, 0, KERNEL_ADDR));
    send(sv[0], "y", 1, 0);
    struct timespec ts = { 5, 0 };
    R("recvmmsg with a timeout, one waiting, MSG_WAITFORONE", recvmmsg(sv[1], in, 4, MSG_WAITFORONE, &ts));
    printf("  timeout left below 5 s: %s\n", ts.tv_sec < 5 || (ts.tv_sec == 5 && ts.tv_nsec == 0) ? "yes" : "no");
    close(sv[0]);
    close(sv[1]);
}

/* The raw calls: musl's sendmsg/recvmsg/sendmmsg/recvmmsg copy the msghdr (and control data) in user space
 * first, so a bad pointer would fault in libc instead of reaching the kernel. musl's struct msghdr is the
 * kernel's layout when its padding is zero (every msghdr here starts memset). */
#define XSENDMSG(fd, m, f) syscall(SYS_sendmsg, (fd), (m), (f))
#define XRECVMSG(fd, m, f) syscall(SYS_recvmsg, (fd), (m), (f))
#define XSENDMMSG(fd, v, n, f) syscall(SYS_sendmmsg, (fd), (v), (n), (f))
#define XRECVMMSG(fd, v, n, f, t) syscall(SYS_recvmmsg, (fd), (v), (n), (f), (t))

/* A control buffer for hand-made cmsg headers (aligned like struct cmsghdr). */
union ctlbuf {
    struct cmsghdr h;
    char b[128];
};

static struct msghdr one_iov(struct iovec *iov) {
    struct msghdr m;
    memset(&m, 0, sizeof m);
    m.msg_iov = iov;
    m.msg_iovlen = 1;
    return m;
}

/* Bad pointers and lengths in every place a socket call reads or writes program memory. */
static void hostile_part(void) {
    int sv[2];
    R("socketpair(DGRAM)", socketpair(AF_UNIX, SOCK_DGRAM, 0, sv));
    char buf[64] = "data";
    struct iovec iov = { buf, 4 };
    struct msghdr m = one_iov(&iov);
    R("sendmsg msghdr at a kernel address", XSENDMSG(sv[0], KERNEL_ADDR, 0));
    R("recvmsg msghdr at address 8", XRECVMSG(sv[1], (void *)8, MSG_DONTWAIT));
    m.msg_iov = KERNEL_ADDR;
    R("sendmsg iov array at a kernel address", XSENDMSG(sv[0], &m, 0));
    m.msg_iov = (void *)8;
    R("sendmsg iov array at address 8", XSENDMSG(sv[0], &m, 0));
    m = one_iov(&iov);
    m.msg_iovlen = 1025;
    R("sendmsg iovlen 1025", XSENDMSG(sv[0], &m, 0));
    R("recvmsg iovlen 1025", XRECVMSG(sv[1], &m, MSG_DONTWAIT));
    struct iovec bad = { KERNEL_ADDR, 4 };
    m = one_iov(&bad);
    R("sendmsg data at a kernel address", XSENDMSG(sv[0], &m, 0));
    struct iovec neg = { buf, (size_t)-1 };
    m = one_iov(&neg);
    R("sendmsg iov_len -1", XSENDMSG(sv[0], &m, 0));
    struct iovec none = { KERNEL_ADDR, 0 };
    m = one_iov(&none);
    R("sendmsg 0 bytes at a kernel address", XSENDMSG(sv[0], &m, 0));
    R("recv the empty datagram", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));
    R("send from a kernel address", send(sv[0], KERNEL_ADDR, 4, 0));

    int u = socket(AF_UNIX, SOCK_DGRAM, 0);
    struct sockaddr_un sa;
    memset(&sa, 0, sizeof sa);
    sa.sun_family = AF_UNIX;
    m = one_iov(&iov);
    m.msg_name = KERNEL_ADDR;
    m.msg_namelen = 16;
    R("sendmsg name at a kernel address", XSENDMSG(u, &m, 0));
    m.msg_name = &sa;
    m.msg_namelen = 129;
    R("sendmsg namelen 129", XSENDMSG(u, &m, 0));
    m.msg_namelen = (socklen_t)-1;
    R("sendmsg namelen -1", XSENDMSG(u, &m, 0));
    R("sendto address at a kernel address", sendto(u, buf, 4, 0, KERNEL_ADDR, 16));
    close(u);

    union ctlbuf c;
    memset(&c, 0, sizeof c);
    m = one_iov(&iov);
    m.msg_control = KERNEL_ADDR;
    m.msg_controllen = 24;
    R("sendmsg control at a kernel address", XSENDMSG(sv[0], &m, 0));
    m.msg_control = c.b;
    m.msg_controllen = CMSG_SPACE(sizeof(int));
    c.h.cmsg_level = SOL_SOCKET;
    c.h.cmsg_type = SCM_RIGHTS;
    c.h.cmsg_len = 8;
    R("sendmsg cmsg_len below the header", XSENDMSG(sv[0], &m, 0));
    c.h.cmsg_len = 64;
    R("sendmsg cmsg_len past msg_controllen", XSENDMSG(sv[0], &m, 0));
    c.h.cmsg_len = CMSG_LEN(2);
    R("sendmsg SCM_RIGHTS with 2 bytes of fds", XSENDMSG(sv[0], &m, 0));
    R("recv it", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));
    c.h.cmsg_len = CMSG_LEN(sizeof(int));
    int minus = -1;
    memcpy(CMSG_DATA(&c.h), &minus, sizeof minus);
    R("sendmsg SCM_RIGHTS fd -1", XSENDMSG(sv[0], &m, 0));
    c.h.cmsg_type = 99;
    R("sendmsg SOL_SOCKET type 99", XSENDMSG(sv[0], &m, 0));
    c.h.cmsg_level = 99;
    R("sendmsg level 99 (not ours: skipped)", XSENDMSG(sv[0], &m, 0));
    R("recv it", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));
    m.msg_controllen = 8;
    R("sendmsg msg_controllen 8 (no whole header)", XSENDMSG(sv[0], &m, 0));
    R("recv it", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));
    static char huge[1 << 20];
    m.msg_control = huge;
    m.msg_controllen = sizeof huge;
    R("sendmsg msg_controllen 1 MiB", XSENDMSG(sv[0], &m, 0));
    R("recv nothing", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));

    /* Receiving into bad memory: a datagram whose copy fails is gone (unix_dgram_recvmsg frees it). */
    R("send abcd", send(sv[0], "abcd", 4, 0));
    m = one_iov(&bad);
    R("recvmsg into a kernel address", XRECVMSG(sv[1], &m, MSG_DONTWAIT));
    R("recv after the failed check", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));
    R("send abcd", send(sv[0], "abcd", 4, 0));
    struct iovec low = { (void *)8, 4 };
    m = one_iov(&low);
    R("recvmsg into address 8 (the copy faults)", XRECVMSG(sv[1], &m, MSG_DONTWAIT));
    R("recv after the failed copy", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));
    R("send efgh", send(sv[0], "efgh", 4, 0));
    m = one_iov(&iov);
    m.msg_name = KERNEL_ADDR;
    m.msg_namelen = sizeof sa;
    R("recvmsg name to a kernel address", XRECVMSG(sv[1], &m, MSG_DONTWAIT));
    R("recv after it", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));
    R("recvfrom address length at a kernel address", recvfrom(sv[1], buf, sizeof buf, MSG_DONTWAIT, (void *)&sa, KERNEL_ADDR));
    /* An fd in flight with the control buffer unwritable: the data arrives, the fd does not. */
    /* (A socketpair stands for a pipe: V2 has no pipes yet. The far end sees EOF once its peer is closed.) */
    int p[2];
    R("socketpair(STREAM) as the passed object", socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, p));
    R("sendmsg one end of it", send_fds(sv[0], &p[1], 1, "W"));
    close(p[1]);
    m = one_iov(&iov);
    m.msg_control = KERNEL_ADDR;
    m.msg_controllen = 64;
    R("recvmsg with control at a kernel address", XRECVMSG(sv[1], &m, MSG_DONTWAIT));
    printf("  MSG_CTRUNC %s\n", m.msg_flags & MSG_CTRUNC ? "set" : "clear");
    R("recv on the other end: the passed end was closed", recv(p[0], buf, 4, 0));
    close(p[0]);

    struct mmsghdr mm[2];
    memset(mm, 0, sizeof mm);
    char d0[4] = "m0", d1[4];
    struct iovec i0 = { d0, 2 }, i1 = { KERNEL_ADDR, 2 };
    mm[0].msg_hdr = one_iov(&i0);
    mm[1].msg_hdr = one_iov(&i1);
    R("sendmmsg, the second entry's data at a kernel address", XSENDMMSG(sv[0], mm, 2, 0));
    printf("  first msg_len %u\n", mm[0].msg_len);
    R("sendmmsg vector at a kernel address", XSENDMMSG(sv[0], KERNEL_ADDR, 2, 0));
    R("recvmmsg vector at a kernel address", XRECVMMSG(sv[1], KERNEL_ADDR, 2, MSG_DONTWAIT, NULL));
    R("recvmmsg vlen 0", XRECVMMSG(sv[1], mm, 0, MSG_DONTWAIT, NULL));
    struct iovec r0 = { d1, 4 };
    mm[0].msg_hdr = one_iov(&r0);
    mm[1].msg_hdr = one_iov(&bad);
    R("recvmmsg, the second entry's buffer at a kernel address", XRECVMMSG(sv[1], mm, 2, MSG_DONTWAIT, NULL));
    R("send", send(sv[0], "n1", 2, 0));
    R("send", send(sv[0], "n2", 2, 0));
    R("recvmmsg, the second entry's buffer at a kernel address", XRECVMMSG(sv[1], mm, 2, MSG_DONTWAIT, NULL));
    R("recv what is left", recv(sv[1], buf, sizeof buf, MSG_DONTWAIT));

    /* The other calls' pointers. */
    int before = dup(0);
    close(before);
    R("socketpair into a kernel address", socketpair(AF_UNIX, SOCK_STREAM, 0, KERNEL_ADDR));
    int after = dup(0);
    close(after);
    printf("  no fds left behind: %s\n", before == after ? "yes" : "no");
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    R("bind from a kernel address", bind(s, KERNEL_ADDR, 16));
    R("bind 200-byte address", bind(s, (void *)&sa, 200));
    R("connect from a kernel address", connect(s, KERNEL_ADDR, 16));
    socklen_t l = sizeof sa;
    R("getsockname to a kernel address", getsockname(s, KERNEL_ADDR, &l));
    R("getsockname length at a kernel address", getsockname(s, (void *)&sa, KERNEL_ADDR));
    l = (socklen_t)-1;
    R("getsockname length -1", getsockname(s, (void *)&sa, &l));
    int v;
    l = sizeof v;
    R("getsockopt value to a kernel address", getsockopt(s, SOL_SOCKET, SO_TYPE, KERNEL_ADDR, &l));
    R("getsockopt length at a kernel address", getsockopt(s, SOL_SOCKET, SO_TYPE, &v, KERNEL_ADDR));
    l = (socklen_t)-1;
    R("getsockopt length -1", getsockopt(s, SOL_SOCKET, SO_TYPE, &v, &l));
    R("setsockopt value at a kernel address", setsockopt(s, SOL_SOCKET, SO_PASSCRED, KERNEL_ADDR, 4));
    R("setsockopt length 2", setsockopt(s, SOL_SOCKET, SO_PASSCRED, &v, 2));
    R("setsockopt length -1", setsockopt(s, SOL_SOCKET, SO_PASSCRED, &v, (socklen_t)-1));
    /* accept with an unwritable address: the connection is taken and dropped, the client sees the end. */
    socklen_t al = sun_of(&sa, "nw5-hostile-accept", 1);
    R("bind abstract", bind(s, (void *)&sa, al));
    R("listen", listen(s, 1));
    int cl = socket(AF_UNIX, SOCK_STREAM, 0);
    R("connect", connect(cl, (void *)&sa, al));
    l = sizeof sa;
    R("accept to a kernel address", accept(s, KERNEL_ADDR, &l));
    R("client recv", recv(cl, buf, sizeof buf, MSG_DONTWAIT));
    R("accept4 with flags 1", accept4(s, NULL, NULL, 1));
    close(cl);
    close(s);
    close(sv[0]);
    close(sv[1]);
}

/* Size limits, the in-flight limit, and closing with files in flight. */
static void limits_part(void) {
    static char big[300000];
    int sv[2];
    R("socketpair(DGRAM)", socketpair(AF_UNIX, SOCK_DGRAM, 0, sv));
    R("send 300000 bytes as one datagram", send(sv[0], big, sizeof big, MSG_DONTWAIT));
    int sb = sopt(sv[0], SOL_SOCKET, SO_SNDBUF);
    R("send SO_SNDBUF - 32 bytes", send(sv[0], big, sb - 32, MSG_DONTWAIT));
    R("send SO_SNDBUF - 31 bytes", send(sv[0], big, sb - 31, MSG_DONTWAIT));
    close(sv[0]);
    close(sv[1]);
    R("socketpair(SEQPACKET)", socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sv));
    R("send 300000 bytes as one record", send(sv[0], big, sizeof big, MSG_DONTWAIT));
    close(sv[0]);
    close(sv[1]);

    /* Files in flight per user: above RLIMIT_NOFILE (lowered to 16 here) a send is ETOOMANYREFS. The count is
     * the user's, so it is printed as a range (another program of the same user may have one in flight). */
    struct rlimit rl;
    getrlimit(RLIMIT_NOFILE, &rl);
    struct rlimit low = { 16, rl.rlim_max };
    R("setrlimit(RLIMIT_NOFILE, 16)", setrlimit(RLIMIT_NOFILE, &low));
    R("socketpair(STREAM)", socketpair(AF_UNIX, SOCK_STREAM, 0, sv));
    int sent = 0, err = 0;
    for (int i = 0; i < 100; i++) {
        if (send_fds(sv[0], &sv[0], 1, "r") < 0) {
            err = errno;
            break;
        }
        sent++;
    }
    printf("sends of one fd each until %s; between 10 and 17 of them: %s\n", err ? sename(err) : "none failed", sent >= 10 && sent <= 17 ? "yes" : "no");
    /* Closing the receiver closes what it held (the cycle through sv[0] is broken by that too). */
    close(sv[1]);
    R("send after the receiver closed with files queued", send(sv[0], "x", 1, MSG_NOSIGNAL));
    close(sv[0]);
    setrlimit(RLIMIT_NOFILE, &rl);

    /* The peer closes with unread data that holds one end of another pair: ECONNRESET once, then EOF, EPIPE;
     * the passed end was closed with the queue (its partner sees EOF). */
    int p[2];
    R("socketpair(STREAM)", socketpair(AF_UNIX, SOCK_STREAM, 0, sv));
    R("socketpair(STREAM) as the passed object", socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, p));
    R("sendmsg one end of it", send_fds(sv[0], &p[1], 1, "P"));
    close(p[1]);
    char b[8];
    R("recv on the other end while the passed end is in flight", recv(p[0], b, sizeof b, 0));
    R("close the receiver", close(sv[1]));
    R("recv on the other end after", recv(p[0], b, sizeof b, 0));
    R("recv after the peer closed with unread data", recv(sv[0], b, sizeof b, MSG_DONTWAIT));
    R("recv again", recv(sv[0], b, sizeof b, MSG_DONTWAIT));
    R("send", send(sv[0], "x", 1, MSG_NOSIGNAL));
    close(sv[0]);
    close(p[0]);
}

/* ioctls on AF_UNIX sockets and the SOL_SOCKET options added after the first round. */
static void ioctl_part(void) {
    int sv[2];
    R("socketpair(DGRAM)", socketpair(AF_UNIX, SOCK_DGRAM, 0, sv));
    int on = 1, off = 0, n = -7;
    char b[16];
    R("ioctl(FIONBIO, 1)", ioctl(sv[1], FIONBIO, &on));
    printf("  O_NONBLOCK %s\n", fcntl(sv[1], F_GETFL) & O_NONBLOCK ? "set" : "clear");
    R("recv without MSG_DONTWAIT", recv(sv[1], b, sizeof b, 0));
    R("ioctl(FIONBIO, 0)", ioctl(sv[1], FIONBIO, &off));
    send(sv[0], "abc", 3, 0);
    send(sv[0], "defgh", 5, 0);
    R("ioctl(SIOCINQ)", ioctl(sv[1], FIONREAD, &n));
    printf("  SIOCINQ %d (the first datagram)\n", n);
    R("dgram ioctl(SIOCATMARK)", ioctl(sv[1], SIOCATMARK, &n));
    int st[2];
    socketpair(AF_UNIX, SOCK_STREAM, 0, st);
    n = -7;
    R("stream ioctl(SIOCATMARK)", ioctl(st[1], SIOCATMARK, &n));
    printf("  SIOCATMARK %d\n", n);
    close(st[0]);
    close(st[1]);
    n = -7;
    R("ioctl(SIOCGPGRP)", ioctl(sv[1], SIOCGPGRP, &n));
    printf("  SIOCGPGRP %d\n", n);
    struct timeval tv;
    R("ioctl(SIOCGSTAMP)", ioctl(sv[1], SIOCGSTAMP, &tv));
    R("ioctl(TCGETS)", ioctl(sv[1], TCGETS, b));

    int s = sv[1];
    printf("SO_TIMESTAMP %d SO_TIMESTAMPNS %d SO_BUSY_POLL %d SO_INCOMING_CPU %d\n", sopt(s, SOL_SOCKET, SO_TIMESTAMP), sopt(s, SOL_SOCKET, SO_TIMESTAMPNS),
        sopt(s, SOL_SOCKET, SO_BUSY_POLL), sopt(s, SOL_SOCKET, SO_INCOMING_CPU));
    R("set SO_TIMESTAMPNS", setsockopt(s, SOL_SOCKET, SO_TIMESTAMPNS, &on, sizeof on));
    printf("  SO_TIMESTAMP %d SO_TIMESTAMPNS %d\n", sopt(s, SOL_SOCKET, SO_TIMESTAMP), sopt(s, SOL_SOCKET, SO_TIMESTAMPNS));
    R("set SO_TIMESTAMP", setsockopt(s, SOL_SOCKET, SO_TIMESTAMP, &on, sizeof on));
    printf("  SO_TIMESTAMP %d SO_TIMESTAMPNS %d\n", sopt(s, SOL_SOCKET, SO_TIMESTAMP), sopt(s, SOL_SOCKET, SO_TIMESTAMPNS));
    R("clear SO_TIMESTAMP", setsockopt(s, SOL_SOCKET, SO_TIMESTAMP, &off, sizeof off));
    printf("  SO_TIMESTAMP %d SO_TIMESTAMPNS %d\n", sopt(s, SOL_SOCKET, SO_TIMESTAMP), sopt(s, SOL_SOCKET, SO_TIMESTAMPNS));
    struct { int flags, phc; } ts = { -1, -1 };
    socklen_t l = sizeof ts;
    R("get SO_TIMESTAMPING", getsockopt(s, SOL_SOCKET, SO_TIMESTAMPING, &ts, &l));
    printf("  length %u flags %d bind_phc %d\n", l, ts.flags, ts.phc);
    R("set SO_TIMESTAMPING 0", setsockopt(s, SOL_SOCKET, SO_TIMESTAMPING, &off, sizeof off));
    int m1 = -1, fifty = 50;
    R("set SO_BUSY_POLL -1", setsockopt(s, SOL_SOCKET, SO_BUSY_POLL, &m1, sizeof m1));
    R("set SO_BUSY_POLL 50", setsockopt(s, SOL_SOCKET, SO_BUSY_POLL, &fifty, sizeof fifty));
    printf("  SO_BUSY_POLL %d\n", sopt(s, SOL_SOCKET, SO_BUSY_POLL));
    int three = 3;
    R("set SO_INCOMING_CPU 3", setsockopt(s, SOL_SOCKET, SO_INCOMING_CPU, &three, sizeof three));
    printf("  SO_INCOMING_CPU %d\n", sopt(s, SOL_SOCKET, SO_INCOMING_CPU));
    char dev[32] = "x";
    l = sizeof dev;
    R("get SO_BINDTODEVICE", getsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, dev, &l));
    printf("  length %u\n", l);
    R("set SO_BINDTODEVICE \"\"", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "", 1));
    R("set SO_BINDTODEVICE length 0", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "", 0));
    R("set SO_BINDTODEVICE nw5nodev0", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, "nw5nodev0", 10));
    R("set SO_BINDTODEVICE from a kernel address", setsockopt(s, SOL_SOCKET, SO_BINDTODEVICE, KERNEL_ADDR, 4));
    gid_t g[64];
    l = sizeof g;
    int r = getsockopt(s, SOL_SOCKET, SO_PEERGROUPS, g, &l);
    printf("get SO_PEERGROUPS on a pair: %s, length a multiple of 4: %s\n", r == 0 ? "0" : sename(errno), l % 4 == 0 ? "yes" : "no");
    int lone = socket(AF_UNIX, SOCK_STREAM, 0);
    l = sizeof g;
    R("get SO_PEERGROUPS unconnected", getsockopt(lone, SOL_SOCKET, SO_PEERGROUPS, g, &l));
    close(lone);
    close(sv[0]);
    close(sv[1]);
}

int main(int argc, char **argv) {
    const char *part = argc > 1 ? argv[1] : "all";
    snprintf(tmpdir_path, sizeof tmpdir_path, "/tmp");
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("part: %s\n", part);
    if (!strcmp(part, "pair") || !strcmp(part, "all"))
        pair_part();
    if (!strcmp(part, "names") || !strcmp(part, "all"))
        names_part();
    if (!strcmp(part, "listen") || !strcmp(part, "all"))
        listen_part();
    if (!strcmp(part, "rights") || !strcmp(part, "all"))
        rights_part();
    if (!strcmp(part, "creds") || !strcmp(part, "all"))
        creds_part();
    if (!strcmp(part, "opts") || !strcmp(part, "all"))
        opts_part();
    if (!strcmp(part, "mmsg") || !strcmp(part, "all"))
        mmsg_part();
    if (!strcmp(part, "hostile") || !strcmp(part, "all"))
        hostile_part();
    if (!strcmp(part, "limits") || !strcmp(part, "all"))
        limits_part();
    if (!strcmp(part, "ioctl") || !strcmp(part, "all"))
        ioctl_part();
    if (!strcmp(part, "iov")) {
        /* write(2)/writev(2)/readv(2) on sockets are one message each (FS routes them to the socket layer). */
        int sv[2];
        R("socketpair(DGRAM)", socketpair(AF_UNIX, SOCK_DGRAM, 0, sv));
        struct iovec two[2] = { { "abc", 3 }, { "defg", 4 } };
        R("writev 2 segments", writev(sv[0], two, 2));
        char b[16] = {0};
        R("recv: one datagram", recv(sv[1], b, sizeof b, 0));
        printf("  got '%s'\n", b);
        static char big[100000];
        R("write 100000 bytes", write(sv[0], big, sizeof big));
        static char in[200000];
        R("recv it whole", recv(sv[1], in, sizeof in, 0));
        R("send", send(sv[0], "xyzuvw", 6, 0));
        char r1[2], r2[8] = {0};
        struct iovec rv[2] = { { r1, 2 }, { r2, 8 } };
        R("readv", readv(sv[1], rv, 2));
        printf("  '%.2s' '%s'\n", r1, r2);
        R("read on an unconnected stream socket", read(socket(AF_UNIX, SOCK_STREAM, 0), b, 4));
        close(sv[0]);
        close(sv[1]);
    }
    if (!strcmp(part, "sigpipe")) {
        int sv[2];
        socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
        close(sv[1]);
        puts("writing to a closed peer");
        fflush(stdout);
        write(sv[0], "x", 1);
        puts("not reached");
    }
    printf("end of part %s\n", part);
    return 0;
}
