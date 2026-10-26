/* NW-5: AF_INET TCP and UDP on 127.0.0.1 against Linux (NW-v6: AF_INET6 parts in inet6.h, ports 24640-24649). argv[1] picks a part. Ports 24620-24639 (lane NW's
 * range): they must be free on the machine that runs it. Kernel-chosen ports and addresses are never printed,
 * only relations. "sigpipe" ends the program by SIGPIPE (send on a reset connection without MSG_NOSIGNAL). */
#include "sock.h"

static struct sockaddr_in at(const char *ip, int port) {
    struct sockaddr_in a = { 0 };
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    return a;
}
#define SA(a) ((struct sockaddr *)&(a))

static int port_of(int fd, int peer) {
    struct sockaddr_in a;
    socklen_t l = sizeof a;
    if ((peer ? getpeername : getsockname)(fd, SA(a), &l) < 0)
        return -1;
    return ntohs(a.sin_port);
}

static const char *ip_of(int fd, int peer) {
    static char s[2][32];
    static int n;
    char *b = s[n++ & 1];
    struct sockaddr_in a;
    socklen_t l = sizeof a;
    if ((peer ? getpeername : getsockname)(fd, SA(a), &l) < 0)
        return sename(errno);
    inet_ntop(AF_INET, &a.sin_addr, b, 32);
    return b;
}

#include "inet6.h"
#include "inet6more.h"

static void tcp_part(void) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    printf("SO_TYPE %d SO_PROTOCOL %d SO_DOMAIN %d SO_ERROR %d\n", sopt(s, SOL_SOCKET, SO_TYPE), sopt(s, SOL_SOCKET, SO_PROTOCOL), sopt(s, SOL_SOCKET, SO_DOMAIN), sopt(s, SOL_SOCKET, SO_ERROR));
    printf("SO_SNDBUF %d SO_RCVBUF %d TCP_NODELAY %d TCP_MAXSEG %d IP_TTL %d SO_REUSEADDR %d SO_KEEPALIVE %d\n", sopt(s, SOL_SOCKET, SO_SNDBUF), sopt(s, SOL_SOCKET, SO_RCVBUF),
        sopt(s, IPPROTO_TCP, TCP_NODELAY), sopt(s, IPPROTO_TCP, TCP_MAXSEG), sopt(s, IPPROTO_IP, IP_TTL), sopt(s, SOL_SOCKET, SO_REUSEADDR), sopt(s, SOL_SOCKET, SO_KEEPALIVE));
    revents("fresh socket", s, POLLIN | POLLOUT | POLLRDHUP);
    printf("name of a fresh socket: %s port %d\n", ip_of(s, 0), port_of(s, 0));
    R("getpeername fresh", getpeername(s, SA((struct sockaddr_in){0}), &(socklen_t){ 16 }));
    char b[64] = {0};
    /* Another socket for these: Linux keeps the shutdown bits of an unconnected socket, and a listener's
     * connections inherit them. */
    int f = socket(AF_INET, SOCK_STREAM, 0);
    R("recv on a fresh socket", recv(f, b, sizeof b, MSG_DONTWAIT));
    R("send on a fresh socket", send(f, "x", 1, MSG_NOSIGNAL));
    R("shutdown a fresh socket", shutdown(f, SHUT_RDWR));
    R("accept on a fresh socket", accept(f, 0, 0));
    close(f);
    struct sockaddr_in a = at("127.0.0.1", 24620);
    /* SO_REUSEADDR on every listener: a run within a minute of the last finds its ports in TIME_WAIT. */
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    R("bind 127.0.0.1:24620", bind(s, SA(a), sizeof a));
    R("bind again", bind(s, SA(a), sizeof a));
    printf("name: %s port %d\n", ip_of(s, 0), port_of(s, 0));
    int t = socket(AF_INET, SOCK_STREAM, 0);
    R("bind the same port", bind(t, SA(a), sizeof a));
    struct sockaddr_in any = at("0.0.0.0", 24620);
    R("bind 0.0.0.0 on that port", bind(t, SA(any), sizeof any));
    struct sockaddr_in priv = at("127.0.0.1", 80);
    if (getuid())
        R("bind port 80 as a user", bind(t, SA(priv), sizeof priv));
    else
        puts("bind port 80 as a user = -EACCES");
    struct sockaddr_in far = at("8.8.8.8", 24621);
    R("bind 8.8.8.8", bind(t, SA(far), sizeof far));
    R("bind 15 bytes", bind(t, SA(a), 15));
    struct sockaddr_in v6 = a;
    v6.sin_family = AF_INET6;
    R("bind family AF_INET6", bind(t, SA(v6), sizeof v6));
    struct sockaddr_in un = at("0.0.0.0", 0);
    un.sin_family = AF_UNSPEC;
    R("bind AF_UNSPEC with INADDR_ANY", bind(t, SA(un), sizeof un));
    printf("  port chosen: %s\n", port_of(t, 0) > 0 ? "yes" : "no");
    close(t);
    R("listen(4)", listen(s, 4));
    printf("SO_ACCEPTCONN %d\n", sopt(s, SOL_SOCKET, SO_ACCEPTCONN));
    revents("listener, nothing waiting", s, POLLIN | POLLOUT);
    R("listen again", listen(s, 8));
    int c = socket(AF_INET, SOCK_STREAM, 0);
    R("connect", connect(c, SA(a), sizeof a));
    R("connect again", connect(c, SA(a), sizeof a));
    revents("listener, one waiting", s, POLLIN | POLLOUT);
    struct sockaddr_in pa;
    socklen_t pl = sizeof pa;
    int x = accept4(s, SA(pa), &pl, SOCK_NONBLOCK | SOCK_CLOEXEC);
    printf("accept4: %s len %u cloexec %s nonblock %s\n", x >= 0 ? "ok" : sename(errno), pl, (fcntl(x, F_GETFD) & FD_CLOEXEC) ? "yes" : "no", (fcntl(x, F_GETFL) & O_NONBLOCK) ? "yes" : "no");
    printf("  peer port is the client's: %s, client's name %s, peer %s\n", ntohs(pa.sin_port) == port_of(c, 0) ? "yes" : "no", ip_of(c, 0), ip_of(c, 1));
    printf("  client's peer port %d, accepted's own port %d\n", port_of(c, 1), port_of(x, 0));
    revents("connected client", c, POLLIN | POLLOUT | POLLRDHUP);
    R("send", send(c, "hello tcp", 9, 0));
    usleep(20000);
    int n = -1;
    R("ioctl(FIONREAD)", ioctl(x, FIONREAD, &n));
    printf("  FIONREAD %d\n", n);
    R("recv(5, MSG_PEEK)", recv(x, b, 5, MSG_PEEK));
    R("recv(64)", recv(x, b, 64, 0));
    printf("  got '%.9s'\n", b);
    R("recv nothing (accept4 SOCK_NONBLOCK)", recv(x, b, 64, 0));
    R("recvfrom gives no address", recvfrom(x, b, 64, MSG_DONTWAIT, SA(pa), &pl));
    R("shutdown(client, SHUT_WR)", shutdown(c, SHUT_WR));
    usleep(20000);
    R("recv after FIN", recv(x, b, 64, 0));
    revents("server after FIN", x, POLLIN | POLLOUT | POLLRDHUP);
    revents("client after own SHUT_WR", c, POLLIN | POLLOUT | POLLRDHUP);
    R("send after own SHUT_WR", send(c, "x", 1, MSG_NOSIGNAL));
    R("server answers", send(x, "answer", 6, 0));
    usleep(20000);
    R("client reads the answer", recv(c, b, 64, 0));
    R("server closes", close(x));
    usleep(20000);
    R("client reads EOF", recv(c, b, 64, 0));
    revents("client after both FINs", c, POLLIN | POLLOUT | POLLRDHUP);
    close(c);

    printf("-- close with unread data resets\n");
    c = socket(AF_INET, SOCK_STREAM, 0);
    R("connect", connect(c, SA(a), sizeof a));
    x = accept(s, 0, 0);
    R("send to the server", send(c, "unread", 6, 0));
    usleep(20000);
    R("server closes without reading", close(x));
    usleep(20000);
    revents("client after the reset", c, POLLIN | POLLOUT | POLLRDHUP);
    R("client recv", recv(c, b, 64, MSG_DONTWAIT));
    R("client recv again", recv(c, b, 64, MSG_DONTWAIT));
    R("client send", send(c, "x", 1, MSG_NOSIGNAL));
    close(c);

    printf("-- SO_LINGER 0 resets\n");
    c = socket(AF_INET, SOCK_STREAM, 0);
    connect(c, SA(a), sizeof a);
    x = accept(s, 0, 0);
    struct linger lg = { 1, 0 };
    R("SO_LINGER {1, 0}", setsockopt(x, SOL_SOCKET, SO_LINGER, &lg, sizeof lg));
    close(x);
    usleep(20000);
    R("client recv after the reset", recv(c, b, 64, MSG_DONTWAIT));
    close(c);
    close(s);

    printf("-- connect to a closed port\n");
    c = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in closed = at("127.0.0.1", 24621);
    R("blocking connect", connect(c, SA(closed), sizeof closed));
    R("SO_ERROR after it", sopt(c, SOL_SOCKET, SO_ERROR));
    revents("after the refusal", c, POLLIN | POLLOUT | POLLRDHUP);
    close(c);
    c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    R("non-blocking connect", connect(c, SA(closed), sizeof closed));
    usleep(20000);
    revents("after the refusal", c, POLLIN | POLLOUT | POLLRDHUP);
    printf("SO_ERROR %d then %d\n", sopt(c, SOL_SOCKET, SO_ERROR), sopt(c, SOL_SOCKET, SO_ERROR));
    R("connect again", connect(c, SA(closed), sizeof closed));
    close(c);

    printf("-- non-blocking connect that works\n");
    s = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in l2 = at("0.0.0.0", 24622);
    bind(s, SA(l2), sizeof l2);
    listen(s, 2);
    c = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    struct sockaddr_in zero = at("0.0.0.0", 24622);
    R("connect 0.0.0.0 (non-blocking)", connect(c, SA(zero), sizeof zero));
    usleep(20000);
    revents("after the handshake", c, POLLIN | POLLOUT);
    R("connect again", connect(c, SA(zero), sizeof zero));
    R("and again", connect(c, SA(zero), sizeof zero));
    printf("  peer %s\n", ip_of(c, 1));
    x = accept(s, 0, 0);
    printf("accepted: %s, local address of the accepted %s\n", x >= 0 ? "yes" : "no", ip_of(x, 0));
    close(x);
    close(c);
    fcntl(s, F_SETFL, O_NONBLOCK);
    R("accept nothing waiting", accept(s, 0, 0));
    close(s);

    printf("-- SO_REUSEADDR\n");
    int r1 = socket(AF_INET, SOCK_STREAM, 0), r2 = socket(AF_INET, SOCK_STREAM, 0);
    setsockopt(r1, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    printf("SO_REUSEADDR %d\n", sopt(r1, SOL_SOCKET, SO_REUSEADDR));
    struct sockaddr_in rp = at("127.0.0.1", 24623);
    R("bind first (reuse)", bind(r1, SA(rp), sizeof rp));
    R("bind second (no reuse)", bind(r2, SA(rp), sizeof rp));
    setsockopt(r2, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    R("bind second (reuse)", bind(r2, SA(rp), sizeof rp));
    R("listen on the first while the second holds the port", listen(r1, 1));
    close(r2);
    R("listen after the second closed", listen(r1, 1));
    close(r1);
    int l3 = socket(AF_INET, SOCK_STREAM, 0);
    R("listen without bind", listen(l3, 1));
    printf("  port chosen: %s, address %s\n", port_of(l3, 0) > 0 ? "yes" : "no", ip_of(l3, 0));
    close(l3);
    printf("-- options\n");
    int v = 1;
    R("TCP_NODELAY 1", setsockopt(c = socket(AF_INET, SOCK_STREAM, 0), IPPROTO_TCP, TCP_NODELAY, &v, sizeof v));
    printf("  TCP_NODELAY %d\n", sopt(c, IPPROTO_TCP, TCP_NODELAY));
    R("TCP_NODELAY short", setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &v, 2));
    R("unknown TCP option", setsockopt(c, IPPROTO_TCP, 9999, &v, sizeof v));
    v = 10;
    R("TCP_KEEPIDLE 10", setsockopt(c, IPPROTO_TCP, TCP_KEEPIDLE, &v, sizeof v));
    printf("  TCP_KEEPIDLE %d TCP_KEEPINTVL %d TCP_KEEPCNT %d\n", sopt(c, IPPROTO_TCP, TCP_KEEPIDLE), sopt(c, IPPROTO_TCP, TCP_KEEPINTVL), sopt(c, IPPROTO_TCP, TCP_KEEPCNT));
    v = 0;
    R("TCP_KEEPIDLE 0", setsockopt(c, IPPROTO_TCP, TCP_KEEPIDLE, &v, sizeof v));
    v = 1;
    R("SO_KEEPALIVE", setsockopt(c, SOL_SOCKET, SO_KEEPALIVE, &v, sizeof v));
    printf("  SO_KEEPALIVE %d\n", sopt(c, SOL_SOCKET, SO_KEEPALIVE));
    v = 0x10;
    R("IP_TOS", setsockopt(c, IPPROTO_IP, IP_TOS, &v, sizeof v));
    printf("  IP_TOS %d\n", sopt(c, IPPROTO_IP, IP_TOS));
    R("SOL_UDP on TCP", setsockopt(c, IPPROTO_UDP, 1, &v, sizeof v));
    close(c);
}

static void udp_part(void) {
    int a = socket(AF_INET, SOCK_DGRAM, 0), b = socket(AF_INET, SOCK_DGRAM, 0);
    printf("SO_TYPE %d SO_PROTOCOL %d SO_SNDBUF %d SO_RCVBUF %d\n", sopt(a, SOL_SOCKET, SO_TYPE), sopt(a, SOL_SOCKET, SO_PROTOCOL), sopt(a, SOL_SOCKET, SO_SNDBUF), sopt(a, SOL_SOCKET, SO_RCVBUF));
    revents("fresh UDP socket", a, POLLIN | POLLOUT | POLLRDHUP);
    struct sockaddr_in la = at("127.0.0.1", 24630);
    R("bind 127.0.0.1:24630", bind(a, SA(la), sizeof la));
    R("bind the same port", bind(b, SA(la), sizeof la));
    R("send unconnected", send(b, "x", 1, 0));
    R("sendto", sendto(b, "hello", 5, 0, SA(la), sizeof la));
    printf("  sendto bound an ephemeral port: %s\n", port_of(b, 0) > 0 ? "yes" : "no");
    revents("receiver", a, POLLIN | POLLOUT);
    int n = -1;
    R("ioctl(FIONREAD)", ioctl(a, FIONREAD, &n));
    printf("  FIONREAD %d\n", n);
    char buf[16];
    struct sockaddr_in from;
    socklen_t fl = sizeof from;
    R("recvfrom(3)", recvfrom(a, buf, 3, 0, SA(from), &fl));
    printf("  from len %u, port is the sender's %s, ip %s\n", fl, ntohs(from.sin_port) == port_of(b, 0) ? "yes" : "no", inet_ntoa(from.sin_addr));
    R("recv, the rest of the datagram is gone", recv(a, buf, 16, MSG_DONTWAIT));
    sendto(b, "twelve bytes", 12, 0, SA(la), sizeof la);
    struct iovec iov = { buf, 4 };
    struct msghdr m = { 0 };
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    R("recvmsg(4)", recvmsg(a, &m, 0));
    printf("  msg_flags %#x\n", m.msg_flags);
    sendto(b, "abc", 3, 0, SA(la), sizeof la);
    R("recv(1, MSG_TRUNC)", recv(a, buf, 1, MSG_TRUNC));
    sendto(b, "peek", 4, 0, SA(la), sizeof la);
    R("recv MSG_PEEK", recv(a, buf, 16, MSG_PEEK));
    R("recv", recv(a, buf, 16, 0));
    struct sockaddr_in zp = at("127.0.0.1", 0);
    R("sendto port 0", sendto(b, "x", 1, 0, SA(zp), sizeof zp));
    static char big[70000];
    R("sendto 65508 bytes", sendto(b, big, 65508, 0, SA(la), sizeof la));
    R("sendto 65507 bytes", sendto(b, big, 65507, 0, SA(la), sizeof la));
    R("recv the big one", recv(a, big, sizeof big, 0));
    struct sockaddr_in bc = at("255.255.255.255", 24631);
    R("sendto broadcast without SO_BROADCAST", sendto(b, "x", 1, 0, SA(bc), sizeof bc));
    R("sendto 15-byte address", sendto(b, "x", 1, 0, SA(la), 15));
    printf("-- connected\n");
    int c = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in cl = at("127.0.0.1", 24632);
    bind(c, SA(cl), sizeof cl);
    R("connect a -> c", connect(a, SA(cl), sizeof cl));
    printf("  a's peer %s port %d\n", ip_of(a, 1), port_of(a, 1));
    R("b sends to a (not a's peer)", sendto(b, "from b", 6, 0, SA(la), sizeof la));
    R("c sends to a", sendto(c, "from c", 6, 0, SA(la), sizeof la));
    memset(buf, 0, sizeof buf);
    R("a receives", recv(a, buf, 16, MSG_DONTWAIT));
    printf("  got '%s'\n", buf);
    R("a receives again", recv(a, buf, 16, MSG_DONTWAIT));
    struct sockaddr un = { AF_UNSPEC };
    R("connect AF_UNSPEC", connect(a, &un, sizeof un));
    R("getpeername after it", getpeername(a, SA(from), &fl));
    printf("-- port unreachable\n");
    int d = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in none = at("127.0.0.1", 24633);
    R("connect to a port nobody has", connect(d, SA(none), sizeof none));
    R("send", send(d, "x", 1, 0));
    usleep(20000);
    revents("after the ICMP error", d, POLLIN | POLLOUT);
    R("send again", send(d, "x", 1, 0));
    R("send a third time", send(d, "x", 1, 0));
    usleep(20000);
    R("recv", recv(d, buf, 16, MSG_DONTWAIT));
    printf("SO_ERROR %d\n", sopt(d, SOL_SOCKET, SO_ERROR));
    R("shutdown unconnected", shutdown(b, SHUT_RDWR));
    R("shutdown connected", shutdown(d, SHUT_RD));
    R("recv after SHUT_RD (non-blocking)", recv(d, buf, 16, MSG_DONTWAIT));
    revents("after SHUT_RD", d, POLLIN | POLLOUT | POLLRDHUP);
    R("listen on UDP", listen(a, 1));
    R("accept on UDP", accept(a, 0, 0));
    close(a);
    close(b);
    close(c);
    close(d);
    int again = socket(AF_INET, SOCK_DGRAM, 0);
    R("bind 24630 after close", bind(again, SA(la), sizeof la));
    close(again);
}

static void errors_part(void) {
    R("socket(AF_INET, STREAM, UDP)", socket(AF_INET, SOCK_STREAM, IPPROTO_UDP));
    R("socket(AF_INET, DGRAM, TCP)", socket(AF_INET, SOCK_DGRAM, IPPROTO_TCP));
    R("socket(AF_INET, STREAM, 300)", socket(AF_INET, SOCK_STREAM, 300));
    R("socket(AF_INET, STREAM, -1)", socket(AF_INET, SOCK_STREAM, -1));
    R("socket(AF_INET, SEQPACKET)", socket(AF_INET, SOCK_SEQPACKET, 0));
    int r = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
    printf("socket(AF_INET, RAW, ICMP): %s\n", r >= 0 ? "ok" : (getuid() == 0 ? "refused as root" : sename(errno)));
    if (r >= 0)
        close(r);
    int sv[2];
    R("socketpair(AF_INET)", socketpair(AF_INET, SOCK_STREAM, 0, sv));
    int s = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in a = at("127.0.0.1", 24634);
    R("connect 15 bytes", connect(s, SA(a), 15));
    R("connect 1 byte", connect(s, SA(a), 1));
    struct sockaddr_in v6 = a;
    v6.sin_family = AF_INET6;
    R("connect family AF_INET6", connect(s, SA(v6), sizeof v6));
    R("connect from a kernel address", connect(s, KERNEL_ADDR, 16));
    R("connect 129-byte address", connect(s, SA(a), 129));
    R("send MSG_OOB", send(s, "x", 1, MSG_OOB | MSG_NOSIGNAL));
    close(s);
}

/* Bad pointers and lengths on AF_INET sockets. Ports 24636-24637. */
static void hostile_part(void) {
    char b[64];
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = at("127.0.0.1", 24636);
    R("bind udp 127.0.0.1:24636", bind(u, SA(a), sizeof a));
    R("sendto from a kernel address", sendto(u, KERNEL_ADDR, 4, 0, SA(a), sizeof a));
    R("sendto address at a kernel address", sendto(u, "abcd", 4, 0, KERNEL_ADDR, sizeof a));
    R("sendto address of 15 bytes", sendto(u, "abcd", 4, 0, SA(a), 15));
    R("sendto address of 129 bytes", sendto(u, "abcd", 4, 0, SA(a), 129));
    R("sendto itself", sendto(u, "abcd", 4, 0, SA(a), sizeof a));
    R("recv into a kernel address", recv(u, KERNEL_ADDR, sizeof b, MSG_DONTWAIT));
    R("recv after the failed check", recv(u, b, sizeof b, MSG_DONTWAIT));
    R("sendto itself", sendto(u, "abcd", 4, 0, SA(a), sizeof a));
    R("recv into address 8 (the copy faults)", recv(u, (void *)8, sizeof b, MSG_DONTWAIT));
    R("recv after the failed copy", recv(u, b, sizeof b, MSG_DONTWAIT));
    R("sendto itself", sendto(u, "efgh", 4, 0, SA(a), sizeof a));
    socklen_t l = sizeof a;
    R("recvfrom with the address to a kernel address", recvfrom(u, b, sizeof b, MSG_DONTWAIT, KERNEL_ADDR, &l));
    R("recv after it", recv(u, b, sizeof b, MSG_DONTWAIT));
    R("sendto itself", sendto(u, "ijkl", 4, 0, SA(a), sizeof a));
    R("recvfrom with the length at a kernel address", recvfrom(u, b, sizeof b, MSG_DONTWAIT, SA(a), KERNEL_ADDR));
    R("recv after it", recv(u, b, sizeof b, MSG_DONTWAIT));
    R("setsockopt IP_TTL from a kernel address", setsockopt(u, IPPROTO_IP, IP_TTL, KERNEL_ADDR, 4));
    int v;
    l = sizeof v;
    R("getsockopt IP_TTL to a kernel address", getsockopt(u, IPPROTO_IP, IP_TTL, KERNEL_ADDR, &l));
    close(u);

    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    a = at("127.0.0.1", 24637);
    R("bind tcp 127.0.0.1:24637", bind(s, SA(a), sizeof a));
    R("listen", listen(s, 4));
    int c = socket(AF_INET, SOCK_STREAM, 0);
    R("connect", connect(c, SA(a), sizeof a));
    int t = accept(s, NULL, NULL);
    printf("accepted: %s\n", t >= 0 ? "yes" : sename(errno));
    R("send from a kernel address", send(c, KERNEL_ADDR, 4, MSG_NOSIGNAL));
    R("send abcd", send(c, "abcd", 4, 0));
    struct pollfd p = { t, POLLIN, 0 };
    poll(&p, 1, 2000);
    R("recv into a kernel address", recv(t, KERNEL_ADDR, sizeof b, MSG_DONTWAIT));
    R("recv into address 8 (the copy faults)", recv(t, (void *)8, sizeof b, MSG_DONTWAIT));
    R("recv after the failed copy", recv(t, b, sizeof b, MSG_DONTWAIT));
    R("setsockopt TCP_NODELAY from a kernel address", setsockopt(c, IPPROTO_TCP, TCP_NODELAY, KERNEL_ADDR, 4));
    l = sizeof v;
    R("getsockopt TCP_NODELAY length at a kernel address", getsockopt(c, IPPROTO_TCP, TCP_NODELAY, &v, KERNEL_ADDR));
    l = sizeof a;
    R("getpeername to a kernel address", getpeername(c, KERNEL_ADDR, &l));
    close(c);
    close(t);
    close(s);
}

/* ioctls on AF_INET sockets. */
static void ioctl_part(void) {
    int n = -7;
    struct timeval tv;
    int t = socket(AF_INET, SOCK_STREAM, 0);
    int u = socket(AF_INET, SOCK_DGRAM, 0);
    R("tcp ioctl(SIOCATMARK)", ioctl(t, SIOCATMARK, &n));
    printf("  SIOCATMARK %d\n", n);
    R("udp ioctl(SIOCATMARK)", ioctl(u, SIOCATMARK, &n));
    n = -7;
    R("tcp ioctl(SIOCGPGRP)", ioctl(t, SIOCGPGRP, &n));
    printf("  SIOCGPGRP %d\n", n);
    R("tcp ioctl(SIOCGSTAMP) before any packet", ioctl(t, SIOCGSTAMP, &tv));
    R("udp ioctl(SIOCGSTAMP) before any packet", ioctl(u, SIOCGSTAMP, &tv));
    R("udp ioctl(SIOCINQ) empty", ioctl(u, FIONREAD, &n));
    printf("  SIOCINQ %d\n", n);
    R("tcp ioctl(SIOCINQ) unconnected", ioctl(t, FIONREAD, &n));
    R("listen", listen(t, 1));
    R("tcp ioctl(SIOCINQ) listening", ioctl(t, FIONREAD, &n));
    R("tcp ioctl(SIOCOUTQ) listening", ioctl(t, TIOCOUTQ, &n));
    R("tcp ioctl(SIOCATMARK) listening", ioctl(t, SIOCATMARK, &n));
    printf("  SIOCATMARK %d\n", n);
    printf("SO_INCOMING_CPU tcp %d udp %d\n", sopt(t, SOL_SOCKET, SO_INCOMING_CPU), sopt(u, SOL_SOCKET, SO_INCOMING_CPU));
    socklen_t l = 64;
    gid_t g[16];
    R("get SO_PEERGROUPS on udp", getsockopt(u, SOL_SOCKET, SO_PEERGROUPS, g, &l));
    close(t);
    close(u);
}

int main(int argc, char **argv) {
    const char *part = argc > 1 ? argv[1] : "all";
    setvbuf(stdout, NULL, _IOLBF, 0);
    printf("part: %s\n", part);
    if (!strcmp(part, "tcp") || !strcmp(part, "all"))
        tcp_part();
    if (!strcmp(part, "udp") || !strcmp(part, "all"))
        udp_part();
    if (!strcmp(part, "errors") || !strcmp(part, "all"))
        errors_part();
    if (!strcmp(part, "hostile") || !strcmp(part, "all"))
        hostile_part();
    if (!strcmp(part, "ioctl") || !strcmp(part, "all"))
        ioctl_part();
    if (!strcmp(part, "tcp6") || !strcmp(part, "all"))
        tcp6_part();
    if (!strcmp(part, "udp6") || !strcmp(part, "all"))
        udp6_part();
    if (!strcmp(part, "v6errors") || !strcmp(part, "all"))
        v6errors_part();
    if (!strcmp(part, "v6opts") || !strcmp(part, "all"))
        v6opts_part();
    if (!strcmp(part, "v6more") || !strcmp(part, "all"))
        v6more_part();
    if (!strcmp(part, "sigpipe")) {
        int s = socket(AF_INET, SOCK_STREAM, 0);
        puts("send on an unconnected TCP socket");
        fflush(stdout);
        send(s, "x", 1, 0);
        puts("not reached");
    }
    printf("end of part %s\n", part);
    return 0;
}
