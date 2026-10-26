/* IN-4: evdev for programs. Without arguments (the ABI diff, both sides) only the ABI's sizes and ioctl numbers
 * from the Linux headers: a CachyOS test must never open /dev/input (Georg's devices). With "v2" (the ktest
 * in.evdev.program, V2 only) it opens the event node of the MCP keyboard, checks the ioctls and reads the
 * events the test injects; the ktest compares the transcript with user/abi-tests/expected/in4-evdev.txt,
 * written from linux-6.6/drivers/input/evdev.c. */
#include "abi.h"
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <sys/ioctl.h>

static void sizes(void) {
    printf("input_event %zu input_id %zu input_absinfo %zu input_mask %zu\n", sizeof(struct input_event),
           sizeof(struct input_id), sizeof(struct input_absinfo), sizeof(struct input_mask));
    printf("EVIOCGVERSION %#lx EVIOCGID %#lx EVIOCGNAME(256) %#lx EVIOCGBIT(EV_KEY,96) %#lx\n", (unsigned long)EVIOCGVERSION,
           (unsigned long)EVIOCGID, (unsigned long)EVIOCGNAME(256), (unsigned long)EVIOCGBIT(EV_KEY, 96));
    printf("EVIOCGABS(ABS_X) %#lx EVIOCGRAB %#lx EVIOCSCLOCKID %#lx EVIOCGKEY(96) %#lx EVIOCREVOKE %#lx\n",
           (unsigned long)EVIOCGABS(ABS_X), (unsigned long)EVIOCGRAB, (unsigned long)EVIOCSCLOCKID,
           (unsigned long)EVIOCGKEY(96), (unsigned long)EVIOCREVOKE);
    printf("EV_VERSION %#x KEY_MAX %#x EV_MAX %#x\n", EV_VERSION, KEY_MAX, EV_MAX);
}

static int find(const char *want) {
    for (int i = 0; i < 64; i++) {
        char path[32], name[256] = { 0 };
        snprintf(path, sizeof path, "/dev/input/event%d", i);
        int fd = open(path, O_RDONLY | O_NONBLOCK);
        if (fd < 0)
            continue;
        if (ioctl(fd, EVIOCGNAME(sizeof name - 1), name) >= 0 && strcmp(name, want) == 0)
            return fd;
        close(fd);
    }
    return -1;
}

static void bytes(const char *label, long r, const unsigned char *b, int n) {
    show(label, r);
    printf("  bytes:");
    for (int i = 0; i < n; i++)
        printf(" %02x", b[i]);
    printf("\n");
}

int main(int argc, char **argv) {
    sizes();
    if (argc < 2 || strcmp(argv[1], "v2") != 0)
        return 0;
    int fd = find("BitsOS MCP keyboard");
    if (fd < 0) {
        printf("no MCP keyboard node\n");
        return 1;
    }
    int v = 0;
    SYS("EVIOCGVERSION", SYS_ioctl, fd, EVIOCGVERSION, &v);
    printf("  version %#x\n", v);
    struct input_id id;
    SYS("EVIOCGID", SYS_ioctl, fd, EVIOCGID, &id);
    printf("  bus %#x vendor %#x product %#x version %#x\n", id.bustype, id.vendor, id.product, id.version);
    char s[256];
    memset(s, 0, sizeof s);
    SYS("EVIOCGNAME(256)", SYS_ioctl, fd, EVIOCGNAME(256), s);
    printf("  name %s\n", s);
    memset(s, 0, sizeof s);
    SYS("EVIOCGNAME(6)", SYS_ioctl, fd, EVIOCGNAME(6), s);
    SYS("EVIOCGPHYS(64)", SYS_ioctl, fd, EVIOCGPHYS(64), s);
    printf("  phys %s\n", s);
    SYS("EVIOCGUNIQ(64)", SYS_ioctl, fd, EVIOCGUNIQ(64), s);
    unsigned char b[128];
    memset(b, 0, sizeof b);
    bytes("EVIOCGBIT(0,32)", ioctl(fd, EVIOCGBIT(0, 32), b), b, 4);
    memset(b, 0, sizeof b);
    bytes("EVIOCGBIT(EV_KEY,128)", ioctl(fd, EVIOCGBIT(EV_KEY, 128), b), b, 4);
    memset(b, 0, sizeof b);
    bytes("EVIOCGBIT(EV_LED,8)", ioctl(fd, EVIOCGBIT(EV_LED, 8), b), b, 2);
    SYS("EVIOCGBIT(EV_REL,8)", SYS_ioctl, fd, EVIOCGBIT(EV_REL, 8), b);
    SYS("EVIOCGBIT(EV_REP,8)", SYS_ioctl, fd, EVIOCGBIT(EV_REP, 8), b);
    SYS("EVIOCGKEY(128)", SYS_ioctl, fd, EVIOCGKEY(128), b);
    SYS("EVIOCGPROP(8)", SYS_ioctl, fd, EVIOCGPROP(8), b);
    unsigned rep[2] = { 0, 0 };
    SYS("EVIOCGREP", SYS_ioctl, fd, EVIOCGREP, rep);
    printf("  delay %u period %u\n", rep[0], rep[1]);
    struct input_absinfo a;
    SYS("EVIOCGABS(ABS_X)", SYS_ioctl, fd, EVIOCGABS(ABS_X), &a);
    SYS("EVIOCGNAME bad pointer", SYS_ioctl, fd, EVIOCGNAME(16), KERNEL_ADDR);
    SYS("unknown 'E' command", SYS_ioctl, fd, _IOR('E', 0x7f, int), &v);
    SYS("terminal command", SYS_ioctl, fd, 0x5401, b);
    int clk = 99;
    SYS("EVIOCSCLOCKID(99)", SYS_ioctl, fd, EVIOCSCLOCKID, &clk);
    clk = CLOCK_MONOTONIC;
    SYS("EVIOCSCLOCKID(MONOTONIC)", SYS_ioctl, fd, EVIOCSCLOCKID, &clk);
    char small[10];
    SYS("read 10 bytes", SYS_read, fd, small, 10);
    struct input_event e[16];
    SYS("read nothing queued", SYS_read, fd, e, sizeof e);
    /* Grab: one client only; the other one cannot grab or ungrab. */
    int fd2 = find("BitsOS MCP keyboard");
    SYS("EVIOCGRAB 1", SYS_ioctl, fd, EVIOCGRAB, 1);
    SYS("EVIOCGRAB 1 (second client)", SYS_ioctl, fd2, EVIOCGRAB, 1);
    SYS("EVIOCGRAB 0 (second client)", SYS_ioctl, fd2, EVIOCGRAB, 0);
    printf("ready\n");
    fflush(stdout);
    /* The test injects KEY_Q down, SYN, KEY_Q up, SYN. */
    struct pollfd p = { fd, POLLIN, 0 };
    int got = 0, rounds = 0;
    uint64_t now = ns_of(CLOCK_MONOTONIC);
    int times_ok = 1;
    while (got < 4 && rounds++ < 100) {
        p.revents = 0;
        int pr = poll(&p, 1, 100);
        if (pr <= 0)
            continue;
        long n = read(fd, e + got, sizeof e - got * sizeof e[0]);
        if (n < 0) {
            printf("read: %s\n", ename(errno));
            break;
        }
        got += n / (long)sizeof e[0];
    }
    printf("events %d\n", got);
    for (int i = 0; i < got && i < 16; i++) {
        uint64_t t = (uint64_t)e[i].input_event_sec * 1000000000u + (uint64_t)e[i].input_event_usec * 1000u;
        if (t + 5000000000u < now || t > ns_of(CLOCK_MONOTONIC) + 1000000)
            times_ok = 0;
        printf("  type %d code %d value %d\n", e[i].type, e[i].code, e[i].value);
    }
    yes("timestamps on CLOCK_MONOTONIC", times_ok);
    SYS("read nonblock, empty", SYS_read, fd2, e, sizeof e);
    SYS("EVIOCGRAB 0", SYS_ioctl, fd, EVIOCGRAB, 0);
    SYS("EVIOCGRAB 0 again", SYS_ioctl, fd, EVIOCGRAB, 0);
    SYS("EVIOCREVOKE", SYS_ioctl, fd2, EVIOCREVOKE, 0);
    SYS("read after revoke", SYS_read, fd2, e, sizeof e);
    close(fd2);
    close(fd);
    printf("done\n");
    return 0;
}
