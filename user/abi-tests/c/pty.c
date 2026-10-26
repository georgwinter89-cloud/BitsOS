/* IN-7: pseudo-terminals. posix_openpt/grantpt/unlockpt/ptsname, the slave node, echo and output processing in
 * both directions, raw mode, poll and epoll on both ends, job control on the slave (controlling terminal on open,
 * TIOCSWINSZ on the master raising SIGWINCH, ^C from the master raising SIGINT, a background read with SIGTTIN
 * ignored), and both hang-ups (master closed: the session leader gets SIGHUP, the slave reads 0 and writes EIO;
 * slave closed: the master reads EIO and polls HUP). Only this process prints: the children report through the
 * pty or a pipe, so the transcript's order never depends on scheduling. Not printed: the pty's number. */
#include "abi.h"
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <termios.h>

static void show_bytes(const char *what, const char *b, long n) {
    printf("%s (%ld):", what, n);
    for (long i = 0; i < n; i++) {
        unsigned char c = b[i];
        if (c == '\r') printf(" \\r");
        else if (c == '\n') printf(" \\n");
        else if (c < 32 || c >= 127) printf(" %02x", c);
        else printf(" %c", c);
    }
    printf("\n");
}

/* Exactly `n` bytes (5 s at most per wait); fewer on a timeout or an end. */
static long read_n(int fd, char *b, long n) {
    long got = 0;
    while (got < n) {
        struct pollfd p = { fd, POLLIN, 0 };
        if (poll(&p, 1, 5000) <= 0)
            break;
        long r = read(fd, b + got, n - got);
        if (r <= 0)
            break;
        got += r;
    }
    return got;
}

/* One line, byte by byte (the chunks a pty hands out depend on timing; lines do not). */
static long read_line(int fd, char *b, long cap) {
    long n = 0;
    while (n < cap - 1 && read_n(fd, b + n, 1) == 1)
        if (b[n++] == '\n')
            break;
    b[n] = 0;
    return n;
}

/* Waits for `sig` with it blocked outside the wait (no lost wake-up). */
static void wait_for(volatile sig_atomic_t *flag, sigset_t *old) {
    while (!*flag)
        sigsuspend(old);
}

static int open_master(void) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0 || grantpt(m) != 0 || unlockpt(m) != 0) {
        printf("no pty: %s\n", ename(errno));
        exit(0);
    }
    return m;
}

static short revents(int fd, short ev) {
    struct pollfd p = { fd, ev, 0 };
    poll(&p, 1, 0);
    return p.revents;
}

static void basics(void) {
    int m = open_master();
    char name[64];
    snprintf(name, sizeof name, "%s", ptsname(m)); /* ptsname's buffer is static */
    unsigned n = 99;
    SYS("TIOCGPTN", SYS_ioctl, m, TIOCGPTN, &n);
    char want[32];
    snprintf(want, sizeof want, "/dev/pts/%u", n);
    yes("ptsname is /dev/pts/<TIOCGPTN>", strcmp(name, want) == 0);
    struct stat st;
    SYS("stat slave", SYS_stat, want, &st);
    printf("slave mode %o gid %u uid is mine %d\n", st.st_mode, st.st_gid, st.st_uid == getuid());
    yes("slave rdev is 136:<n>", major(st.st_rdev) == 136 && minor(st.st_rdev) == n);
    SYS("stat /dev/ptmx", SYS_stat, "/dev/ptmx", &st);
    printf("ptmx mode %o rdev %u:%u\n", st.st_mode, major(st.st_rdev), minor(st.st_rdev));
    int lk = -1;
    SYS("TIOCGPTLCK", SYS_ioctl, m, TIOCGPTLCK, &lk);
    printf("locked %d\n", lk);
    /* A second master stays locked: its slave does not open. */
    int m2 = posix_openpt(O_RDWR | O_NOCTTY);
    char name2[64];
    snprintf(name2, sizeof name2, "%s", ptsname(m2));
    SYS("open locked slave", SYS_open, name2, O_RDWR | O_NOCTTY);
    lk = 1;
    SYS("TIOCSPTLCK 1", SYS_ioctl, m2, TIOCSPTLCK, &lk);
    SYS("TIOCGPTLCK", SYS_ioctl, m2, TIOCGPTLCK, &lk);
    printf("locked %d\n", lk);
    /* No slave was ever opened: the master's write is still taken (the slave's count is 1 from the master). */
    SYS("write master before slave open", SYS_write, m2, "q", 1);
    close(m2);
    int s = open(name, O_RDWR | O_NOCTTY);
    yes("slave opens", s >= 0);
    yes("isatty master", isatty(m));
    yes("isatty slave", isatty(s));
    struct termios t, tm;
    memset(&t, 0, sizeof t);
    memset(&tm, 0, sizeof tm);
    tcgetattr(s, &t);
    printf("slave termios: iflag %#o oflag %#o cflag %#o lflag %#o vmin %d\n", t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_cc[VMIN]);
    tcgetattr(m, &tm);
    yes("master's TCGETS is the slave's", memcmp(&t, &tm, sizeof t) == 0);
    struct winsize w = { 7, 7, 7, 7 };
    SYS("TIOCGWINSZ", SYS_ioctl, s, TIOCGWINSZ, &w);
    printf("winsize %dx%d %dx%d\n", w.ws_row, w.ws_col, w.ws_xpixel, w.ws_ypixel);
    unsigned dev = 0;
    SYS("TIOCGDEV slave", SYS_ioctl, s, TIOCGDEV, &dev);
    yes("TIOCGDEV is 136:<n>", major(dev) == 136 && minor(dev) == n);
    SYS("TIOCGPTN on the slave", SYS_ioctl, s, TIOCGPTN, &n);
    int pk = 0;
    SYS("TIOCPKT 0", SYS_ioctl, m, TIOCPKT, &pk);
    SYS("TIOCGPKT", SYS_ioctl, m, TIOCGPKT, &pk);
    printf("packet %d\n", pk);
    SYS("TIOCSIG 0", SYS_ioctl, m, TIOCSIG, 0);
    SYS("TIOCNOTTY on the master", SYS_ioctl, m, TIOCNOTTY, 0);
    pid_t pg = -1;
    SYS("TIOCGPGRP on the master", SYS_ioctl, m, TIOCGPGRP, &pg);
    printf("pgrp %d\n", pg);
    SYS("TIOCGSID on the master", SYS_ioctl, m, TIOCGSID, &pg);
    SYS("TIOCGPGRP on the slave", SYS_ioctl, s, TIOCGPGRP, &pg);

    /* Echo, ICRNL and ONLCR, erase. */
    char b[256];
    printf("poll before: master %#x slave %#x\n", revents(m, POLLIN | POLLOUT), revents(s, POLLIN | POLLOUT));
    write(m, "hello\r", 6);
    show_bytes("master reads echo", b, read_n(m, b, 7));
    printf("poll slave %#x\n", revents(s, POLLIN | POLLOUT));
    int q = -1;
    SYS("FIONREAD slave", SYS_ioctl, s, FIONREAD, &q);
    printf("slave has %d\n", q);
    show_bytes("slave reads", b, read(s, b, sizeof b));
    write(m, "ab\x7f" "c\r", 5);
    show_bytes("master reads echo", b, read_n(m, b, 8));
    show_bytes("slave reads", b, read(s, b, sizeof b));
    write(s, "one\ntwo", 7);
    show_bytes("master reads output", b, read_n(m, b, 8));
    write(s, "!", 1);
    printf("poll master %#x\n", revents(m, POLLIN) & POLLIN);
    usleep(20000);
    SYS("FIONREAD master", SYS_ioctl, m, FIONREAD, &q);
    printf("master has %d\n", q);
    read_n(m, b, 1);
    /* Raw: no echo, no processing. */
    struct termios r = t;
    cfmakeraw(&r);
    SYS("tcsetattr raw", SYS_ioctl, s, TCSETS, &r);
    write(m, "x\ry", 3);
    show_bytes("slave reads raw", b, read_n(s, b, 3));
    printf("poll master after raw input %#x\n", revents(m, POLLIN));
    write(s, "\n", 1);
    show_bytes("master reads raw output", b, read_n(m, b, 1));
    /* epoll on the master. */
    int ep = epoll_create1(0);
    struct epoll_event ev = { .events = EPOLLIN, .data.u32 = 7 }, got;
    SYS("epoll_ctl master", SYS_epoll_ctl, ep, EPOLL_CTL_ADD, m, &ev);
    SYS("epoll_wait empty", SYS_epoll_wait, ep, &got, 1, 0);
    write(s, "z", 1);
    long k = SYS("epoll_wait", SYS_epoll_wait, ep, &got, 1, 5000);
    if (k == 1)
        printf("event %#x data %u\n", got.events, got.data.u32);
    read(m, b, sizeof b);
    close(ep);
    /* Nonblocking master read with nothing there. */
    fcntl(m, F_SETFL, O_NONBLOCK);
    SYS("read empty master", SYS_read, m, b, 4);
    fcntl(m, F_SETFL, 0);
    /* The last slave closes: the master polls HUP and reads EIO. */
    close(s);
    printf("poll master after slave close %#x\n", revents(m, POLLIN | POLLOUT));
    SYS("read master", SYS_read, m, b, 4);
    SYS("write master", SYS_write, m, "q", 1);
    s = open(name, O_RDWR | O_NOCTTY);
    printf("poll master after reopen %#x\n", revents(m, POLLIN));
    close(s);
    close(m);
}

static volatile sig_atomic_t winch, hup;
static void on_winch(int sig) { (void)sig; winch = 1; }
static void on_hup(int sig) { (void)sig; hup = 1; }

static void say(int fd, const char *s) { write(fd, s, strlen(s)); }

/* The child leads a session whose controlling terminal is the slave (opened without O_NOCTTY). */
static void job_child(const char *name) {
    setsid();
    /* Write-only: no controlling terminal from this open (tty_open_proc_set_tty wants read access). */
    int wo = open(name, O_WRONLY);
    int wsid = wo >= 0 ? tcgetsid(wo) : -2;
    int s = open(name, O_RDWR);
    if (s < 0)
        _exit(10);
    struct termios t;
    tcgetattr(s, &t);
    t.c_lflag &= ~ECHO;
    tcsetattr(s, TCSANOW, &t);
    sigset_t blk, old;
    sigemptyset(&blk);
    sigaddset(&blk, SIGWINCH);
    sigprocmask(SIG_BLOCK, &blk, &old);
    signal(SIGWINCH, on_winch);
    char line[64];
    snprintf(line, sizeof line, "ctty %s fg %s\n", tcgetsid(s) == getpid() ? "mine" : "other",
             tcgetpgrp(s) == getpgrp() ? "mine" : "other");
    say(s, line);
    snprintf(line, sizeof line, "wronly open ctty %s\n", wsid == getpid() ? "yes" : "no");
    say(s, line);
    close(wo);
    /* A background group with SIGTTIN ignored reads: EIO. */
    pid_t g = fork();
    if (g == 0) {
        setpgid(0, 0);
        signal(SIGTTIN, SIG_IGN);
        char c;
        long r = read(s, &c, 1);
        snprintf(line, sizeof line, "bg read %ld %s\n", r, r < 0 ? ename(errno) : "");
        say(s, line);
        _exit(0);
    }
    waitpid(g, 0, 0);
    say(s, "ready\n");
    wait_for(&winch, &old);
    struct winsize w;
    ioctl(s, TIOCGWINSZ, &w);
    snprintf(line, sizeof line, "winch %dx%d\n", w.ws_row, w.ws_col);
    say(s, line);
    for (;;)
        pause(); /* until ^C */
}

static void job_control(void) {
    int m = open_master();
    char name[64];
    strcpy(name, ptsname(m));
    fflush(stdout);
    pid_t c = fork();
    if (c == 0)
        job_child(name);
    char b[256];
    for (int i = 0; i < 4; i++) {
        long n = read_line(m, b, sizeof b);
        show_bytes("child says", b, n);
        if (strstr(b, "ready"))
            break;
    }
    pid_t pg = -1;
    ioctl(m, TIOCGPGRP, &pg);
    yes("master TIOCGPGRP is the child", pg == c);
    ioctl(m, TIOCGSID, &pg);
    yes("master TIOCGSID is the child", pg == c);
    struct winsize w = { 24, 80, 0, 0 };
    SYS("TIOCSWINSZ on the master", SYS_ioctl, m, TIOCSWINSZ, &w);
    long n = read_line(m, b, sizeof b);
    show_bytes("child says", b, n);
    SYS("write ^C", SYS_write, m, "\x03", 1);
    int st = 0;
    waitpid(c, &st, 0);
    printf("child: signaled %d sig %d\n", WIFSIGNALED(st), WIFSIGNALED(st) ? WTERMSIG(st) : -1);
    close(m);
}

/* The master closes under a session leader: SIGHUP, then reads 0, writes EIO, poll all of IN|OUT|ERR|HUP. */
static void hangup(void) {
    int m = open_master();
    char name[64];
    strcpy(name, ptsname(m));
    int pp[2];
    pipe(pp);
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        close(pp[0]);
        close(m); /* else the master stays open in here */
        sigset_t blk, old;
        sigemptyset(&blk);
        sigaddset(&blk, SIGHUP);
        sigprocmask(SIG_BLOCK, &blk, &old);
        signal(SIGHUP, on_hup);
        setsid();
        int s = open(name, O_RDWR);
        say(pp[1], "R");
        wait_for(&hup, &old);
        char b[8], line[96];
        long r = read(s, b, sizeof b);
        long wr = write(s, "x", 1);
        int we = errno;
        struct pollfd p = { s, POLLIN | POLLOUT, 0 };
        poll(&p, 1, 0);
        struct stat st;
        int gone = stat(name, &st) < 0 && errno == ENOENT;
        long io = ioctl(s, TCFLSH, TCIFLUSH);
        int ie = errno;
        snprintf(line, sizeof line, "hup: read %ld write %ld %s poll %#x node gone %d ioctl %ld %s\n", r, wr, ename(we), p.revents, gone, io, ename(ie));
        say(pp[1], line);
        _exit(0);
    }
    close(pp[1]);
    char b[160];
    long n = read(pp[0], b, 1);
    usleep(20000);
    close(m);
    n = read_line(pp[0], b, sizeof b);
    printf("%s", n > 0 ? b : "no answer\n");
    int st = 0;
    waitpid(c, &st, 0);
    printf("child exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    close(pp[0]);
}

/* `/dev/tty` of a session whose controlling terminal is a pty is that pty (tty_open_current_tty). */
static void devtty(void) {
    int m = open_master();
    char name[64];
    strcpy(name, ptsname(m));
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        setsid();
        int s = open(name, O_RDWR);
        int t = open("/dev/tty", O_RDWR);
        char line[96];
        snprintf(line, sizeof line, "devtty open %s sid mine %s\n", t >= 0 ? "ok" : ename(errno), t >= 0 && tcgetsid(t) == getpid() ? "yes" : "no");
        say(s, line);
        if (t >= 0)
            say(t, "via /dev/tty\n");
        _exit(0);
    }
    char b[128];
    for (int i = 0; i < 2; i++)
        show_bytes("child says", b, read_line(m, b, sizeof b));
    int st = 0;
    waitpid(c, &st, 0);
    close(m);
}

#ifndef TIOCGPTPEER
#define TIOCGPTPEER _IO('T', 0x41)
#endif

/* TIOCGPTPEER: the master opens its own slave (EIO while locked or on a slave, ENOTTY on a non-tty). */
static void peer(void) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0) {
        printf("no pty: %s\n", ename(errno));
        return;
    }
    long r = ioctl(m, TIOCGPTPEER, O_RDWR | O_NOCTTY);
    printf("locked: %s\n", r >= 0 ? "opened" : ename(errno));
    if (r >= 0)
        close(r);
    unlockpt(m);
    int s = ioctl(m, TIOCGPTPEER, O_RDWR | O_NOCTTY | O_CLOEXEC);
    printf("peer: %s cloexec %d\n", s >= 0 ? "opened" : ename(errno), s >= 0 ? (fcntl(s, F_GETFD) & FD_CLOEXEC) : -1);
    if (s < 0)
        return;
    struct stat a, b;
    printf("same node: %d\n", stat(ptsname(m), &a) == 0 && fstat(s, &b) == 0 && a.st_rdev == b.st_rdev);
    int ro = ioctl(m, TIOCGPTPEER, O_RDONLY | O_NOCTTY);
    printf("read-only peer: access mode %d\n", ro >= 0 ? fcntl(ro, F_GETFL) & O_ACCMODE : -1);
    if (ro >= 0)
        close(ro);
    write(s, "hi\n", 3);
    char b2[16];
    show_bytes("master reads", b2, read_n(m, b2, 4));
    r = ioctl(s, TIOCGPTPEER, O_RDWR);
    printf("on the slave: %ld %s\n", r, r < 0 ? ename(errno) : "");
    int z = open("/dev/null", O_RDWR);
    r = ioctl(z, TIOCGPTPEER, O_RDWR);
    printf("on /dev/null: %ld %s\n", r, r < 0 ? ename(errno) : "");
    close(z);
    close(s);
    close(m);
}

int main(int argc, char **argv) {
    const char *what = argc > 1 ? argv[1] : "basics";
    if (!strcmp(what, "basics"))
        basics();
    else if (!strcmp(what, "jobs"))
        job_control();
    else if (!strcmp(what, "hangup"))
        hangup();
    else if (!strcmp(what, "devtty"))
        devtty();
    else if (!strcmp(what, "peer"))
        peer();
    else
        printf("usage: abi-pty basics|jobs|hangup|devtty|peer\n");
    return 0;
}
