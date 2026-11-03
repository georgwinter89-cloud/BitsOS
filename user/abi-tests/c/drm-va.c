/* DR-7: the GPU VM of an amdgpu render-node fd as libdrm and Mesa meet it, without any GPU work: GEM_VA's checks
 * (amdgpu_gem.c:664: the user range DEV_INFO reports, the canonical hole, flags, operations, the handle) and
 * operations (amdgpu_vm.c: MAP with its alignment, size and overlap rules, UNMAP of a whole mapping, CLEAR that
 * splits mappings, REPLACE, PRT ranges without memory, DELAY_UPDATE), the 40-byte Linux 6.6 form, the bo_va
 * going with GEM_CLOSE, one VM per fd, and AMDGPU_VM's VMID reservation. The VA bounds come from DEV_INFO like
 * Mesa takes them. Takes the first amdgpu render node. Prints results and errno names only: no handles, addresses
 * or chip facts. */
#include "abi.h"
#include <dirent.h>
#include <fcntl.h>
#include <sys/ioctl.h>

#define IOC(dir, ty, nr, size) ((unsigned long)(dir) << 30 | (unsigned long)(size) << 16 | (unsigned long)(ty) << 8 | (nr))
#define D(dir, nr, size) IOC(dir, 0x64u, nr, size)

struct drm_version { int major, minor, patch, pad; unsigned long name_len; char *name; unsigned long date_len; char *date; unsigned long desc_len; char *desc; };
struct gem_create { uint64_t size, align, domains, flags; };
struct gem_close { uint32_t handle, pad; };
struct info { uint64_t ptr; uint32_t size, query; uint32_t u[4]; };
struct gem_va {
    uint32_t handle, pad, op, flags;
    uint64_t va, offset, size;
    uint64_t tl_point;
    uint32_t tl_out, n_in;
    uint64_t in;
};
struct vm_in { uint32_t op, flags; };
struct syncobj_create { uint32_t handle, flags; };

#define VERSION D(3u, 0x00, sizeof(struct drm_version))
#define GEM_CLOSE D(1u, 0x09, 8)
#define SYNCOBJ_CREATE D(3u, 0xbf, 8)
#define GEM_CREATE D(3u, 0x40, 32)
#define INFO D(1u, 0x45, 32)
#define GEM_VA D(1u, 0x48, 64)
#define GEM_VA_40 D(1u, 0x48, 40)
#define GEM_VA_48 D(1u, 0x48, 48)
#define GEM_VA_56 D(1u, 0x48, 56)
#define VM D(3u, 0x53, 8)

#define DEV_INFO 0x16u
#define GTT 2u
#define VRAM 4u
#define MAP 1u
#define UNMAP 2u
#define CLEAR 3u
#define REPLACE 4u
#define DELAY 1u
#define R 2u
#define W 4u
#define X 8u
#define PRT 0x10u
#define MTYPE_UC 0x80u
#define CLOEXEC 0x80000u
#define P 4096ull

static int fd = -1, fd2 = -1;
static uint64_t bottom, top, high, high_max;

static long io(int f, unsigned long cmd, void *arg) {
    return ioctl(f, cmd, arg) == -1 ? -errno : 0;
}

static void res(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, ename((int)-r));
    else
        printf("%s = %ld\n", label, r);
}

static int pick_node(void) {
    DIR *d = opendir("/dev/dri");
    if (!d)
        return 0;
    char best[300] = "";
    struct dirent *e;
    while ((e = readdir(d))) {
        if (strncmp(e->d_name, "renderD", 7))
            continue;
        char p[300];
        snprintf(p, sizeof p, "/dev/dri/%s", e->d_name);
        int f = open(p, O_RDWR | O_CLOEXEC);
        if (f < 0)
            continue;
        char name[16] = "";
        struct drm_version v = {.name_len = sizeof name - 1, .name = name};
        if (ioctl(f, VERSION, &v) == 0 && !strcmp(name, "amdgpu") && (!best[0] || strcmp(p, best) < 0))
            strcpy(best, p);
        close(f);
    }
    closedir(d);
    if (!best[0])
        return 0;
    fd = open(best, O_RDWR | O_CLOEXEC);
    fd2 = open(best, O_RDWR | O_CLOEXEC);
    return fd >= 0 && fd2 >= 0;
}

static uint32_t create(int f, uint64_t size, uint64_t domains) {
    union { struct gem_create in; uint32_t out[8]; } a = {.in = {size, 0, domains, 0}};
    return io(f, GEM_CREATE, &a) == 0 ? a.out[0] : 0;
}

static long va_on(int f, uint32_t h, uint32_t op, uint32_t flags, uint64_t va, uint64_t off, uint64_t size) {
    struct gem_va a = {.handle = h, .op = op, .flags = flags, .va = va, .offset = off, .size = size};
    return io(f, GEM_VA, &a);
}

static long va(uint32_t h, uint32_t op, uint32_t flags, uint64_t addr, uint64_t off, uint64_t size) {
    return va_on(fd, h, op, flags, addr, off, size);
}

static long vm(int f, uint32_t op, uint32_t flags) {
    struct vm_in a = {op, flags};
    return io(f, VM, &a);
}

int main(void) {
    if (!pick_node()) {
        puts("amdgpu render node: none");
        return 0;
    }
    puts("amdgpu render node: found");
    unsigned char dev[448];
    memset(dev, 0, sizeof dev);
    struct info q = {.ptr = (uintptr_t)dev, .size = sizeof dev, .query = DEV_INFO};
    res("INFO DEV_INFO", io(fd, INFO, &q));
    memcpy(&bottom, dev + 144, 8);
    memcpy(&top, dev + 152, 8);
    memcpy(&high, dev + 336, 8);
    memcpy(&high_max, dev + 344, 8);
    printf("  bottom page aligned and below 4 GiB: %s\n", bottom % P == 0 && bottom < (1ull << 32) ? "yes" : "no");
    printf("  high half from the hole's end: %s\n", high == 0xffff800000000000ull ? "yes" : "no");

    uint32_t a = create(fd, 8 * P, GTT), b = create(fd, 2 * P, VRAM);
    printf("objects: %s\n", a && b ? "yes" : "no");
    const uint64_t V = 1ull << 32;

    puts("-- checks");
    res("MAP below the bottom", va(a, MAP, R | W, bottom - P, 0, P));
    res("MAP at 0", va(a, MAP, R | W, 0, 0, P));
    res("MAP in the hole", va(a, MAP, R | W, 0x0000800000000000ull, 0, P));
    res("MAP last hole page", va(a, MAP, R | W, 0xffff7ffffffff000ull, 0, P));
    res("MAP at the high top", va(a, MAP, R | W, high_max, 0, P));
    res("MAP across the high top", va(a, MAP, R | W, high_max - P, 0, 2 * P));
    res("MAP overflowing size", va(a, MAP, R | W, V, 0, ~0ull - P));
    res("MAP bad range, unknown handle", va(0x7777, MAP, R | W, 0, 0, P));
    res("MAP unknown flag 1<<20", va(a, MAP, 1u << 20, V, 0, P));
    res("MAP PRT|READ", va(a, MAP, PRT | R, V, 0, P));
    res("op 0", va(a, 0, R, V, 0, P));
    res("op 5", va(a, 5, R, V, 0, P));
    res("MAP unknown handle", va(0x7777, MAP, R | W, V, 0, P));
    res("UNMAP unknown handle", va(0x7777, UNMAP, 0, V, 0, P));
    res("MAP unaligned va", va(a, MAP, R | W, V + 8, 0, P));
    res("MAP unaligned offset", va(a, MAP, R | W, V, 8, P));
    res("MAP unaligned size", va(a, MAP, R | W, V, 0, P + 8));
    res("MAP size 0", va(a, MAP, R | W, V, 0, 0));
    res("MAP past the object", va(a, MAP, R | W, V, P, 8 * P));
    res("MAP huge offset", va(a, MAP, R | W, V, ~0xfffull, P));
    res("MAP PRT huge offset", va(0, MAP, PRT, V, ~0xfffull, P));
    res("GEM_VA without memory", io(fd, GEM_VA, NULL));
    res("GEM_VA 40-byte without memory", io(fd, GEM_VA_40, NULL));
    res("VM without memory", io(fd, VM, NULL));

    puts("-- map and unmap");
    res("MAP at the bottom", va(a, MAP, R | W, bottom, 0, P));
    res("UNMAP at the bottom", va(a, UNMAP, 0, bottom, 0, P));
    /* Linux 6.6 refused everything below 2 MiB; Mesa uses what DEV_INFO reports. */
    res("MAP at 64 KiB", va(a, MAP, R | W, 0x10000, 0, P));
    res("UNMAP at 64 KiB", va(a, UNMAP, 0, 0x10000, 0, P));
    res("MAP just below 2 MiB", va(a, MAP, R | W, 0x1ff000, 0, P));
    res("UNMAP just below 2 MiB", va(a, UNMAP, 0, 0x1ff000, 0, P));
    res("MAP last page below the high top", va(a, MAP, R | W, high_max - P, 0, P));
    res("UNMAP it", va(a, UNMAP, 0, high_max - P, 0, P));
    res("MAP 8 pages", va(a, MAP, R | W, V, 0, 8 * P));
    res("MAP overlapping the end", va(a, MAP, R | W, V + 7 * P, 0, P));
    res("MAP overlapping the start", va(a, MAP, R | W, V - P, 0, 2 * P));
    res("MAP part of the object elsewhere", va(a, MAP, R | X | MTYPE_UC, V + 64 * P, 2 * P, 2 * P));
    res("MAP in the high half", va(a, MAP, R | W, 0xffff800000000000ull, 0, P));
    res("UNMAP high half", va(a, UNMAP, 0, 0xffff800000000000ull, 0, P));
    res("UNMAP not at a start", va(a, UNMAP, 0, V + P, 0, P));
    res("UNMAP by another object", va(b, UNMAP, 0, V, 0, 8 * P));
    res("second fd: UNMAP", va_on(fd2, 0, UNMAP, PRT, V, 0, 8 * P));

    puts("-- clear and replace");
    res("CLEAR pages 2-3", va(0, CLEAR, 0, V + 2 * P, 0, 2 * P));
    res("UNMAP the cleared part", va(a, UNMAP, 0, V + 2 * P, 0, P));
    res("MAP into the hole it left", va(b, MAP, R | W, V + 2 * P, 0, 2 * P));
    res("UNMAP that", va(b, UNMAP, 0, V + 2 * P, 0, 2 * P));
    res("UNMAP the piece after", va(a, UNMAP, 0, V + 4 * P, 0, 4 * P));
    res("UNMAP the piece before", va(a, UNMAP, 0, V, 0, 2 * P));
    res("UNMAP it again", va(a, UNMAP, 0, V, 0, 2 * P));
    res("CLEAR over nothing", va(0, CLEAR, 0, V + 128 * P, 0, 4 * P));
    res("MAP 4 pages again", va(a, MAP, R | W, V, 0, 4 * P));
    res("REPLACE the middle", va(b, REPLACE, R | W, V + P, 0, 2 * P));
    res("UNMAP the replaced start by a", va(a, UNMAP, 0, V + P, 0, P));
    res("UNMAP it by b", va(b, UNMAP, 0, V + P, 0, 2 * P));
    res("UNMAP a's last piece", va(a, UNMAP, 0, V + 3 * P, 0, P));
    res("UNMAP a's first piece", va(a, UNMAP, 0, V, 0, P));
    res("REPLACE past the object", va(b, REPLACE, R | W, V, P, 2 * P));
    res("REPLACE over nothing", va(b, REPLACE, R | W, V, 0, 2 * P));
    res("UNMAP it", va(b, UNMAP, 0, V, 0, 2 * P));
    res("CLEAR unaligned", va(0, CLEAR, 0, V + 8, 0, P));
    res("CLEAR size 0", va(0, CLEAR, 0, V, 0, 0));

    puts("-- PRT and delay");
    const uint64_t S = 1ull << 34;
    res("MAP PRT 1 GiB", va(0, MAP, PRT, S, 0, 1ull << 30));
    res("MAP PRT|DELAY", va(0, MAP, PRT | DELAY, S + (1ull << 30), 0, P));
    res("REPLACE a page of it", va(a, REPLACE, R | W, S + P, 0, P));
    res("UNMAP PRT by an object", va(a, UNMAP, 0, S, 0, P));
    res("UNMAP PRT", va(0, UNMAP, PRT, S, 0, P));
    res("UNMAP PRT after the page", va(0, UNMAP, PRT, S + 2 * P, 0, P));
    res("UNMAP the page", va(a, UNMAP, 0, S + P, 0, P));
    res("UNMAP PRT|DELAY", va(0, UNMAP, PRT | DELAY, S + (1ull << 30), 0, P));
    res("MAP DELAY_UPDATE", va(a, MAP, R | W | DELAY, V, 0, P));
    res("UNMAP DELAY_UPDATE", va(a, UNMAP, DELAY, V, 0, P));

    puts("-- forms");
    struct gem_va f40[2];
    memset(f40, 0xff, sizeof f40);
    f40[0] = (struct gem_va){.handle = a, .op = MAP, .flags = R | W, .va = V, .size = P};
    memset((char *)&f40[0] + 40, 0xff, sizeof f40[0] - 40);
    res("MAP 40-byte form", io(fd, GEM_VA_40, &f40[0]));
    struct gem_va t = {.handle = a, .op = UNMAP, .va = V, .size = P, .tl_out = 0x7777};
    long r = io(fd, GEM_VA, &t);
    res("UNMAP with an unknown out-syncobj", r);
    struct syncobj_create sc = {0, 0};
    res("SYNCOBJ_CREATE", io(fd, SYNCOBJ_CREATE, &sc));
    t.tl_out = sc.handle;
    res("UNMAP with an out-syncobj", io(fd, GEM_VA, &t));
    /* Callers between the two forms: what lies past their size is not read. */
    struct gem_va f48[2];
    memset(f48, 0xff, sizeof f48);
    f48[0] = (struct gem_va){.handle = a, .op = MAP, .flags = R | W, .va = V, .size = P, .tl_point = 5, .tl_out = 0x7777, .n_in = 3};
    res("MAP 48-byte caller", io(fd, GEM_VA_48, &f48[0]));
    f48[0].op = UNMAP;
    f48[0].n_in = 0;
    res("UNMAP 56-byte caller, unknown out-syncobj", io(fd, GEM_VA_56, &f48[0]));
    f48[0].tl_out = 0;
    res("UNMAP 56-byte caller", io(fd, GEM_VA_56, &f48[0]));

    puts("-- close");
    res("MAP b", va(b, MAP, R | W, V + 16 * P, 0, 2 * P));
    struct gem_close c = {b, 0};
    res("GEM_CLOSE b", io(fd, GEM_CLOSE, &c));
    res("UNMAP by the closed handle", va(b, UNMAP, 0, V + 16 * P, 0, 2 * P));
    uint32_t n = create(fd, 2 * P, GTT);
    res("MAP a new object where b was", va(n, MAP, R | W, V + 16 * P, 0, 2 * P));
    uint32_t o = create(fd2, P, GTT);
    res("second fd: same address", va_on(fd2, o, MAP, R | W, V + 16 * P, 0, P));

    puts("-- VM");
    res("VM flags 1", vm(fd, 1, 1));
    res("VM op 0", vm(fd, 0, 0));
    res("VM op 3", vm(fd, 3, 0));
    res("UNRESERVE without reserve", vm(fd, 2, 0));
    res("RESERVE_VMID", vm(fd, 1, 0));
    res("RESERVE_VMID again", vm(fd, 1, 0));
    res("second fd: RESERVE_VMID", vm(fd2, 1, 0));
    res("UNRESERVE_VMID", vm(fd, 2, 0));
    res("UNRESERVE_VMID again", vm(fd, 2, 0));
    res("second fd: RESERVE_VMID now", vm(fd2, 1, 0));
    res("first fd: UNRESERVE without holding it", vm(fd, 2, 0));
    res("first fd: RESERVE_VMID", vm(fd, 1, 0));
    close(fd2);
    res("first fd: after the holder closed", vm(fd, 1, 0));
    res("UNRESERVE_VMID", vm(fd, 2, 0));
    close(fd);
    return 0;
}
