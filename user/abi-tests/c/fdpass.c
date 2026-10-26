/* NW-5: the Wayland wl_shm pattern over AF_UNIX. A client makes a shared memory file (a /dev/shm file, or a
 * memfd with "memfd"), maps it, passes the fd over a stream socket with SCM_RIGHTS, and the compositor side maps
 * what arrived and sees the client's pixels, also those written after the hand-over.
 *   abi-fdpass [shm|memfd]       both sides in one program (socketpair)
 *   abi-fdpass server PATH       the compositor: listens on PATH, takes one client, checks its buffer
 *   abi-fdpass client PATH       the client: connects to PATH (waits up to 5 s for it), sends a buffer
 * Output names relations only (no pids, fds or addresses). */
#include "sock.h"
#include <sys/mman.h>
#include <sys/stat.h>

#define POOL (4 * 4096)

static int make_pool(const char *how) {
    if (!strcmp(how, "memfd"))
        return (int)syscall(SYS_memfd_create, "abi-fdpass", 1 /* MFD_CLOEXEC */);
    char p[64];
    snprintf(p, sizeof p, "/dev/shm/abi-fdpass-%d", (int)getpid());
    int fd = open(p, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    unlink(p);
    return fd;
}

static void fill(uint32_t *px, uint32_t seed) {
    for (int i = 0; i < POOL / 4; i++)
        px[i] = seed ^ (uint32_t)i * 2654435761u;
}

static int check(const uint32_t *px, uint32_t seed) {
    for (int i = 0; i < POOL / 4; i++)
        if (px[i] != (seed ^ (uint32_t)i * 2654435761u))
            return 0;
    return 1;
}

static int send_fd(int s, int fd, const char *msg) {
    char ctl[CMSG_SPACE(sizeof(int))];
    memset(ctl, 0, sizeof ctl);
    struct iovec iov = { (void *)msg, strlen(msg) };
    struct msghdr m = { 0 };
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = ctl;
    m.msg_controllen = sizeof ctl;
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof fd);
    return sendmsg(s, &m, MSG_NOSIGNAL);
}

static int recv_fd(int s, char *msg, size_t n) {
    char ctl[CMSG_SPACE(sizeof(int))];
    struct iovec iov = { msg, n - 1 };
    struct msghdr m = { 0 };
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = ctl;
    m.msg_controllen = sizeof ctl;
    long r = recvmsg(s, &m, MSG_CMSG_CLOEXEC);
    if (r < 0)
        return -1;
    msg[r] = 0;
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    if (!c || c->cmsg_type != SCM_RIGHTS)
        return -2;
    int fd;
    memcpy(&fd, CMSG_DATA(c), sizeof fd);
    return fd;
}

/* The compositor's side of one buffer: map what arrived, check it, check a later write too. */
static int compositor(int s) {
    char msg[64];
    int fd = recv_fd(s, msg, sizeof msg);
    printf("compositor: request '%s', fd arrived: %s\n", msg, fd >= 0 ? "yes" : "no");
    if (fd < 0)
        return 1;
    struct stat st;
    fstat(fd, &st);
    printf("compositor: pool size %ld, cloexec %s\n", (long)st.st_size, (fcntl(fd, F_GETFD) & FD_CLOEXEC) ? "yes" : "no");
    uint32_t *px = mmap(0, POOL, PROT_READ, MAP_SHARED, fd, 0);
    if (px == MAP_FAILED) {
        printf("compositor: mmap failed: %s\n", sename(errno));
        return 1;
    }
    printf("compositor: first frame right: %s\n", check(px, 0x1111) ? "yes" : "no");
    send(s, "frame1", 6, MSG_NOSIGNAL);
    char b[16];
    long r = recv(s, b, sizeof b, 0);
    printf("compositor: client says '%.*s'\n", r > 0 ? (int)r : 0, b);
    printf("compositor: second frame (written after the hand-over) right: %s\n", check(px, 0x2222) ? "yes" : "no");
    send(s, "frame2", 6, MSG_NOSIGNAL);
    munmap(px, POOL);
    close(fd);
    return 0;
}

static int client(int s, const char *how) {
    int pool = make_pool(how);
    if (pool < 0) {
        printf("client: pool: %s\n", sename(errno));
        return 1;
    }
    R("client: ftruncate", ftruncate(pool, POOL));
    uint32_t *px = mmap(0, POOL, PROT_READ | PROT_WRITE, MAP_SHARED, pool, 0);
    if (px == MAP_FAILED) {
        printf("client: mmap failed: %s\n", sename(errno));
        return 1;
    }
    fill(px, 0x1111);
    R("client: sendmsg create_pool", send_fd(s, pool, "create_pool"));
    close(pool);
    char b[16];
    long r = recv(s, b, sizeof b, 0);
    printf("client: compositor says '%.*s'\n", r > 0 ? (int)r : 0, b);
    fill(px, 0x2222);
    send(s, "attach", 6, MSG_NOSIGNAL);
    r = recv(s, b, sizeof b, 0);
    printf("client: compositor says '%.*s'\n", r > 0 ? (int)r : 0, b);
    munmap(px, POOL);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    const char *mode = argc > 1 ? argv[1] : "shm";
    if (!strcmp(mode, "server") && argc > 2) {
        int l = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        struct sockaddr_un a = { AF_UNIX };
        strncpy(a.sun_path, argv[2], sizeof a.sun_path - 1);
        unlink(argv[2]);
        if (R("server: bind", bind(l, (struct sockaddr *)&a, sizeof a)) < 0 || R("server: listen", listen(l, 4)) < 0)
            return 1;
        puts("server: ready");
        int s = accept4(l, 0, 0, SOCK_CLOEXEC);
        struct ucred cr;
        socklen_t crl = sizeof cr;
        getsockopt(s, SOL_SOCKET, SO_PEERCRED, &cr, &crl);
        printf("server: client accepted: %s, its pid differs from mine: %s, same uid: %s\n", s >= 0 ? "yes" : "no", cr.pid != getpid() ? "yes" : "no", cr.uid == getuid() ? "yes" : "no");
        int r = compositor(s);
        close(s);
        close(l);
        unlink(argv[2]);
        return r;
    }
    if (!strcmp(mode, "client") && argc > 2) {
        int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        struct sockaddr_un a = { AF_UNIX };
        strncpy(a.sun_path, argv[2], sizeof a.sun_path - 1);
        int r = -1;
        for (int i = 0; i < 500 && r < 0; i++) {
            r = connect(s, (struct sockaddr *)&a, sizeof a);
            if (r < 0)
                usleep(10000);
        }
        R("client: connect", r);
        if (r < 0)
            return 1;
        r = client(s, argc > 3 ? argv[3] : "shm");
        close(s);
        return r;
    }
    if (strcmp(mode, "shm") && strcmp(mode, "memfd")) {
        puts("usage: abi-fdpass [shm|memfd] | server PATH | client PATH [shm|memfd]");
        return 0;
    }
    /* One program, both sides: the client writes from one end, the compositor reads at the other. */
    int sv[2];
    R("socketpair", socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv));
    int pool = make_pool(mode);
    if (pool < 0) {
        printf("pool: %s\n", sename(errno));
        return 1;
    }
    R("ftruncate", ftruncate(pool, POOL));
    uint32_t *px = mmap(0, POOL, PROT_READ | PROT_WRITE, MAP_SHARED, pool, 0);
    fill(px, 0x1111);
    R("sendmsg create_pool", send_fd(sv[0], pool, "create_pool"));
    close(pool);
    char msg[64];
    int fd = recv_fd(sv[1], msg, sizeof msg);
    printf("fd arrived: %s, request '%s'\n", fd >= 0 ? "yes" : "no", msg);
    uint32_t *seen = mmap(0, POOL, PROT_READ, MAP_SHARED, fd, 0);
    printf("compositor mapping: %s\n", seen != MAP_FAILED ? "ok" : sename(errno));
    printf("first frame right: %s\n", seen != MAP_FAILED && check(seen, 0x1111) ? "yes" : "no");
    fill(px, 0x2222);
    printf("second frame right: %s\n", seen != MAP_FAILED && check(seen, 0x2222) ? "yes" : "no");
    munmap(px, POOL);
    printf("still mapped after the client unmapped: %s\n", seen != MAP_FAILED && check(seen, 0x2222) ? "yes" : "no");
    munmap(seen, POOL);
    close(fd);
    close(sv[0]);
    close(sv[1]);
    puts("done");
    return 0;
}
