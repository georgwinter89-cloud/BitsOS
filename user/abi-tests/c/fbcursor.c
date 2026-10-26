/* GX-5a CR 4: FBIO_BITSOS_CURSOR, BitsOS's private hardware-cursor ioctl on /dev/fb0. Modes:
 *   (none)  the ioctl's number and its answer on a file that is no framebuffer (/dev/null: ENOTTY on both
 *           kernels); never opens /dev/fb0 (on CachyOS that is Georg's screen, and Linux has no such ioctl)
 *   v2      V2 only (ktest dp.fbdev.cursor.program): the errno of each refusal in order, on one line; without a
 *           hardware cursor (QEMU) "EFAULT EINVAL EINVAL ENODEV ENODEV ENOTTY" */
#include "abi.h"
#include <fcntl.h>
#include <sys/ioctl.h>

struct bitsos_fb_cursor {
    uint32_t op, flags;
    int32_t x, y;
    uint32_t width, height, hot_x, hot_y;
    uint64_t pixels;
};

#define FBIO_BITSOS_CURSOR _IOW('F', 0xB2, struct bitsos_fb_cursor)
#define WRONG_SIZE _IOW('F', 0xB2, char[36])

static uint32_t image[64 * 64];

static const char *answer(int fd, unsigned long cmd, void *arg) {
    return ioctl(fd, cmd, arg) == 0 ? "0" : ename(errno);
}

static void facts(void) {
    printf("FBIO_BITSOS_CURSOR = %#lx, struct %zu bytes\n", (unsigned long)FBIO_BITSOS_CURSOR,
           sizeof(struct bitsos_fb_cursor));
    int fd = open("/dev/null", O_RDWR | O_CLOEXEC);
    struct bitsos_fb_cursor a = {0};
    printf("on /dev/null: %s\n", fd < 0 ? ename(errno) : answer(fd, FBIO_BITSOS_CURSOR, &a));
    if (fd >= 0) close(fd);
}

static void v2(void) {
    int fd = open("/dev/fb0", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        printf("open /dev/fb0: %s\n", ename(errno));
        return;
    }
    for (int i = 0; i < 64 * 64; i++) image[i] = 0xff000000u | (uint32_t)i;
    struct bitsos_fb_cursor a = {.op = 0, .flags = 0, .x = 10, .y = 10, .width = 64, .height = 64,
                                 .pixels = (uint64_t)(uintptr_t)image};
    const char *r[6];
    r[0] = answer(fd, FBIO_BITSOS_CURSOR, KERNEL_ADDR);
    a.op = 3;
    r[1] = answer(fd, FBIO_BITSOS_CURSOR, &a);
    a.op = 0, a.flags = 2;
    r[2] = answer(fd, FBIO_BITSOS_CURSOR, &a);
    a.flags = 0;
    r[3] = answer(fd, FBIO_BITSOS_CURSOR, &a);
    a.op = 1;
    r[4] = answer(fd, FBIO_BITSOS_CURSOR, &a);
    r[5] = answer(fd, WRONG_SIZE, &a);
    printf("%s %s %s %s %s %s\n", r[0], r[1], r[2], r[3], r[4], r[5]);
    close(fd);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        facts();
        return 0;
    }
    if (!strcmp(argv[1], "v2")) {
        v2();
        return 0;
    }
    fprintf(stderr, "usage: abi-fbcursor [v2]\n");
    return 2;
}
