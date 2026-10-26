/* fs-inotify: inotify as programs see it (FS-14), for the ABI diff (CachyOS is the oracle;
 * user/abi-tests/c/inotify.c is the same file) and the ktest fs.inotify.programs. Prints event masks by name,
 * names, wds and cookie relations only (no cookie values, inode numbers or limits the host decides).
 * Modes (argv[1]):
 *   checks    inotify_init1/add_watch/rm_watch errors in Linux's order, wd numbering, read's EINVAL/EAGAIN,
 *             FIONREAD, the record layout (name padding), /proc/sys/fs/inotify readable
 *   dir       a watched directory: create, open, write, close, chmod, read, getdents, rename (one cookie), link,
 *             unlink, mkdir, rmdir, O_TRUNC, ftruncate, utimes, fallocate, O_PATH, the directory removed
 *   self      a watched file: link count changes, rename, unlink while open (DELETE_SELF at the last close),
 *             IN_ONESHOT, IN_MASK_ADD, IN_EXCL_UNLINK, IN_DONT_FOLLOW on a symlink
 *   wait      a blocking read woken by a child's create, poll and epoll readiness, IN_Q_OVERFLOW after
 *             max_queued_events (16384) events */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

static char base[64];

static const char *en(int e) {
    static char b[16];
    switch (e) {
    case 0: return "0";
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EBADF: return "EBADF";
    case EAGAIN: return "EAGAIN";
    case EACCES: return "EACCES";
    case EFAULT: return "EFAULT";
    case EEXIST: return "EEXIST";
    case ENOTDIR: return "ENOTDIR";
    case EINVAL: return "EINVAL";
    case EMFILE: return "EMFILE";
    case ENOSPC: return "ENOSPC";
    case ENOSYS: return "ENOSYS";
    }
    snprintf(b, sizeof b, "errno%d", e);
    return b;
}

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...) {
    char line[512];
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(line, sizeof line - 1, fmt, ap);
    va_end(ap);
    if (n < 0) return;
    if (n > (int)sizeof line - 2) n = sizeof line - 2;
    line[n++] = '\n';
    if (write(1, line, n) < 0) _exit(3);
}

static void res(const char *label, long r) { say("%s: %s", label, r < 0 ? en(errno) : "ok"); }
static void num(const char *label, long r) {
    if (r < 0) say("%s: %s", label, en(errno));
    else say("%s: %ld", label, r);
}

static const char *P(const char *rel) {
    static char b[4][128];
    static int i;
    i = (i + 1) % 4;
    snprintf(b[i], sizeof b[i], "%s/%s", base, rel);
    return b[i];
}

static const struct { unsigned bit; const char *name; } BITS[] = {
    {IN_ACCESS, "ACCESS"}, {IN_MODIFY, "MODIFY"}, {IN_ATTRIB, "ATTRIB"}, {IN_CLOSE_WRITE, "CLOSE_WRITE"},
    {IN_CLOSE_NOWRITE, "CLOSE_NOWRITE"}, {IN_OPEN, "OPEN"}, {IN_MOVED_FROM, "MOVED_FROM"}, {IN_MOVED_TO, "MOVED_TO"},
    {IN_CREATE, "CREATE"}, {IN_DELETE, "DELETE"}, {IN_DELETE_SELF, "DELETE_SELF"}, {IN_MOVE_SELF, "MOVE_SELF"},
    {IN_UNMOUNT, "UNMOUNT"}, {IN_Q_OVERFLOW, "Q_OVERFLOW"}, {IN_IGNORED, "IGNORED"}, {IN_ISDIR, "ISDIR"},
};

static void mask_name(unsigned m, char *out, size_t n) {
    out[0] = 0;
    for (size_t i = 0; i < sizeof BITS / sizeof BITS[0]; i++)
        if (m & BITS[i].bit) {
            if (out[0]) strncat(out, "|", n - strlen(out) - 1);
            strncat(out, BITS[i].name, n - strlen(out) - 1);
            m &= ~BITS[i].bit;
        }
    if (m) snprintf(out + strlen(out), n - strlen(out), "|0x%x", m);
}

/* Reads what is queued (O_NONBLOCK fd) and prints one line per event; cookies as "cookie=" + a letter per
 * distinct value (a, b, ...), 0 as "-". */
static void drain(int fd, const char *what) {
    static unsigned cookies[64];
    static int ncookies;
    char buf[8192] __attribute__((aligned(8)));
    int any = 0;
    for (;;) {
        ssize_t n = read(fd, buf, sizeof buf);
        if (n <= 0) break;
        for (char *p = buf; p < buf + n;) {
            struct inotify_event *e = (struct inotify_event *)p;
            char m[128], c[8] = "-";
            mask_name(e->mask, m, sizeof m);
            if (e->cookie) {
                int k = 0;
                while (k < ncookies && cookies[k] != e->cookie) k++;
                if (k == ncookies && ncookies < 64) cookies[ncookies++] = e->cookie;
                snprintf(c, sizeof c, "%c", 'a' + k % 26);
            }
            say("%s: wd %d %s %s cookie=%s len %u", what, e->wd, m, e->len ? e->name : "(none)", c, e->len);
            p += sizeof *e + e->len;
            any = 1;
        }
    }
    if (!any) say("%s: no events", what);
}

static int touch(const char *p) {
    int f = open(p, O_CREAT | O_WRONLY, 0644);
    if (f >= 0) close(f);
    return f < 0 ? -1 : 0;
}

static int checks(void) {
    errno = 0;
    res("init1 bad flag", inotify_init1(1));
    int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (fd < 0) {
        say("init1: %s", en(errno));
        return 1;
    }
    say("init1 fd cloexec: %s", fcntl(fd, F_GETFD) & FD_CLOEXEC ? "yes" : "no");
    say("init1 F_GETFL nonblock: %s", fcntl(fd, F_GETFL) & O_NONBLOCK ? "yes" : "no");
    int old = syscall(SYS_inotify_init);
    say("inotify_init: %s", old >= 0 ? "ok" : en(errno));
    if (old >= 0) close(old);
    num("add mask 0", inotify_add_watch(fd, base, 0));
    num("add unknown bit", inotify_add_watch(fd, base, 0x08000000));
    num("add bad fd", inotify_add_watch(-1, base, IN_CREATE));
    num("add MASK_ADD|MASK_CREATE", inotify_add_watch(fd, base, IN_CREATE | IN_MASK_ADD | IN_MASK_CREATE));
    touch(P("plain"));
    int plain = open(P("plain"), O_RDONLY);
    num("add on a plain fd", inotify_add_watch(plain, base, IN_CREATE));
    num("rm on a plain fd", inotify_rm_watch(plain, 1));
    close(plain);
    num("add ONLYDIR on a file", inotify_add_watch(fd, P("plain"), IN_CREATE | IN_ONLYDIR));
    num("add missing", inotify_add_watch(fd, P("nothing"), IN_CREATE));
    num("add empty path", inotify_add_watch(fd, "", IN_CREATE));
    num("add bad path pointer", inotify_add_watch(fd, (const char *)8, IN_CREATE));
    num("add dir", inotify_add_watch(fd, base, IN_CREATE));
    num("add file", inotify_add_watch(fd, P("plain"), IN_MODIFY));
    num("add dir again", inotify_add_watch(fd, base, IN_DELETE));
    num("add dir MASK_CREATE", inotify_add_watch(fd, base, IN_CREATE | IN_MASK_CREATE));
    num("rm unknown wd", inotify_rm_watch(fd, 77));
    num("rm bad fd", inotify_rm_watch(-1, 1));
    num("rm file watch", inotify_rm_watch(fd, 2));
    drain(fd, "after rm");
    num("rm it again", inotify_rm_watch(fd, 2));
    num("add file again", inotify_add_watch(fd, P("plain"), IN_MODIFY));
    char b[64];
    errno = 0;
    num("read empty nonblock", read(fd, b, sizeof b));
    num("dir back to CREATE", inotify_add_watch(fd, base, IN_CREATE));
    touch(P("abcdefghijklmnop"));
    touch(P("x"));
    int q = -1;
    int qr = ioctl(fd, FIONREAD, &q);
    say("FIONREAD: %s %d", qr < 0 ? en(errno) : "ok", q);
    num("read 47 bytes", read(fd, b, 47));
    num("read 0 bytes", read(fd, b, 0));
    char big[256] __attribute__((aligned(8)));
    num("read 60 bytes (one event)", read(fd, big, 60));
    struct inotify_event *e = (struct inotify_event *)big;
    say("first: wd %d len %u name %s", e->wd, e->len, e->len ? e->name : "(none)");
    num("read the rest", read(fd, big, sizeof big));
    say("lseek: %ld", (long)lseek(fd, 0, SEEK_CUR));
    num("write", write(fd, "x", 1));
    struct stat st;
    fstat(fd, &st);
    say("fstat mode %o", st.st_mode);
    char link[64] = {0};
    snprintf(b, sizeof b, "/proc/self/fd/%d", fd);
    memset(link, 0, sizeof link);
    if (readlink(b, link, sizeof link - 1) < 0) snprintf(link, sizeof link, "%s", en(errno));
    say("fd link: %s", link);
    const char *knobs[] = {"max_queued_events", "max_user_instances", "max_user_watches"};
    for (int i = 0; i < 3; i++) {
        char kp[96], v[32] = {0};
        snprintf(kp, sizeof kp, "/proc/sys/fs/inotify/%s", knobs[i]);
        int k = open(kp, O_RDONLY);
        long got = k < 0 ? -1 : read(k, v, sizeof v - 1);
        if (k >= 0) close(k);
        v[strcspn(v, "\n")] = 0;
        if (i == 0) say("%s: %s", knobs[i], got > 0 ? v : en(errno));
        else say("%s: %s", knobs[i], got > 0 ? "readable" : en(errno));
    }
    close(fd);
    unlink(P("plain"));
    unlink(P("abcdefghijklmnop"));
    unlink(P("x"));
    return 0;
}

static int dir_mode(void) {
    mkdir(P("w"), 0755);
    int fd = inotify_init1(IN_NONBLOCK);
    num("watch w", inotify_add_watch(fd, P("w"), IN_ALL_EVENTS));
    int f = open(P("w/f"), O_CREAT | O_WRONLY, 0644);
    write(f, "abc", 3);
    write(f, "abc", 3);
    close(f);
    drain(fd, "create+2 writes+close");
    chmod(P("w/f"), 0600);
    drain(fd, "chmod");
    char b[16];
    f = open(P("w/f"), O_RDONLY);
    read(f, b, sizeof b);
    read(f, b, sizeof b);
    close(f);
    drain(fd, "open+read+eof+close");
    DIR *d = opendir(P("w"));
    while (d && readdir(d)) {}
    if (d) closedir(d);
    drain(fd, "ls w");
    rename(P("w/f"), P("w/g"));
    drain(fd, "rename f g");
    link(P("w/g"), P("w/h"));
    drain(fd, "link g h");
    unlink(P("w/h"));
    drain(fd, "unlink h");
    f = open(P("w/g"), O_WRONLY | O_TRUNC);
    close(f);
    drain(fd, "O_TRUNC open");
    truncate(P("w/g"), 100);
    drain(fd, "truncate");
    f = open(P("w/g"), O_WRONLY);
    ftruncate(f, 10);
    syscall(SYS_fallocate, f, 0, 0, 8192);
    syscall(SYS_fallocate, f, 1, 0, 100);
    close(f);
    drain(fd, "ftruncate+fallocate+KEEP_SIZE");
    struct timeval tv[2] = {{1, 0}, {2, 0}};
    utimes(P("w/g"), tv);
    drain(fd, "utimes");
    f = open(P("w/g"), O_PATH);
    close(f);
    drain(fd, "O_PATH open+close");
    symlink("g", P("w/s"));
    mkfifo(P("w/p"), 0600);
    drain(fd, "symlink+mkfifo");
    rename(P("w/s"), P("w/g"));
    drain(fd, "rename s over g");
    unlink(P("w/g"));
    unlink(P("w/p"));
    drain(fd, "unlink g p");
    mkdir(P("w/d"), 0755);
    num("watch w/d", inotify_add_watch(fd, P("w/d"), IN_ALL_EVENTS));
    touch(P("w/d/x"));
    drain(fd, "mkdir d + file in it");
    rename(P("w/d"), P("w/e"));
    drain(fd, "rename d e");
    unlink(P("w/e/x"));
    rmdir(P("w/e"));
    drain(fd, "rmdir e");
    rmdir(P("w"));
    drain(fd, "rmdir w");
    close(fd);
    return 0;
}

static int self_mode(void) {
    touch(P("s"));
    int fd = inotify_init1(IN_NONBLOCK);
    num("watch s", inotify_add_watch(fd, P("s"), IN_ALL_EVENTS));
    link(P("s"), P("s2"));
    drain(fd, "link");
    unlink(P("s"));
    drain(fd, "unlink one name");
    rename(P("s2"), P("s3"));
    drain(fd, "rename");
    int h = open(P("s3"), O_RDONLY);
    unlink(P("s3"));
    drain(fd, "unlink while open");
    close(h);
    drain(fd, "last close");
    touch(P("o"));
    num("oneshot", inotify_add_watch(fd, P("o"), IN_OPEN | IN_ONESHOT));
    close(open(P("o"), O_RDONLY));
    close(open(P("o"), O_RDONLY));
    drain(fd, "oneshot two opens");
    num("watch o OPEN", inotify_add_watch(fd, P("o"), IN_OPEN));
    num("MASK_ADD CLOSE_NOWRITE", inotify_add_watch(fd, P("o"), IN_CLOSE_NOWRITE | IN_MASK_ADD));
    close(open(P("o"), O_RDONLY));
    drain(fd, "open after MASK_ADD");
    num("replace with ATTRIB", inotify_add_watch(fd, P("o"), IN_ATTRIB));
    close(open(P("o"), O_RDONLY));
    chmod(P("o"), 0600);
    drain(fd, "open+chmod after replace");
    mkdir(P("x"), 0755);
    int w = open(P("x/f"), O_CREAT | O_WRONLY, 0644);
    num("watch x EXCL_UNLINK", inotify_add_watch(fd, P("x"), IN_MODIFY | IN_CLOSE_WRITE | IN_EXCL_UNLINK));
    unlink(P("x/f"));
    write(w, "z", 1);
    close(w);
    drain(fd, "write+close after unlink (EXCL_UNLINK)");
    w = open(P("x/f2"), O_CREAT | O_WRONLY, 0644);
    num("watch x plain", inotify_add_watch(fd, P("x"), IN_MODIFY | IN_CLOSE_WRITE));
    unlink(P("x/f2"));
    write(w, "z", 1);
    close(w);
    drain(fd, "write+close after unlink");
    rmdir(P("x"));
    drain(fd, "rmdir x");
    symlink("o", P("l"));
    num("DONT_FOLLOW on a symlink", inotify_add_watch(fd, P("l"), IN_ATTRIB | IN_DONT_FOLLOW));
    num("follow the symlink", inotify_add_watch(fd, P("l"), IN_ATTRIB));
    num("ONLYDIR|DONT_FOLLOW on a symlink", inotify_add_watch(fd, P("l"), IN_ATTRIB | IN_ONLYDIR | IN_DONT_FOLLOW));
    chmod(P("o"), 0644);
    drain(fd, "chmod through the link");
    unlink(P("l"));
    unlink(P("o"));
    drain(fd, "unlink l o");
    close(fd);
    return 0;
}

static int wait_mode(void) {
    int fd = inotify_init1(0);
    mkdir(P("v"), 0755);
    num("watch v", inotify_add_watch(fd, P("v"), IN_CREATE | IN_MODIFY));
    struct pollfd pf = {fd, POLLIN, 0};
    int pr = poll(&pf, 1, 0);
    say("poll empty: %d revents %x", pr, pf.revents);
    int ep = epoll_create1(0);
    struct epoll_event ee = {.events = EPOLLIN, .data.u32 = 7}, got;
    res("epoll_ctl", epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ee));
    pid_t pid = fork();
    if (pid == 0) {
        usleep(100000);
        touch(P("v/late"));
        _exit(0);
    }
    char buf[256] __attribute__((aligned(8)));
    long n = read(fd, buf, sizeof buf);
    struct inotify_event *e = (struct inotify_event *)buf;
    say("blocking read: %ld bytes, %s", n, n > 0 ? e->name : en(errno));
    waitpid(pid, NULL, 0);
    touch(P("v/now"));
    pr = poll(&pf, 1, 0);
    say("poll ready: %d revents %x", pr, pf.revents);
    int k = epoll_wait(ep, &got, 1, 1000);
    say("epoll_wait: %d events %x data %u", k, k > 0 ? got.events : 0, k > 0 ? got.data.u32 : 0);
    read(fd, buf, sizeof buf);
    close(ep);
    /* IN_Q_OVERFLOW: alternate writes to two files (no merging) past max_queued_events. */
    int a = open(P("v/a"), O_CREAT | O_WRONLY, 0644), b = open(P("v/b"), O_CREAT | O_WRONLY, 0644);
    char big[65536] __attribute__((aligned(8)));
    fcntl(fd, F_SETFL, O_NONBLOCK);
    while (read(fd, big, sizeof big) > 0) {}
    for (int i = 0; i < 16400; i++) write(i & 1 ? b : a, "x", 1);
    long events = 0, overflow = 0, after = 0;
    for (;;) {
        long r = read(fd, big, sizeof big);
        if (r <= 0) break;
        for (char *p = big; p < big + r;) {
            struct inotify_event *x = (struct inotify_event *)p;
            if (x->mask & IN_Q_OVERFLOW) overflow++, say("overflow: wd %d len %u after %ld events", x->wd, x->len, events);
            else if (overflow) after++;
            else events++;
            p += sizeof *x + x->len;
        }
    }
    say("queued %ld events, %ld overflow, %ld after it", events, overflow, after);
    write(a, "x", 1);
    write(b, "x", 1);
    drain(fd, "after the overflow was read");
    close(a);
    close(b);
    unlink(P("v/a"));
    unlink(P("v/b"));
    unlink(P("v/late"));
    unlink(P("v/now"));
    rmdir(P("v"));
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    const char *m = argc > 1 ? argv[1] : "checks";
    snprintf(base, sizeof base, "/tmp/fs-inotify-%d", (int)getpid());
    if (mkdir(base, 0755) < 0) {
        say("mkdir base: %s", en(errno));
        return 1;
    }
    int rc = 2;
    if (!strcmp(m, "checks")) rc = checks();
    else if (!strcmp(m, "dir")) rc = dir_mode();
    else if (!strcmp(m, "self")) rc = self_mode();
    else if (!strcmp(m, "wait")) rc = wait_mode();
    else say("unknown mode %s", m);
    rmdir(base);
    return rc;
}
