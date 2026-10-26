/* DP-3: FlipPace for Linux fbdev (V1: echt-tests/FlipPace.asm, G-2). Measures frame pacing on /dev/fb0 the way
 * fbDOOM-style programs draw: fill the hidden buffer, FBIOPAN_DISPLAY to it, FBIO_WAITFORVSYNC, then stand-in
 * game logic (a busy wait). Rounds of N frames at logic 0/10/20 ms, once with the vsync wait ("vsync": frames
 * snap to whole refresh periods) and once without ("free": V1 G-2's non-waiting present, 20 ms of logic gives
 * about 20 ms frames). On V2 it also reads the kernel's pacing info (FBIO_BITSOS_PACING: exact refresh, its
 * source, vblank sequence and CLOCK_MONOTONIC times, present completions) and checks it; on Linux that ioctl is
 * ENOTTY (EBUSY for every fb ioctl while a compositor is DRM master: run it on a VT without the desktop) and the
 * numbers are measured only.
 * Modes:
 *   (none)                              ABI facts (struct size, ioctl numbers) and both ioctls on files that are
 *                                       no framebuffer, also with bad pointers and wrong numbers (ENOTTY); never
 *                                       opens /dev/fb0 (on CachyOS: Georg's screen)
 *   info [dev]                          the mode's timing fields and the pacing info, nothing drawn (read-only)
 *   run [frames] [logic,ms,...] [dev]   the measurement (default 120 frames, logic 0,10,20, /dev/fb0); with
 *                                       FLIPPACE_MEASURE_ONLY set it ignores the pacing ioctl (Linux's path)
 * Mini-PC (59.95 Hz): vsync logic 0 and 10 ms -> frame ~16,680 us; vsync 20 ms -> ~33,360 us; free 20 ms -> 20-21 ms
 * plus fill and pan. */
#include "abi.h"
#include <fcntl.h>
#include <linux/fb.h>
#include <stddef.h>
#include <sys/ioctl.h>
#include <sys/mman.h>

struct bitsos_fb_pacing {
    uint32_t version;
    uint32_t flags;
    uint32_t refresh_mhz;
    uint32_t source;
    uint64_t period_ns;
    uint64_t vblank_seq;
    uint64_t vblank_ns;
    uint64_t next_vblank_ns;
    uint64_t present_seq;
    uint64_t present_ns;
};
#define FBIO_BITSOS_PACING _IOR('F', 0xB0, struct bitsos_fb_pacing)
/* The same number with another size or direction: not the pacing ioctl (ENOTTY), so the size is part of it. */
#define PACING_SHORT _IOR('F', 0xB0, uint32_t[8])
#define PACING_WRITE _IOW('F', 0xB0, struct bitsos_fb_pacing)
#define PACING_NONE _IO('F', 0xB0)
#define BITSOS_PACING_RATE_KNOWN 1u
#define BITSOS_PACING_VBLANK_ESTIMATED 2u

/* EBUSY: Linux's DRM fbdev while a compositor is master (abi.h names the common ones). */
static const char *errname(int e) {
    return e == EBUSY ? "EBUSY" : ename(e);
}

static const char *source_name(uint32_t s) {
    static const char *n[] = {"default", "config", "edid", "hardware"};
    return s < 4 ? n[s] : "unknown";
}

/* A page the program may read but not write (the answer of an _IOR ioctl cannot land there). */
static void *read_only_page(void) {
    void *p = mmap(0, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p == MAP_FAILED ? KERNEL_ADDR : p;
}

/* The pacing ioctl with pointers it cannot write and numbers that are not it, on `f` named `name`. */
static void hostile_pacing(int f, const char *name) {
    struct bitsos_fb_pacing p;
    char l[96];
    snprintf(l, sizeof l, "%sPACING(kernel address)", name);
    SYS(l, SYS_ioctl, f, FBIO_BITSOS_PACING, KERNEL_ADDR);
    snprintf(l, sizeof l, "%sPACING(unmapped 0x1)", name);
    SYS(l, SYS_ioctl, f, FBIO_BITSOS_PACING, (void *)1);
    snprintf(l, sizeof l, "%sPACING(read-only page)", name);
    SYS(l, SYS_ioctl, f, FBIO_BITSOS_PACING, read_only_page());
    snprintf(l, sizeof l, "%sPACING with size 32", name);
    SYS(l, SYS_ioctl, f, PACING_SHORT, &p);
    snprintf(l, sizeof l, "%sPACING as _IOW", name);
    SYS(l, SYS_ioctl, f, PACING_WRITE, &p);
    snprintf(l, sizeof l, "%sPACING as _IO", name);
    SYS(l, SYS_ioctl, f, PACING_NONE, 0);
}

static void abi_facts(void) {
    printf("sizeof bitsos_fb_pacing = %zu\n", sizeof(struct bitsos_fb_pacing));
    printf("offsets: period_ns %zu vblank_seq %zu present_ns %zu\n", offsetof(struct bitsos_fb_pacing, period_ns),
           offsetof(struct bitsos_fb_pacing, vblank_seq), offsetof(struct bitsos_fb_pacing, present_ns));
    printf("FBIO_BITSOS_PACING = %#lx, FBIO_WAITFORVSYNC = %#lx\n", (unsigned long)FBIO_BITSOS_PACING,
           (unsigned long)FBIO_WAITFORVSYNC);
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-flippace-%d", getpid());
    int files[2] = {open("/dev/null", O_RDWR), open(path, O_RDWR | O_CREAT | O_EXCL, 0600)};
    const char *names[2] = {"/dev/null", "regular file"};
    struct bitsos_fb_pacing p;
    uint32_t crtc = 0;
    for (int i = 0; i < 2; i++) {
        char l[96];
        snprintf(l, sizeof l, "%s PACING", names[i]);
        SYS(l, SYS_ioctl, files[i], FBIO_BITSOS_PACING, &p);
        snprintf(l, sizeof l, "%s ", names[i]);
        hostile_pacing(files[i], l);
        snprintf(l, sizeof l, "%s WAITFORVSYNC", names[i]);
        SYS(l, SYS_ioctl, files[i], FBIO_WAITFORVSYNC, &crtc);
        close(files[i]);
    }
    unlink(path);
}

static int fd = -1;
static struct fb_var_screeninfo var, orig;
static struct fb_fix_screeninfo fix;
static int have_pacing;
static struct bitsos_fb_pacing pace;

static int read_pacing(struct bitsos_fb_pacing *p) {
    return ioctl(fd, FBIO_BITSOS_PACING, p) == 0;
}

static void open_fb(const char *dev, int flags) {
    fd = open(dev, flags | O_CLOEXEC);
    if (fd < 0) {
        printf("open %s = -%s\n", dev, errname(errno));
        exit(1);
    }
    if (ioctl(fd, FBIOGET_VSCREENINFO, &var) || ioctl(fd, FBIOGET_FSCREENINFO, &fix)) {
        int e = errno;
        printf("%s: no framebuffer (%s)\n", dev, errname(e));
        /* Linux's DRM fbdev answers EBUSY to every fb ioctl while a compositor is DRM master. */
        if (e == EBUSY)
            printf("hint: a desktop holds the display; run this on a text VT without it (or stop the compositor)\n");
        exit(1);
    }
    orig = var;
}

/* The rate the mode's timing fields promise (fbset's formula); DRM's emulation and V2 leave them 0. */
static void say_timing(void) {
    uint64_t ht = (uint64_t)var.left_margin + var.xres + var.right_margin + var.hsync_len;
    uint64_t vt = (uint64_t)var.upper_margin + var.yres + var.lower_margin + var.vsync_len;
    printf("mode: %ux%u, virtual %ux%u, %u bpp, line %u bytes, smem %u bytes\n", var.xres, var.yres, var.xres_virtual,
           var.yres_virtual, var.bits_per_pixel, fix.line_length, fix.smem_len);
    if (var.pixclock && ht && vt)
        printf("var timing: pixclock %u ps, %lux%lu total -> %lu mHz\n", var.pixclock, (unsigned long)ht,
               (unsigned long)vt, (unsigned long)(1000000000000000ull / ((uint64_t)var.pixclock * ht * vt)));
    else
        printf("var timing: none (pixclock %u)\n", var.pixclock);
}

static void say_pacing(void) {
    if (getenv("FLIPPACE_MEASURE_ONLY")) {
        printf("pacing: not asked (FLIPPACE_MEASURE_ONLY): the numbers below are measured only\n");
        return;
    }
    have_pacing = read_pacing(&pace);
    if (!have_pacing) {
        printf("pacing: unsupported (%s): the numbers below are measured only\n", errname(errno));
        return;
    }
    printf("pacing: version %u, refresh %u mHz, period %lu ns, source %s, flags%s%s\n", pace.version, pace.refresh_mhz,
           (unsigned long)pace.period_ns, source_name(pace.source),
           pace.flags & BITSOS_PACING_RATE_KNOWN ? " rate-known" : "",
           pace.flags & BITSOS_PACING_VBLANK_ESTIMATED ? " vblank-estimated" : "");
}

static void busy_until(uint64_t t) {
    while (ns_of(CLOCK_MONOTONIC) < t)
        ;
}

static void fill(uint32_t *buf, uint32_t color) {
    uint32_t words = fix.line_length / 4;
    for (uint32_t y = 0; y < var.yres; y++) {
        uint32_t *row = buf + (size_t)y * words;
        for (uint32_t x = 0; x < var.xres; x++)
            row[x] = color;
    }
}

struct stats {
    uint64_t n, sum, min, max;
};
static void add(struct stats *s, uint64_t v) {
    if (!s->n || v < s->min)
        s->min = v;
    if (v > s->max)
        s->max = v;
    s->sum += v;
    s->n++;
}
static unsigned long avg_us(const struct stats *s) {
    return s->n ? (unsigned long)(s->sum / s->n / 1000) : 0;
}

/* Results of the pacing checks over all rounds (V2 only). */
static int next_ok = 1, seq_ok = 1, present_ok = 1, clock_ok = 1, pan_errors, pan_errno;
/* The refresh period measured from back-to-back vsync waits (ns); no_vsync: FBIO_WAITFORVSYNC refused. */
static uint64_t measured_ns;
static int no_vsync;

static int cmp_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}

/* `n` FBIO_WAITFORVSYNC calls in a row: the median gap between two returns is the refresh period (a late wake
 * makes one gap long and the next short; nothing else runs between the waits, so no gap spans two periods unless
 * a wake came a whole period late, which the kernel's sequence shows on V2 as a skipped vblank). */
static void calibrate(int n) {
    uint64_t *t = calloc((size_t)n + 1, sizeof *t);
    if (!t) {
        printf("no memory for %d waits\n", n);
        exit(1);
    }
    uint32_t crtc = 0;
    uint64_t skipped = 0, prev_seq = 0;
    struct bitsos_fb_pacing p;
    for (int i = 0; i <= n; i++) {
        if (ioctl(fd, FBIO_WAITFORVSYNC, &crtc)) {
            printf("measured: FBIO_WAITFORVSYNC = -%s: no vsync rounds\n", errname(errno));
            no_vsync = 1;
            free(t);
            return;
        }
        t[i] = ns_of(CLOCK_MONOTONIC);
        if (have_pacing && read_pacing(&p)) {
            if (i > 0 && p.vblank_seq != prev_seq + 1)
                skipped++;
            prev_seq = p.vblank_seq;
        }
    }
    for (int i = 0; i < n; i++)
        t[i] = t[i + 1] - t[i];
    qsort(t, (size_t)n, sizeof *t, cmp_u64);
    measured_ns = t[n / 2];
    free(t);
    if (!measured_ns) {
        printf("measured: no vsync period (the waits returned at once)\n");
        return;
    }
    printf("measured: vsync period %lu ns = %lu mHz (median of %d back-to-back waits)", (unsigned long)measured_ns,
           (unsigned long)(1000000000000ull / measured_ns), n);
    if (have_pacing)
        printf(", %lu skipped vblanks", (unsigned long)skipped);
    printf("\n");
}

static void round_(uint32_t *map, int frames, unsigned logic_ms, int vsync, int dbl) {
    struct stats fillt = {0}, pan = {0}, wait = {0}, wake = {0};
    uint64_t *took = calloc((size_t)frames, sizeof *took);
    if (!took) {
        printf("no memory for %d frames\n", frames);
        exit(1);
    }
    uint64_t start = 0, prev = 0;
    uint32_t crtc = 0;
    struct bitsos_fb_pacing a, b;
    for (int i = 0; i <= frames; i++) {
        uint64_t t0 = ns_of(CLOCK_MONOTONIC);
        if (i == 0)
            start = t0;
        else
            took[i - 1] = t0 - prev;
        prev = t0;
        if (i == frames)
            break;
        int buf = dbl ? i & 1 : 0;
        fill(map + (size_t)buf * var.yres * (fix.line_length / 4), i & 1 ? 0x405060 : 0x203040);
        uint64_t t1 = ns_of(CLOCK_MONOTONIC);
        add(&fillt, t1 - t0);
        if (have_pacing)
            read_pacing(&a);
        struct fb_var_screeninfo v = var;
        v.xoffset = 0;
        v.yoffset = buf * var.yres;
        if (ioctl(fd, FBIOPAN_DISPLAY, &v)) {
            pan_errors++;
            pan_errno = errno;
        }
        uint64_t t2 = ns_of(CLOCK_MONOTONIC);
        add(&pan, t2 - t1);
        if (have_pacing && read_pacing(&b) && b.present_seq <= a.present_seq)
            present_ok = 0;
        if (vsync) {
            if (have_pacing)
                read_pacing(&a);
            ioctl(fd, FBIO_WAITFORVSYNC, &crtc);
            uint64_t t3 = ns_of(CLOCK_MONOTONIC);
            add(&wait, t3 - t2);
            int got = have_pacing && read_pacing(&b);
            uint64_t t4 = ns_of(CLOCK_MONOTONIC);
            if (got) {
                if (b.vblank_seq <= a.vblank_seq)
                    seq_ok = 0;
                if (b.next_vblank_ns != b.vblank_ns + b.period_ns)
                    next_ok = 0;
                /* Same clock: the last vblank lies before the read ended, the next one after the read began. */
                if (b.vblank_ns > t4 || b.next_vblank_ns <= t3)
                    clock_ok = 0;
                else if (b.vblank_ns <= t3)
                    add(&wake, t3 - b.vblank_ns);
            }
        }
        busy_until(ns_of(CLOCK_MONOTONIC) + (uint64_t)logic_ms * 1000000);
    }
    struct stats frame = {0};
    for (int i = 0; i < frames; i++)
        add(&frame, took[i]);
    /* Late: a frame longer than 1.5 refresh periods (it missed a vblank it could have made at this load). */
    uint64_t period = have_pacing ? pace.period_ns : measured_ns, late = 0;
    for (int i = 0; period && i < frames; i++)
        late += took[i] * 2 > period * 3;
    free(took);
    uint64_t per = frames ? (prev - start) / frames : 0;
    printf("%s logic %u ms: frame %lu us (min %lu, max %lu), late %lu, fill %lu us, pan %lu us (longest %lu)",
           vsync ? "vsync" : "free", logic_ms, (unsigned long)(per / 1000), (unsigned long)(frame.min / 1000),
           (unsigned long)(frame.max / 1000), (unsigned long)late, avg_us(&fillt), avg_us(&pan),
           (unsigned long)(pan.max / 1000));
    if (vsync)
        printf(", wait %lu us", avg_us(&wait));
    if (wake.n)
        printf(", wake +%lu us (max %lu)", avg_us(&wake), (unsigned long)(wake.max / 1000));
    printf("\n");
}

static void run(int frames, const char *logic, const char *dev) {
    open_fb(dev, O_RDWR);
    /* fbDOOM's dance: a double-height virtual screen; single-buffered if refused. */
    if (var.yres_virtual < 2 * var.yres) {
        struct fb_var_screeninfo v = var;
        v.yres_virtual = 2 * var.yres;
        v.activate = FB_ACTIVATE_NOW;
        if (ioctl(fd, FBIOPUT_VSCREENINFO, &v) == 0)
            ioctl(fd, FBIOGET_VSCREENINFO, &var);
    }
    int dbl = var.yres_virtual >= 2 * var.yres && fix.smem_len >= (uint64_t)fix.line_length * 2 * var.yres;
    if (var.bits_per_pixel != 32) {
        printf("%u bpp: only 32-bit modes are measured\n", var.bits_per_pixel);
        exit(1);
    }
    size_t len = (size_t)fix.line_length * var.yres * (dbl ? 2 : 1);
    uint32_t *map = mmap(0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) {
        printf("mmap: %s\n", errname(errno));
        exit(1);
    }
    say_timing();
    say_pacing();
    printf("buffers: %s, %d frames per round\n", dbl ? "double (pan)" : "single", frames);
    calibrate(frames);
    int rounds = 1;
    for (const char *p = logic; *p; p++)
        rounds += *p == ',';
    unsigned *ms = calloc((size_t)rounds, sizeof *ms);
    if (!ms) {
        printf("no memory for %d rounds\n", rounds);
        exit(1);
    }
    char *p = (char *)logic;
    for (int r = 0; r < rounds; r++) {
        ms[r] = (unsigned)strtoul(p, &p, 10);
        if (*p == ',')
            p++;
    }
    for (int vsync = !no_vsync; vsync >= 0; vsync--)
        for (int r = 0; r < rounds; r++)
            round_(map, frames, ms[r], vsync, dbl);
    free(ms);
    if (pan_errors)
        printf("pan errors: %d (%s)\n", pan_errors, errname(pan_errno));
    if (have_pacing) {
        yes("pacing: next vblank one period after the last", next_ok);
        yes("pacing: vblank sequence moved on with each wait", seq_ok);
        yes("pacing: present seen after each pan", present_ok);
        yes("pacing: vblank times on CLOCK_MONOTONIC", clock_ok);
    }
    /* Leave the mode as found (the first buffer shown, the original virtual size). */
    munmap(map, len);
    orig.xoffset = orig.yoffset = 0;
    orig.activate = FB_ACTIVATE_NOW;
    if (ioctl(fd, FBIOPUT_VSCREENINFO, &orig))
        printf("restore: FBIOPUT_VSCREENINFO = -%s (the mode may keep the double height)\n", errname(errno));
    close(fd);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        abi_facts();
        return 0;
    }
    if (!strcmp(argv[1], "info")) {
        open_fb(argc > 2 ? argv[2] : "/dev/fb0", O_RDONLY);
        say_timing();
        say_pacing();
        /* The answer is copied out like any _IOR ioctl's: a pointer it cannot write is EFAULT, another size or
         * direction is no pacing ioctl (all ENOTTY on Linux). */
        hostile_pacing(fd, "");
        return 0;
    }
    if (!strcmp(argv[1], "run")) {
        int frames = argc > 2 ? atoi(argv[2]) : 120;
        if (frames < 2)
            frames = 2;
        run(frames, argc > 3 ? argv[3] : "0,10,20", argc > 4 ? argv[4] : "/dev/fb0");
        return 0;
    }
    fprintf(stderr, "usage: abi-flippace [info [dev] | run [frames] [logic-ms,...] [dev]]\n");
    return 2;
}
