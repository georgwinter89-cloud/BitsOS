/* IN-4: a raw-mode program on a terminal (the ktests in.tty.program_* run it on the V2 console with injected
 * keys). Without arguments, as the ABI diff runs it (stdin a pipe), it only says that stdin is no terminal.
 *   raw    cfmakeraw, VMIN 1: prints each read in hex until a 'q' arrives, then one VMIN 0 VTIME 2 read
 *          (times out: 0 bytes), restores the settings with TCSAFLUSH and ends with 0
 *   wait   a canonical read that should never end by itself (the test ends it with Ctrl+C) */
#include "abi.h"
#include <termios.h>

int main(int argc, char **argv) {
    if (!isatty(0)) {
        printf("rawtty: stdin is not a terminal (%s)\n", ename(errno));
        return 0;
    }
    if (argc > 1 && strcmp(argv[1], "wait") == 0) {
        printf("waiting\n");
        fflush(stdout);
        char b[64];
        long n = read(0, b, sizeof b);
        printf("read returned %ld (%s)\n", n, n < 0 ? ename(errno) : "data");
        return 3;
    }
    struct termios old, t;
    if (tcgetattr(0, &old) != 0) {
        printf("tcgetattr: %s\n", ename(errno));
        return 1;
    }
    t = old;
    cfmakeraw(&t);
    t.c_cc[VMIN] = 1;
    t.c_cc[VTIME] = 0;
    if (tcsetattr(0, TCSANOW, &t) != 0) {
        printf("tcsetattr: %s\n", ename(errno));
        return 1;
    }
    struct termios check;
    tcgetattr(0, &check);
    printf("raw: icanon %d echo %d isig %d opost %d\n", !!(check.c_lflag & ICANON), !!(check.c_lflag & ECHO),
           !!(check.c_lflag & ISIG), !!(check.c_oflag & OPOST));
    printf("ready\n");
    fflush(stdout);
    int done = 0;
    for (int reads = 0; !done && reads < 64; reads++) {
        unsigned char b[16];
        long n = read(0, b, sizeof b);
        if (n <= 0) {
            printf("read %ld (%s)\n", n, n < 0 ? ename(errno) : "eof");
            break;
        }
        printf("got %ld:", n);
        for (long i = 0; i < n; i++) {
            printf(" %02x", b[i]);
            if (b[i] == 'q')
                done = 1;
        }
        printf("\n");
        fflush(stdout);
    }
    t.c_cc[VMIN] = 0;
    t.c_cc[VTIME] = 2;
    tcsetattr(0, TCSANOW, &t);
    uint64_t t0 = ns_of(CLOCK_MONOTONIC);
    char b[8];
    long n = read(0, b, sizeof b);
    uint64_t waited = ns_of(CLOCK_MONOTONIC) - t0;
    printf("timed read = %ld after %s\n", n, waited >= 100000000u ? "a wait" : "no wait");
    if (tcsetattr(0, TCSAFLUSH, &old) != 0)
        printf("restore: %s\n", ename(errno));
    printf("restored\n");
    return 0;
}
