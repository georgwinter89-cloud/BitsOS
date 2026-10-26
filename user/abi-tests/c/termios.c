/* IN-4: the terminal ioctls on a terminal of its own. CachyOS: a pty slave (the harness has no terminal and a
 * non-root program gets no console); V2: the console /dev/tty0 (no ptys yet). Both run the same line
 * discipline rules (n_tty), so after setting a known termios first every answer must match. Not printed:
 * what differs between a pty and a VT by design (the start settings, the VT's cflag hardware bits, the VT-only
 * KD* calls). The program makes itself a session leader without a controlling terminal first (setsid; on V2
 * it already is one, and setsid's answer is not printed). */
#include "abi.h"
#include <fcntl.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <termios.h>

/* The kernel's struct termios2 (asm/termbits.h; not included: it clashes with musl's termios.h). */
struct ktermios2 {
    unsigned c_iflag, c_oflag, c_cflag, c_lflag;
    unsigned char c_line, c_cc[19];
    unsigned c_ispeed, c_ospeed;
};
struct ktermio {
    unsigned short c_iflag, c_oflag, c_cflag, c_lflag;
    unsigned char c_line, c_cc[8];
};
#define K_TCGETS2 0x802C542Aul
#define K_TCSETS2 0x402C542Bul
#define K_TCGETA 0x5405
#define K_TCSETA 0x5406

static int open_terminal(void) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m >= 0) {
        if (grantpt(m) == 0 && unlockpt(m) == 0) {
            char *name = ptsname(m);
            if (name) {
                int s = open(name, O_RDWR | O_NOCTTY);
                if (s >= 0)
                    return s; /* the master stays open (leaked on purpose until exit) */
            }
        }
        close(m);
    }
    return open("/dev/tty0", O_RDWR | O_NOCTTY);
}

static void show_termios(const char *what, struct termios *t) {
    printf("%s: iflag %#o oflag %#o lflag %#o cs8 %d vmin %d vtime %d intr %d erase %d eof %d\n", what,
           t->c_iflag, t->c_oflag, t->c_lflag, (t->c_cflag & CSIZE) == CS8, t->c_cc[VMIN], t->c_cc[VTIME],
           t->c_cc[VINTR], t->c_cc[VERASE], t->c_cc[VEOF]);
}

int main(void) {
    signal(SIGHUP, SIG_IGN); /* TIOCNOTTY below sends SIGHUP to our own group */
    setsid();
    int fd = open_terminal();
    if (fd < 0) {
        printf("no terminal: %s\n", ename(errno));
        return 0;
    }
    yes("isatty", isatty(fd));
    struct termios t;
    SYS("TCGETS", SYS_ioctl, fd, TCGETS, &t);
    /* A known state: the Linux console's settings. */
    t.c_iflag = ICRNL | IXON | IUTF8;
    t.c_oflag = OPOST | ONLCR;
    t.c_lflag = ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN;
    t.c_cc[VMIN] = 1;
    t.c_cc[VTIME] = 0;
    SYS("TCSETS known", SYS_ioctl, fd, TCSETS, &t);
    struct termios g;
    memset(&g, 0, sizeof g);
    SYS("TCGETS after", SYS_ioctl, fd, TCGETS, &g);
    show_termios("known", &g);
    /* cfmakeraw by hand, TCSETSF (flush), then back with TCSETSW. */
    struct termios r = g;
    r.c_iflag &= ~(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON);
    r.c_oflag &= ~OPOST;
    r.c_lflag &= ~(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
    r.c_cc[VMIN] = 0;
    r.c_cc[VTIME] = 3;
    SYS("TCSETSF raw", SYS_ioctl, fd, TCSETSF, &r);
    SYS("TCGETS raw", SYS_ioctl, fd, TCGETS, &g);
    show_termios("raw", &g);
    SYS("TCSETSW known", SYS_ioctl, fd, TCSETSW, &t);
    /* termios2 and the old termio. */
    struct ktermios2 t2;
    SYS("TCGETS2", SYS_ioctl, fd, K_TCGETS2, &t2);
    printf("termios2: ispeed %u ospeed %u lflag %#o\n", t2.c_ispeed, t2.c_ospeed, t2.c_lflag);
    t2.c_lflag &= ~ECHO;
    SYS("TCSETS2 no echo", SYS_ioctl, fd, K_TCSETS2, &t2);
    SYS("TCGETS", SYS_ioctl, fd, TCGETS, &g);
    yes("ECHO off after TCSETS2", !(g.c_lflag & ECHO));
    struct ktermio o;
    SYS("TCGETA", SYS_ioctl, fd, K_TCGETA, &o);
    printf("termio: lflag %#o intr %d\n", o.c_lflag, o.c_cc[0]);
    o.c_lflag |= ECHO;
    SYS("TCSETA echo", SYS_ioctl, fd, K_TCSETA, &o);
    SYS("TCGETS", SYS_ioctl, fd, TCGETS, &g);
    show_termios("after termio", &g);
    /* Bad pointers. */
    SYS("TCGETS bad pointer", SYS_ioctl, fd, TCGETS, KERNEL_ADDR);
    SYS("TCSETS bad pointer", SYS_ioctl, fd, TCSETS, KERNEL_ADDR);
    SYS("TIOCGWINSZ bad pointer", SYS_ioctl, fd, TIOCGWINSZ, KERNEL_ADDR);
    SYS("TIOCSWINSZ bad pointer", SYS_ioctl, fd, TIOCSWINSZ, KERNEL_ADDR);
    /* Window size. */
    struct winsize w = { 24, 80, 0, 0 }, w2;
    SYS("TIOCSWINSZ 24x80", SYS_ioctl, fd, TIOCSWINSZ, &w);
    SYS("TIOCGWINSZ", SYS_ioctl, fd, TIOCGWINSZ, &w2);
    printf("winsize %d rows %d cols\n", w2.ws_row, w2.ws_col);
    /* Queues, flushing, flow control, break, line discipline, exclusive mode. */
    int n = -1;
    SYS("FIONREAD", SYS_ioctl, fd, FIONREAD, &n);
    printf("FIONREAD %d\n", n);
    n = -1;
    SYS("TIOCOUTQ", SYS_ioctl, fd, TIOCOUTQ, &n);
    printf("TIOCOUTQ %d\n", n);
    SYS("TCFLSH TCIFLUSH", SYS_ioctl, fd, TCFLSH, TCIFLUSH);
    SYS("TCFLSH TCIOFLUSH", SYS_ioctl, fd, TCFLSH, TCIOFLUSH);
    SYS("TCFLSH 7", SYS_ioctl, fd, TCFLSH, 7);
    SYS("TCXONC TCOOFF", SYS_ioctl, fd, TCXONC, TCOOFF);
    SYS("TCXONC TCOON", SYS_ioctl, fd, TCXONC, TCOON);
    SYS("TCXONC 9", SYS_ioctl, fd, TCXONC, 9);
    SYS("TCSBRK 1 (tcdrain)", SYS_ioctl, fd, TCSBRK, 1);
    n = -1;
    SYS("TIOCGETD", SYS_ioctl, fd, TIOCGETD, &n);
    printf("line discipline %d\n", n);
    n = 0;
    SYS("TIOCSETD N_TTY", SYS_ioctl, fd, TIOCSETD, &n);
    n = 99;
    SYS("TIOCSETD 99", SYS_ioctl, fd, TIOCSETD, &n);
    SYS("TIOCEXCL", SYS_ioctl, fd, TIOCEXCL, 0);
    n = -1;
    SYS("TIOCGEXCL", SYS_ioctl, fd, TIOCGEXCL, &n);
    printf("exclusive %d\n", n);
    SYS("TIOCNXCL", SYS_ioctl, fd, TIOCNXCL, 0);
    char c = 'x';
    SYS("TIOCSTI (not root)", SYS_ioctl, fd, TIOCSTI, &c);
    SYS("unknown 0x54ff", SYS_ioctl, fd, 0x54ff, 0);
    /* Job control: not our terminal yet, then the controlling terminal of our session. */
    pid_t pg = -1;
    SYS("TIOCGPGRP no ctty", SYS_ioctl, fd, TIOCGPGRP, &pg);
    SYS("TIOCGSID no ctty", SYS_ioctl, fd, TIOCGSID, &pg);
    SYS("TIOCNOTTY no ctty", SYS_ioctl, fd, TIOCNOTTY, 0);
    SYS("TIOCSCTTY", SYS_ioctl, fd, TIOCSCTTY, 0);
    SYS("TIOCSCTTY again", SYS_ioctl, fd, TIOCSCTTY, 0);
    pg = -1;
    SYS("TIOCGPGRP", SYS_ioctl, fd, TIOCGPGRP, &pg);
    yes("foreground group is ours", pg == getpgrp());
    pid_t sid = -1;
    SYS("TIOCGSID", SYS_ioctl, fd, TIOCGSID, &sid);
    yes("session is ours (we lead it)", sid == getpid());
    pid_t want = getpgrp();
    SYS("TIOCSPGRP own group", SYS_ioctl, fd, TIOCSPGRP, &want);
    want = -1;
    SYS("TIOCSPGRP -1", SYS_ioctl, fd, TIOCSPGRP, &want);
    want = 0x7ffffff0;
    SYS("TIOCSPGRP no such group", SYS_ioctl, fd, TIOCSPGRP, &want);
    SYS("TIOCSPGRP bad pointer", SYS_ioctl, fd, TIOCSPGRP, KERNEL_ADDR);
    SYS("TIOCGPGRP bad pointer", SYS_ioctl, fd, TIOCGPGRP, KERNEL_ADDR);
    SYS("TIOCNOTTY", SYS_ioctl, fd, TIOCNOTTY, 0);
    SYS("TIOCGPGRP after TIOCNOTTY", SYS_ioctl, fd, TIOCGPGRP, &pg);
    /* Leave the terminal as the console wants it. */
    SYS("TCSETS known", SYS_ioctl, fd, TCSETS, &t);
    close(fd);
    return 0;
}
