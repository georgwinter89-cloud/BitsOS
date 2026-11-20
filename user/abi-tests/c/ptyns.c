/* IN-PTY: ptys of devpts instances (linux-7.3.1/fs/devpts/inode.c:181 devpts_acquire, :498 devpts_pty_new;
 * drivers/tty/pty.c:767 ptmx_open, :617 ptm_open_peer). As uid 1000 in a new user and mount namespace: a devpts
 * of its own on /dev/pts, then posix_openpt (devtmpfs's /dev/ptmx finds the pts next to it), grantpt, unlockpt,
 * ptsname, the slave's look, both directions, TIOCGPTPEER; a second instance with its own numbers, the wrong
 * instance refused, the node gone with the master, and a lazy unmount with a pty open. No pids, inode numbers or
 * host-specific values. */
#include "ns.h"
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/sysmacros.h>
#include <termios.h>

#ifndef TIOCGPTPEER
#define TIOCGPTPEER _IO('T', 0x41)
#endif

static const char *er(int e) {
    switch (e) {
    case EIO: return "EIO";
    case ENOSPC: return "ENOSPC";
    case EBUSY: return "EBUSY";
    }
    return ename(e);
}

static long M(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, er(errno));
    else
        printf("%s = %ld\n", label, r);
    return r;
}

static void look(const char *label, const char *path) {
    struct stat st;
    if (stat(path, &st)) {
        printf("%s = -%s\n", label, er(errno));
        return;
    }
    printf("%s = chr %s, mode %04o, major %u, uid mine %s, gid mine %s\n", label, yn(S_ISCHR(st.st_mode)),
           (unsigned)(st.st_mode & 07777), major(st.st_rdev), yn(st.st_uid == getuid()), yn(st.st_gid == getgid()));
}

/* What one end reads now (non-blocking, a short wait first). */
static void got(const char *label, int fd) {
    char b[64] = {0};
    usleep(20000);
    int fl = fcntl(fd, F_GETFL);
    fcntl(fd, F_SETFL, fl | O_NONBLOCK);
    long r = read(fd, b, sizeof b - 1);
    fcntl(fd, F_SETFL, fl);
    if (r < 0) {
        printf("%s = -%s\n", label, er(errno));
        return;
    }
    for (long i = 0; i < r; i++)
        if (b[i] == '\r' || b[i] == '\n')
            b[i] = b[i] == '\r' ? 'R' : 'N';
    printf("%s = %ld \"%s\"\n", label, r, b);
}

static int openpt(const char *label, const char *ptmx, unsigned *n) {
    int m = ptmx ? open(ptmx, O_RDWR | O_NOCTTY) : posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0) {
        printf("%s = -%s\n", label, er(errno));
        return -1;
    }
    int zero = 0;
    *n = 9999;
    printf("%s = ok, grantpt %d, unlockpt %d, TIOCGPTN %d, number %u\n", label, ptmx ? 0 : grantpt(m),
           ioctl(m, TIOCSPTLCK, &zero), ioctl(m, TIOCGPTN, n), *n);
    return m;
}

int main(void) {
    unsigned uid = getuid(), gid = getgid();
    if (unshare(CLONE_NEWUSER | CLONE_NEWNS)) {
        printf("unshare = -%s\n", er(errno));
        return 0;
    }
    char map[64];
    snprintf(map, sizeof map, "%u %u 1", uid, uid);
    wr("uid_map", "/proc/self/uid_map", map);
    wr("setgroups", "/proc/self/setgroups", "deny");
    snprintf(map, sizeof map, "%u %u 1", gid, gid);
    wr("gid_map", "/proc/self/gid_map", map);
    M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
    M("devpts on /dev/pts", mount("ptyns", "/dev/pts", "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=620"));
    look("/dev/pts/ptmx", "/dev/pts/ptmx");

    /* posix_openpt opens devtmpfs's /dev/ptmx: its pty is made in the devpts on /dev/pts next to it. */
    unsigned n0, n1;
    int m0 = openpt("posix_openpt", NULL, &n0);
    if (m0 < 0)
        return 0;
    char *name = ptsname(m0);
    printf("ptsname = %s\n", name ? name : er(errno));
    look("slave", "/dev/pts/0");
    int s0 = open("/dev/pts/0", O_RDWR | O_NOCTTY);
    printf("open slave = %s\n", s0 >= 0 ? "ok" : er(errno));
    if (s0 >= 0) {
        M("master writes", write(m0, "hi\r", 3));
        got("slave reads", s0);
        got("master reads echo", m0);
        M("slave writes", write(s0, "yo\n", 3));
        got("master reads", m0);
    }
    int peer = ioctl(m0, TIOCGPTPEER, O_RDWR | O_NOCTTY);
    if (peer < 0) {
        printf("TIOCGPTPEER = -%s\n", er(errno));
    } else {
        struct stat st;
        fstat(peer, &st);
        char link[64] = {0};
        char fdp[32];
        snprintf(fdp, sizeof fdp, "/proc/self/fd/%d", peer);
        long l = readlink(fdp, link, sizeof link - 1);
        printf("TIOCGPTPEER = ok, rdev %u:%u, fd link %s\n", major(st.st_rdev), minor(st.st_rdev), l > 0 ? link : er(errno));
        close(peer);
    }
    int m1 = openpt("posix_openpt again", NULL, &n1);

    /* A second instance: its own numbers, its own nodes. */
    M("tmpfs /tmp", mount("ptyns", "/tmp", "tmpfs", 0, NULL));
    M("mkdir /tmp/b", mkdir("/tmp/b", 0755));
    M("devpts on /tmp/b", mount("ptyns", "/tmp/b", "devpts", 0, "newinstance,ptmxmode=0666,max=1"));
    unsigned nb;
    int mb = openpt("open /tmp/b/ptmx", "/tmp/b/ptmx", &nb);
    look("/tmp/b/0", "/tmp/b/0");
    look("/tmp/b/1 (the first instance's number)", "/tmp/b/1");
    unsigned nx;
    int mx = openpt("open /tmp/b/ptmx past max=1", "/tmp/b/ptmx", &nx);
    if (mx >= 0)
        close(mx);
    look("/dev/pts/0 is still the first instance's", "/dev/pts/0");

    /* The node goes with its master. */
    if (s0 >= 0)
        close(s0);
    close(m0);
    look("/dev/pts/0 after its master closed", "/dev/pts/0");

    /* A lazy unmount with a pty open: it goes on working. */
    int s1 = m1 >= 0 ? open("/dev/pts/1", O_RDWR | O_NOCTTY) : -1;
    M("umount2 /dev/pts detach", umount2("/dev/pts", MNT_DETACH));
    if (s1 >= 0) {
        M("master writes after the unmount", write(m1, "u\r", 2));
        got("slave reads after the unmount", s1);
        close(s1);
    }
    if (m1 >= 0)
        close(m1);
    if (mb >= 0)
        close(mb);
    M("umount2 /tmp/b", umount2("/tmp/b", 0));
    return 0;
}
