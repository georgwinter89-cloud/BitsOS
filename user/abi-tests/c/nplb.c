/* YT-4a (taken over by INT-W6 from plan/notes/yt-YT-4a-abi-nplb.c): binary abi-nplb.
 * The three calls behind nplb failures on V2: readlink("") (fs/stat.c:490-494: ENOENT), socket(AF_INET6) with the
 * options nplb sets (net/ipv6/af_inet6.c:119), and /etc/localtime as a link into a zoneinfo directory (Starboard's
 * time_zone_get_name.cc reads only the link target). Host-independent output: no zone name, no addresses. */
#include "abi.h"
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

static void opt(const char *label, int fd, int level, int name) {
    int one = 1;
    show(label, setsockopt(fd, level, name, &one, sizeof one));
}

int main(void) {
    char buf[256];
    SYS("readlink.empty", SYS_readlink, "", buf, sizeof buf);
    SYS("readlinkat.empty_cwd", SYS_readlinkat, AT_FDCWD, "", buf, sizeof buf);
    SYS("readlink.not_link", SYS_readlink, "/", buf, sizeof buf);

    long n = readlink("/etc/localtime", buf, sizeof buf - 1);
    yes("localtime.is_link", n > 0);
    if (n > 0) {
        buf[n] = 0;
        yes("localtime.into_zoneinfo", strstr(buf, "zoneinfo/") != NULL);
    }

    for (int type = SOCK_STREAM; type <= SOCK_DGRAM; type++) {
        int fd = socket(AF_INET6, type | SOCK_CLOEXEC, 0);
        show(type == SOCK_STREAM ? "inet6.stream" : "inet6.dgram", fd < 0 ? -1 : 0);
        if (fd < 0)
            continue;
        opt("  SO_REUSEADDR", fd, SOL_SOCKET, SO_REUSEADDR);
        opt("  SO_KEEPALIVE", fd, SOL_SOCKET, SO_KEEPALIVE);
        opt("  SO_BROADCAST", fd, SOL_SOCKET, SO_BROADCAST);
        if (type == SOCK_STREAM) {
            opt("  TCP_NODELAY", fd, IPPROTO_TCP, TCP_NODELAY);
            opt("  TCP_KEEPIDLE", fd, IPPROTO_TCP, TCP_KEEPIDLE);
            opt("  TCP_KEEPINTVL", fd, IPPROTO_TCP, TCP_KEEPINTVL);
        }
        close(fd);
    }
    return 0;
}
