#define _GNU_SOURCE
#include "ns.h"
#include <sched.h>
#include <sys/socket.h>

static long send_cred(int fd, pid_t pid, uid_t uid, gid_t gid) {
    struct ucred u = { pid, uid, gid };
    char buf[CMSG_SPACE(sizeof u)] = {0};
    char b = 'x';
    struct iovec io = { &b, 1 };
    struct msghdr m = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = buf, .msg_controllen = sizeof buf };
    struct cmsghdr *c = CMSG_FIRSTHDR(&m);
    c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_CREDENTIALS; c->cmsg_len = CMSG_LEN(sizeof u);
    memcpy(CMSG_DATA(c), &u, sizeof u);
    return sendmsg(fd, &m, 0);
}

int main(void) {
    setvbuf(stdout, 0, _IONBF, 0);
    uid_t ou = getuid(); gid_t og = getgid(); pid_t op = getpid();
    int sp[2], one = 1;
    R("socketpair", socketpair(AF_UNIX, SOCK_DGRAM, 0, sp));
    R("passcred", setsockopt(sp[0], SOL_SOCKET, SO_PASSCRED, &one, 4));
    pid_t c = fork();
    if (c == 0) {
        if (R("unshare", unshare(CLONE_NEWUSER | CLONE_NEWNET)) < 0) _exit(1);
        char m[64];
        snprintf(m, sizeof m, "0 %u 1", ou); wr("uid_map", "/proc/self/uid_map", m);
        wr("setgroups", "/proc/self/setgroups", "deny");
        snprintf(m, sizeof m, "0 %u 1", og); wr("gid_map", "/proc/self/gid_map", m);
        struct ucred p; socklen_t l = sizeof p;
        R("peercred", getsockopt(sp[1], SOL_SOCKET, SO_PEERCRED, &p, &l));
        printf("peercred inside: pid parent %s uid %u gid %u\n", yn(p.pid == op), p.uid, p.gid);
        R("send own", send_cred(sp[1], getpid(), 0, 0));
        R("send uid 5", send_cred(sp[1], getpid(), 5, 0));
        R("send other pid", send_cred(sp[1], 1, 0, 0));
        _exit(0);
    }
    reap("child", c);
    char b; char buf[CMSG_SPACE(sizeof(struct ucred))];
    struct iovec io = { &b, 1 };
    struct msghdr m = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = buf, .msg_controllen = sizeof buf };
    if (R("recv", recvmsg(sp[0], &m, MSG_DONTWAIT)) == 1) {
        struct cmsghdr *h = CMSG_FIRSTHDR(&m);
        struct ucred u; memcpy(&u, CMSG_DATA(h), sizeof u);
        printf("outside: pid child %s uid real %s gid real %s\n", yn(u.pid == c), yn(u.uid == ou), yn(u.gid == og));
    }
    m.msg_controllen = sizeof buf;
    R("recv more", recvmsg(sp[0], &m, MSG_DONTWAIT));
    return 0;
}
