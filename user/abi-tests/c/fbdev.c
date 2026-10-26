/* DP-2: /dev/fb0 (fbdev) the way fbDOOM-style programs use it. Modes:
 *   (none)                       the fbdev ABI facts both kernels must agree on (sizes, offsets, ioctl numbers) and
 *                                the fbdev ioctls on files that are no framebuffer; never opens /dev/fb0 (on CachyOS
 *                                that is Georg's screen)
 *   v2                           every fbdev call on /dev/fb0 with its Linux answer (ktest dp.fbdev.program compares
 *                                the transcript with user/abi-tests/expected/dp2-fbdev.txt); leaves fb0 as it found it
 *   draw <seed> <frames> [hold|crash]
 *                                double-buffered frames (draw the hidden buffer, FBIOPAN_DISPLAY, FBIO_WAITFORVSYNC),
 *                                then a frame stored straight into the visible buffer and a band of rows written with
 *                                pwrite, fsync; prints "drawn a=<seed> b=<seed> band=<row>+<rows> top=<row>"; then
 *                                exits, sleeps until killed (hold) or crashes (crash, a store to address 0)
 *   store <seed> <how>           the shown window (no PUT_V, no pan) filled with pat(seed) by mmap stores, then held:
 *                                how = unmap (munmap at once after the stores), mprotect (stores, fsync, PROT_READ,
 *                                PROT_READ|WRITE, the stores again with the next seed: they must still be tracked),
 *                                loop (the first row stored again every 20 ms), thread (a second thread stores),
 *                                fork (the child stores after the parent ended); prints "drawn a=<seed> b=<seed>
 *                                band=0+0 top=<row> double=<0|1> pid=<pid that drew>", or "<how>: unsupported <errno>"
 *   idle                         opens /dev/fb0, prints "idle pid=<pid>", sleeps until killed (never draws)
 * Pixels are XRGB8888: pat(seed, x, y) below; ktests compute the same picture. */
#include "abi.h"
#include <fcntl.h>
#include <linux/fb.h>
#include <pthread.h>
#include <stddef.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>

static const char *fbname(int e) {
    switch (e) {
    case EFBIG: return "EFBIG";
    case ENOSPC: return "ENOSPC";
    case E2BIG: return "E2BIG";
    case ENXIO: return "ENXIO";
    }
    return ename(e);
}

static long fbshow(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, fbname(errno));
    else
        printf("%s = %ld\n", label, r);
    return r;
}
#define FB(label, ...) fbshow(label, syscall(__VA_ARGS__))

static uint32_t pat(uint32_t seed, uint32_t x, uint32_t y) {
    return ((x * 3 + seed * 7) & 0xff) << 16 | ((y * 5 + seed * 11) & 0xff) << 8 | ((x ^ y ^ seed) & 0xff);
}

static void abi_facts(void) {
    printf("sizeof fb_var_screeninfo = %zu\n", sizeof(struct fb_var_screeninfo));
    printf("sizeof fb_fix_screeninfo = %zu\n", sizeof(struct fb_fix_screeninfo));
    printf("sizeof fb_cmap = %zu\n", sizeof(struct fb_cmap));
    printf("var: red %zu transp %zu nonstd %zu activate %zu pixclock %zu vmode %zu reserved %zu\n",
           offsetof(struct fb_var_screeninfo, red), offsetof(struct fb_var_screeninfo, transp),
           offsetof(struct fb_var_screeninfo, nonstd), offsetof(struct fb_var_screeninfo, activate),
           offsetof(struct fb_var_screeninfo, pixclock), offsetof(struct fb_var_screeninfo, vmode),
           offsetof(struct fb_var_screeninfo, reserved));
    printf("fix: smem_start %zu smem_len %zu visual %zu xpanstep %zu line_length %zu mmio_start %zu accel %zu capabilities %zu\n",
           offsetof(struct fb_fix_screeninfo, smem_start), offsetof(struct fb_fix_screeninfo, smem_len),
           offsetof(struct fb_fix_screeninfo, visual), offsetof(struct fb_fix_screeninfo, xpanstep),
           offsetof(struct fb_fix_screeninfo, line_length), offsetof(struct fb_fix_screeninfo, mmio_start),
           offsetof(struct fb_fix_screeninfo, accel), offsetof(struct fb_fix_screeninfo, capabilities));
    printf("ioctls: %#x %#x %#x %#x %#x %#x %#lx %#x %#x %#x %#lx %#lx\n", FBIOGET_VSCREENINFO, FBIOPUT_VSCREENINFO,
           FBIOGET_FSCREENINFO, FBIOGETCMAP, FBIOPUTCMAP, FBIOPAN_DISPLAY, (unsigned long)FBIO_CURSOR,
           FBIOGET_CON2FBMAP, FBIOPUT_CON2FBMAP, FBIOBLANK, (unsigned long)FBIOGET_VBLANK,
           (unsigned long)FBIO_WAITFORVSYNC);
    printf("FB_ACTIVATE_TEST %d FORCE %d INV_MODE %d KD_TEXT %d VMODE_YWRAP %d\n", FB_ACTIVATE_TEST, FB_ACTIVATE_FORCE,
           FB_ACTIVATE_INV_MODE, FB_ACTIVATE_KD_TEXT, FB_VMODE_YWRAP);
    /* The same calls on files that are no framebuffer: ENOTTY everywhere (the generic ioctl knows none of them). */
    struct fb_var_screeninfo v;
    struct fb_fix_screeninfo f;
    uint32_t crtc = 0;
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-fbdev-%d", getpid());
    int files[2] = {open("/dev/null", O_RDWR), open(path, O_RDWR | O_CREAT | O_EXCL, 0600)};
    const char *names[2] = {"/dev/null", "regular file"};
    for (int i = 0; i < 2; i++) {
        char l[96];
        snprintf(l, sizeof l, "%s GET_V", names[i]);
        FB(l, SYS_ioctl, files[i], FBIOGET_VSCREENINFO, &v);
        snprintf(l, sizeof l, "%s GET_F", names[i]);
        FB(l, SYS_ioctl, files[i], FBIOGET_FSCREENINFO, &f);
        snprintf(l, sizeof l, "%s PAN", names[i]);
        FB(l, SYS_ioctl, files[i], FBIOPAN_DISPLAY, &v);
        snprintf(l, sizeof l, "%s WAITFORVSYNC", names[i]);
        FB(l, SYS_ioctl, files[i], FBIO_WAITFORVSYNC, &crtc);
        close(files[i]);
    }
    unlink(path);
}

static int fd;
static struct fb_var_screeninfo cur;
static struct fb_fix_screeninfo fix;

static long put(const char *label, struct fb_var_screeninfo *v) {
    return FB(label, SYS_ioctl, fd, FBIOPUT_VSCREENINFO, v);
}

static long cmap_call(const char *label, unsigned long cmd, uint32_t start, uint32_t len, uint16_t *r, uint16_t *g,
                      uint16_t *b) {
    struct fb_cmap c = {.start = start, .len = len, .red = r, .green = g, .blue = b, .transp = 0};
    return FB(label, SYS_ioctl, fd, cmd, &c);
}

static void v2(void) {
    fd = open("/dev/fb0", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        printf("open /dev/fb0 = -%s\n", fbname(errno));
        exit(1);
    }
    struct stat st;
    fstat(fd, &st);
    printf("fb0: char %d, major %u minor %u, mode %o\n", S_ISCHR(st.st_mode), major(st.st_rdev), minor(st.st_rdev),
           st.st_mode & 0777);
    /* A known start: another program may have left the second buffer shown or an activate value behind. */
    if (ioctl(fd, FBIOGET_VSCREENINFO, &cur) == 0) {
        cur.xoffset = cur.yoffset = 0;
        cur.activate = FB_ACTIVATE_NOW;
        cur.vmode = 0;
        ioctl(fd, FBIOPUT_VSCREENINFO, &cur);
    }
    FB("GET_V", SYS_ioctl, fd, FBIOGET_VSCREENINFO, &cur);
    printf("bpp %u red %u/%u green %u/%u blue %u/%u transp %u/%u grayscale %u nonstd %u\n", cur.bits_per_pixel,
           cur.red.offset, cur.red.length, cur.green.offset, cur.green.length, cur.blue.offset, cur.blue.length,
           cur.transp.offset, cur.transp.length, cur.grayscale, cur.nonstd);
    yes("xres_virtual == xres", cur.xres_virtual == cur.xres);
    yes("yres_virtual is yres or 2 * yres", cur.yres_virtual == cur.yres || cur.yres_virtual == 2 * cur.yres);
    printf("offsets %u %u, activate %u, pixclock %u, vmode %u\n", cur.xoffset, cur.yoffset, cur.activate, cur.pixclock,
           cur.vmode);
    FB("GET_F", SYS_ioctl, fd, FBIOGET_FSCREENINFO, &fix);
    printf("id \"%.16s\", smem_start %#lx, type %u, visual %u, panstep %u %u, wrapstep %u, accel %u\n", fix.id,
           fix.smem_start, fix.type, fix.visual, fix.xpanstep, fix.ypanstep, fix.ywrapstep, fix.accel);
    yes("line_length == 4 * xres", fix.line_length == 4 * cur.xres);
    yes("smem_len holds the virtual screen", fix.smem_len >= fix.line_length * cur.yres_virtual);
    yes("smem_len is whole pages", fix.smem_len % 4096 == 0);
    int dbl = cur.yres_virtual == 2 * cur.yres;

    FB("GET_V(kernel address)", SYS_ioctl, fd, FBIOGET_VSCREENINFO, KERNEL_ADDR);
    FB("GET_F(kernel address)", SYS_ioctl, fd, FBIOGET_FSCREENINFO, KERNEL_ADDR);
    FB("PUT_V(kernel address)", SYS_ioctl, fd, FBIOPUT_VSCREENINFO, KERNEL_ADDR);
    FB("PAN(kernel address)", SYS_ioctl, fd, FBIOPAN_DISPLAY, KERNEL_ADDR);

    struct fb_var_screeninfo v = cur;
    put("PUT_V(same)", &v);
    v = cur;
    v.xres -= 8;
    put("PUT_V(xres - 8)", &v);
    v = cur;
    v.xres_virtual += 1;
    put("PUT_V(xres_virtual + 1)", &v);
    v = cur;
    v.yres_virtual = cur.yres_virtual + 1;
    put("PUT_V(yres_virtual + 1)", &v);
    v = cur;
    v.bits_per_pixel = 16;
    v.red = (struct fb_bitfield){11, 5, 0};
    v.green = (struct fb_bitfield){5, 6, 0};
    v.blue = (struct fb_bitfield){0, 5, 0};
    put("PUT_V(16 bpp 565)", &v);
    v = cur;
    v.bits_per_pixel = 16;
    memset(&v.red, 0, 4 * sizeof v.red);
    put("PUT_V(16 bpp, format all zero)", &v);
    printf("  bpp %u red %u/%u\n", v.bits_per_pixel, v.red.offset, v.red.length);
    v = cur;
    v.transp = (struct fb_bitfield){24, 8, 0};
    put("PUT_V(ARGB)", &v);
    v = cur;
    v.grayscale = 1;
    put("PUT_V(grayscale)", &v);
    v = cur;
    v.nonstd = 1;
    put("PUT_V(nonstd)", &v);
    v = cur;
    v.pixclock = 1234;
    v.left_margin = 40;
    v.reserved[0] = 5;
    v.activate = FB_ACTIVATE_TEST;
    put("PUT_V(TEST, timings)", &v);
    printf("  pixclock %u left_margin %u reserved0 %u\n", v.pixclock, v.left_margin, v.reserved[0]);
    v = cur;
    v.yres_virtual = cur.yres;
    v.activate = FB_ACTIVATE_TEST;
    put("PUT_V(TEST, yres_virtual = yres)", &v);
    yes("  yres_virtual filled to the buffer", v.yres_virtual == cur.yres_virtual);
    v = cur;
    v.activate = FB_ACTIVATE_INV_MODE;
    put("PUT_V(INV_MODE of the current mode)", &v);
    v.xres = 640;
    put("PUT_V(INV_MODE of another mode)", &v);
    v = cur;
    v.activate = FB_ACTIVATE_KD_TEXT | FB_ACTIVATE_FORCE;
    put("PUT_V(FORCE | KD_TEXT)", &v);
    printf("  activate back %u\n", v.activate);
    v = cur;
    v.xres = 4;
    put("PUT_V(xres 4)", &v);

    v = cur;
    v.yoffset = cur.yres;
    long r = syscall(SYS_ioctl, fd, FBIOPAN_DISPLAY, &v);
    yes("PAN(yoffset = yres) as the buffer allows", dbl ? r == 0 : (r == -1 && errno == EINVAL));
    struct fb_var_screeninfo now;
    syscall(SYS_ioctl, fd, FBIOGET_VSCREENINFO, &now);
    yes("  GET_V shows the offset", now.yoffset == (dbl ? cur.yres : 0));
    v.yoffset = cur.yres_virtual - cur.yres + 1;
    FB("PAN(one row too far)", SYS_ioctl, fd, FBIOPAN_DISPLAY, &v);
    v.yoffset = 0;
    v.xoffset = 1;
    FB("PAN(xoffset 1)", SYS_ioctl, fd, FBIOPAN_DISPLAY, &v);
    v.xoffset = 0;
    v.yoffset = 1;
    v.vmode = FB_VMODE_YWRAP;
    FB("PAN(YWRAP)", SYS_ioctl, fd, FBIOPAN_DISPLAY, &v);
    v.vmode = 0;
    v.yoffset = 0;
    FB("PAN(0)", SYS_ioctl, fd, FBIOPAN_DISPLAY, &v);
    uint32_t crtc = 0;
    FB("WAITFORVSYNC", SYS_ioctl, fd, FBIO_WAITFORVSYNC, &crtc);
    FB("WAITFORVSYNC(kernel address)", SYS_ioctl, fd, FBIO_WAITFORVSYNC, KERNEL_ADDR);
    FB("WAITFORVSYNC(garbage)", SYS_ioctl, fd, FBIO_WAITFORVSYNC, 0xdeadbeeful);

    uint16_t r16[16], g16[16], b16[16], keep[3][16];
    cmap_call("GETCMAP(0, 16)", FBIOGETCMAP, 0, 16, keep[0], keep[1], keep[2]);
    printf("  entry 4 = %04x %04x %04x, entry 9 = %04x %04x %04x\n", keep[0][4], keep[1][4], keep[2][4], keep[0][9],
           keep[1][9], keep[2][9]);
    cmap_call("GETCMAP(256, 1)", FBIOGETCMAP, 256, 1, r16, g16, b16);
    cmap_call("GETCMAP(0, 0)", FBIOGETCMAP, 0, 0, r16, g16, b16);
    cmap_call("GETCMAP(green at a kernel address)", FBIOGETCMAP, 0, 4, r16, KERNEL_ADDR, b16);
    for (int i = 0; i < 16; i++) {
        r16[i] = 0x1000 + i;
        g16[i] = 0x2000 + i;
        b16[i] = 0x3000 + i;
    }
    cmap_call("PUTCMAP(2, 2)", FBIOPUTCMAP, 2, 2, r16, g16, b16);
    uint16_t c[3][4];
    cmap_call("GETCMAP(1, 4)", FBIOGETCMAP, 1, 4, c[0], c[1], c[2]);
    printf("  red %04x %04x %04x %04x\n", c[0][0], c[0][1], c[0][2], c[0][3]);
    cmap_call("PUTCMAP(15, 2)", FBIOPUTCMAP, 15, 2, r16, g16, b16);
    cmap_call("PUTCMAP(0, 0)", FBIOPUTCMAP, 0, 0, r16, g16, b16);
    cmap_call("PUTCMAP(0, 1 << 30)", FBIOPUTCMAP, 0, 1u << 30, r16, g16, b16);
    cmap_call("PUTCMAP(blue at a kernel address)", FBIOPUTCMAP, 0, 2, r16, g16, KERNEL_ADDR);
    cmap_call("PUTCMAP(start 0x7fffffff, 2)", FBIOPUTCMAP, 0x7fffffffu, 2, r16, g16, b16);
    cmap_call("PUTCMAP(start 0xfffffff0, 2)", FBIOPUTCMAP, 0xfffffff0u, 2, r16, g16, b16);
    cmap_call("GETCMAP(start 0xffffffff, 1)", FBIOGETCMAP, 0xffffffffu, 1, r16, g16, b16);
    cmap_call("GETCMAP(255, 0xffffffff)", FBIOGETCMAP, 255, 0xffffffffu, r16, g16, b16);
    FB("GETCMAP(kernel address)", SYS_ioctl, fd, FBIOGETCMAP, KERNEL_ADDR);
    cmap_call("PUTCMAP(restore)", FBIOPUTCMAP, 0, 16, keep[0], keep[1], keep[2]);

    FB("BLANK(1)", SYS_ioctl, fd, FBIOBLANK, 1);
    FB("BLANK(0)", SYS_ioctl, fd, FBIOBLANK, 0);
    FB("BLANK(5)", SYS_ioctl, fd, FBIOBLANK, 5);
    char cursor[104] = {0};
    FB("CURSOR", SYS_ioctl, fd, FBIO_CURSOR, cursor);
    struct fb_con2fbmap m = {.console = 1, .framebuffer = 7};
    FB("GET_CON2FBMAP", SYS_ioctl, fd, FBIOGET_CON2FBMAP, &m);
    printf("  framebuffer %u\n", m.framebuffer);
    struct fb_vblank vb;
    FB("GET_VBLANK", SYS_ioctl, fd, FBIOGET_VBLANK, &vb);
    FB("ioctl 0x46ff", SYS_ioctl, fd, 0x46ff, 0);
    FB("TCGETS", SYS_ioctl, fd, 0x5401, cursor);

    /* read, write, mmap */
    uint32_t row[64], back[64];
    for (int i = 0; i < 64; i++) row[i] = pat(3, i, 0);
    FB("pwrite(row 0)", SYS_pwrite64, fd, row, sizeof row, 0);
    FB("pread(row 0)", SYS_pread64, fd, back, sizeof back, 0);
    yes("  read back the same", memcmp(row, back, sizeof row) == 0);
    FB("pwrite(at smem_len)", SYS_pwrite64, fd, row, 4, (long)fix.smem_len);
    FB("pwrite(past smem_len)", SYS_pwrite64, fd, row, 4, (long)fix.smem_len + 1);
    FB("pwrite(across the end)", SYS_pwrite64, fd, row, 16, (long)fix.smem_len - 8);
    FB("pread(at smem_len)", SYS_pread64, fd, back, 4, (long)fix.smem_len);
    FB("pread(across the end)", SYS_pread64, fd, back, 16, (long)fix.smem_len - 8);
    FB("pread(kernel address)", SYS_pread64, fd, KERNEL_ADDR, 16, 0);
    FB("pwrite(kernel address)", SYS_pwrite64, fd, KERNEL_ADDR, 16, 0);
    FB("pread(offset -4)", SYS_pread64, fd, back, 4, -4l);
    FB("pwrite(offset -4)", SYS_pwrite64, fd, row, 4, -4l);
    FB("pread(offset 1 << 40)", SYS_pread64, fd, back, 4, 1l << 40);
    FB("pwrite(offset 1 << 40)", SYS_pwrite64, fd, row, 4, 1l << 40);
    uint32_t *p = mmap(0, fix.smem_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    yes("mmap(smem_len, shared)", p != MAP_FAILED);
    if (p != MAP_FAILED) {
        yes("  mapping shows what pwrite wrote", memcmp(p, row, sizeof row) == 0);
        p[1] = 0x00abcdef;
        FB("pread(pixel 1)", SYS_pread64, fd, back, 4, 4);
        yes("  pread sees the store", back[0] == 0x00abcdef);
        p[fix.smem_len / 4 - 1] = 0x123456;
        FB("pread(last pixel)", SYS_pread64, fd, back, 4, (long)fix.smem_len - 4);
        yes("  the last page is mapped", back[0] == 0x123456);
        FB("msync", SYS_msync, p, fix.smem_len, MS_SYNC);
        FB("munmap", SYS_munmap, p, fix.smem_len);
    }
    void *q = mmap(0, 4096, PROT_READ, MAP_SHARED, fd, (long)fix.smem_len);
    yes("mmap(past smem_len) is accepted (pages fault later)", q != MAP_FAILED);
    if (q != MAP_FAILED) munmap(q, 4096);
    q = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    yes("mmap(private) maps", q != MAP_FAILED);
    if (q != MAP_FAILED) munmap(q, 4096);
    FB("fsync", SYS_fsync, fd);
    FB("lseek(100, SEEK_END)", SYS_lseek, fd, 100l, SEEK_END);
    FB("lseek(-4, SEEK_END)", SYS_lseek, fd, -4l, SEEK_END);
    FB("lseek(0, SEEK_DATA)", SYS_lseek, fd, 0l, SEEK_DATA);
    FB("lseek(-4, SEEK_DATA)", SYS_lseek, fd, -4l, SEEK_DATA);
    FB("lseek(0, SEEK_HOLE)", SYS_lseek, fd, 0l, SEEK_HOLE);
    FB("lseek(-4, SEEK_HOLE)", SYS_lseek, fd, -4l, SEEK_HOLE);
    FB("lseek(8, SEEK_SET)", SYS_lseek, fd, 8l, SEEK_SET);
    FB("lseek(4, SEEK_CUR)", SYS_lseek, fd, 4l, SEEK_CUR);
    FB("lseek(0, 5)", SYS_lseek, fd, 0l, 5);
    int ro = open("/dev/fb0", O_RDONLY);
    q = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, ro, 0);
    printf("mmap(write, read-only fd) = %s\n", q == MAP_FAILED ? fbname(errno) : "ok");
    FB("write(read-only fd)", SYS_write, ro, row, 4);
    close(ro);
    /* Leave fb0 as it was: the var read at the start (activate included), the first buffer at the top. */
    v = cur;
    v.yoffset = 0;
    put("PUT_V(restore)", &v);
    close(fd);
    printf("done\n");
}

static void fill(uint32_t *buf, uint32_t seed, uint32_t top, uint32_t rows) {
    for (uint32_t y = 0; y < rows; y++)
        for (uint32_t x = 0; x < cur.xres; x++) buf[(size_t)(top + y) * cur.xres + x] = pat(seed, x, y);
}

static void draw(int seed, int frames, const char *then) {
    fd = open("/dev/fb0", O_RDWR | O_CLOEXEC);
    if (fd < 0 || ioctl(fd, FBIOGET_VSCREENINFO, &cur) || ioctl(fd, FBIOGET_FSCREENINFO, &fix)) {
        printf("fb0: %s\n", fbname(errno));
        exit(1);
    }
    /* fbDOOM's dance: ask for a double-height virtual screen; if refused, draw single-buffered. */
    struct fb_var_screeninfo v = cur;
    v.yres_virtual = 2 * cur.yres;
    v.yoffset = 0;
    if (ioctl(fd, FBIOPUT_VSCREENINFO, &v) == 0) cur = v;
    int dbl = cur.yres_virtual >= 2 * cur.yres;
    uint32_t *fb = mmap(0, fix.smem_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (fb == MAP_FAILED) {
        printf("mmap: %s\n", fbname(errno));
        exit(1);
    }
    uint32_t crtc = 0, top = 0;
    for (int f = 0; f < frames; f++) {
        uint32_t back = dbl ? (top == 0 ? cur.yres : 0) : 0;
        fill(fb, seed + f, back, cur.yres);
        v = cur;
        v.yoffset = back;
        if (dbl && ioctl(fd, FBIOPAN_DISPLAY, &v)) {
            printf("pan: %s\n", fbname(errno));
            exit(1);
        }
        top = back;
        ioctl(fd, FBIO_WAITFORVSYNC, &crtc);
    }
    /* Then straight into what is shown (deferred io sees the stores), and a band of rows by pwrite. */
    uint32_t a = seed + frames, b = seed + frames + 1, band = cur.yres / 3, n = cur.yres / 10;
    fill(fb, a, top, cur.yres);
    uint32_t *rows = malloc((size_t)n * fix.line_length);
    for (uint32_t y = 0; y < n; y++)
        for (uint32_t x = 0; x < cur.xres; x++) rows[(size_t)y * cur.xres + x] = pat(b, x, band + y);
    if (pwrite(fd, rows, (size_t)n * fix.line_length, (off_t)(top + band) * fix.line_length) < 0) {
        printf("pwrite: %s\n", fbname(errno));
        exit(1);
    }
    ioctl(fd, FBIO_WAITFORVSYNC, &crtc);
    fsync(fd);
    printf("drawn a=%u b=%u band=%u+%u top=%u double=%d\n", a, b, band, n, top, dbl);
    fflush(stdout);
    if (then && !strcmp(then, "hold"))
        for (;;) sleep(1);
    if (then && !strcmp(then, "crash")) *(volatile int *)0 = 1;
}

/* store <seed> <how>: the cases a compositor or an SDL program meets (see the head of this file). */
static uint32_t *map;
static uint32_t top_row;
static int store_seed;

static void open_shown(void) {
    fd = open("/dev/fb0", O_RDWR | O_CLOEXEC);
    if (fd < 0 || ioctl(fd, FBIOGET_VSCREENINFO, &cur) || ioctl(fd, FBIOGET_FSCREENINFO, &fix)) {
        printf("fb0: %s\n", fbname(errno));
        exit(1);
    }
    top_row = cur.yoffset;
    map = mmap(0, fix.smem_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) {
        printf("mmap: %s\n", fbname(errno));
        exit(1);
    }
}

static void say_drawn(int seed) {
    printf("drawn a=%d b=%d band=0+0 top=%u double=%d pid=%d\n", seed, seed, top_row, cur.yres_virtual >= 2 * cur.yres,
           getpid());
    fflush(stdout);
}

static void *thread_draw(void *arg) {
    (void)arg;
    fill(map, store_seed, top_row, cur.yres);
    return 0;
}

static void hold(void) {
    for (;;) sleep(1);
}

static void store(int seed, const char *how) {
    open_shown();
    store_seed = seed;
    if (!strcmp(how, "unmap")) {
        fill(map, seed, top_row, cur.yres);
        munmap(map, fix.smem_len);
    } else if (!strcmp(how, "mprotect")) {
        fill(map, seed - 1, top_row, cur.yres);
        fsync(fd);
        if (mprotect(map, fix.smem_len, PROT_READ) || mprotect(map, fix.smem_len, PROT_READ | PROT_WRITE)) {
            printf("mprotect: %s\n", fbname(errno));
            exit(1);
        }
        fill(map, seed, top_row, cur.yres);
    } else if (!strcmp(how, "loop")) {
        fill(map, seed, top_row, cur.yres);
        say_drawn(seed);
        for (;;) {
            fill(map, seed, top_row, 1);
            usleep(20000);
        }
    } else if (!strcmp(how, "thread")) {
        pthread_t t;
        int e = pthread_create(&t, 0, thread_draw, 0);
        if (e) {
            printf("thread: unsupported %s\n", fbname(e));
            exit(0);
        }
        pthread_join(t, 0);
    } else if (!strcmp(how, "fork")) {
        fill(map, seed - 1, top_row, cur.yres);
        fsync(fd);
        fflush(stdout);
        pid_t parent = getpid();
        pid_t c = fork();
        if (c < 0) {
            printf("fork: unsupported %s\n", fbname(errno));
            exit(0);
        }
        if (c > 0)
            _exit(0);
        /* The child: the parent has ended (the screen went back to the console); its stores take it again. */
        for (int i = 0; i < 300 && getppid() == parent; i++) usleep(10000);
        usleep(300000);
        fill(map, seed, top_row, cur.yres);
    } else {
        fprintf(stderr, "store: unknown case %s\n", how);
        exit(2);
    }
    say_drawn(seed);
    hold();
}

int main(int argc, char **argv) {
    if (argc < 2) {
        abi_facts();
        return 0;
    }
    if (!strcmp(argv[1], "v2")) {
        v2();
        return 0;
    }
    if (!strcmp(argv[1], "store") && argc >= 4) {
        store(atoi(argv[2]), argv[3]);
        return 0;
    }
    if (!strcmp(argv[1], "idle")) {
        fd = open("/dev/fb0", O_RDWR | O_CLOEXEC);
        printf("idle pid=%d fd=%d\n", getpid(), fd >= 0);
        fflush(stdout);
        hold();
    }
    if (!strcmp(argv[1], "draw") && argc >= 4) {
        draw(atoi(argv[2]), atoi(argv[3]), argc > 4 ? argv[4] : 0);
        return 0;
    }
    fprintf(stderr, "usage: abi-fbdev [v2 | draw <seed> <frames> [hold|crash] | store <seed> <how> | idle]\n");
    return 2;
}
