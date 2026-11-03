/* DR-9: AMDGPU_BO_LIST (create, update, destroy and their checks in Linux's order), FENCE_TO_HANDLE (syncobj,
 * syncobj fd, sync_file fd, and its errors) and AMDGPU_INFO_GPUVM_FAULT on an amdgpu render-node fd, without any
 * GPU work: no CS here passes its checks (the only CS names a list handle together with a BO_HANDLES chunk, which
 * Linux refuses before it looks the list up), and FENCE_TO_HANDLE only asks for numbers that need no job (0 and
 * "the last" of a context without jobs: a signaled stub), plus GEM_USERPTR's refusals that need no object
 * (Linux's order of checks; a call that makes an object would differ, V2 has no page source yet). Takes the
 * first amdgpu render node. Prints results,
 * errno names and yes/no only. No case makes the kernel allocate more than a page or so. */
#include "abi.h"
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>

#define IOC(dir, ty, nr, size) ((unsigned long)(dir) << 30 | (unsigned long)(size) << 16 | (unsigned long)(ty) << 8 | (nr))
#define D(dir, nr, size) IOC(dir, 0x64u, nr, size)

struct drm_version { int major, minor, patch, pad; unsigned long name_len; char *name; unsigned long date_len; char *date; unsigned long desc_len; char *desc; };
struct gem_create { uint64_t size, align, domains, flags; };
struct ctx_in { uint32_t op, flags, ctx_id; int32_t priority; };
struct list_in { uint32_t op, handle, number, entry_size; uint64_t ptr; };
struct list_entry { uint32_t handle, priority; };
struct cs_in { uint32_t ctx_id, bo_list_handle, num_chunks, flags; uint64_t chunks; };
struct chunk { uint32_t id, length_dw; uint64_t data; };
struct chunk_ib { uint32_t pad, flags; uint64_t va; uint32_t bytes, ip, inst, ring; };
struct fence { uint32_t ctx, ip, inst, ring; uint64_t seq; };
struct fence_to_handle_in { struct fence fence; uint32_t what, pad; };
struct info_req { uint64_t ptr; uint32_t size, query; uint32_t a[4]; };
struct gem_userptr { uint64_t addr, size; uint32_t flags, handle; };
struct syncobj_wait { uint64_t handles; int64_t timeout; uint32_t count, flags, first, pad; uint64_t deadline; };

#define VERSION D(3u, 0x00, sizeof(struct drm_version))
#define SYNCOBJ_DESTROY D(3u, 0xc0, 8)
#define SYNCOBJ_WAIT D(3u, 0xc3, 40)
#define GEM_CREATE D(3u, 0x40, 32)
#define CTX D(3u, 0x42, 16)
#define BO_LIST D(3u, 0x43, 24)
#define CS D(3u, 0x44, 24)
#define INFO D(1u, 0x45, 32)
#define FENCE_TO_HANDLE D(3u, 0x54, 32)
#define GEM_USERPTR D(3u, 0x51, 24)
#define GEM_CLOSE D(1u, 0x09, 8)

#define GTT 2u
#define CREATE 0u
#define DESTROY 1u
#define UPDATE 2u
#define GET_SYNCOBJ 0u
#define GET_SYNCOBJ_FD 1u
#define GET_SYNC_FILE_FD 2u
#define GPUVM_FAULT 0x23u

static int fd = -1;
static uint32_t ctx, bo, bo2;

static long io(unsigned long cmd, void *arg) {
    return ioctl(fd, cmd, arg) == -1 ? -errno : 0;
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
        char nm[16] = "";
        struct drm_version v = {.name_len = sizeof nm - 1, .name = nm};
        if (ioctl(f, VERSION, &v) == 0 && !strcmp(nm, "amdgpu") && (!best[0] || strcmp(p, best) < 0))
            strcpy(best, p);
        close(f);
    }
    closedir(d);
    if (!best[0])
        return 0;
    fd = open(best, O_RDWR | O_CLOEXEC);
    return fd >= 0;
}

static uint32_t create(uint64_t size) {
    union { struct gem_create in; uint32_t out[8]; } a = {.in = {size, 0, GTT, 0}};
    return io(GEM_CREATE, &a) == 0 ? a.out[0] : 0;
}

static struct list_entry entries[4];

/* One BO_LIST call: prints the result and the handle the kernel answered; checks the rest of the union is 0. */
static void list(const char *label, uint32_t op, uint32_t handle, uint32_t n, uint32_t size, void *ptr) {
    union { struct list_in in; uint32_t out[6]; } a = {.in = {op, handle, n, size, (uintptr_t)ptr}};
    long r = io(BO_LIST, &a);
    if (r < 0) {
        res(label, r);
        return;
    }
    printf("%s = 0, handle %u, rest zero %s\n", label, a.out[0], !a.out[1] && !a.out[2] && !a.out[3] && !a.out[4] && !a.out[5] ? "yes" : "no");
}

static long to_handle(uint32_t c, uint32_t ip, uint32_t ring, uint64_t seq, uint32_t what, uint32_t *out, int *rest_same) {
    union fth { struct fence_to_handle_in in; uint32_t w[8]; } a = {.in = {{c, ip, 0, ring, seq}, what, 0}};
    union fth before = a;
    long r = io(FENCE_TO_HANDLE, &a);
    if (out)
        *out = a.w[0];
    if (rest_same)
        *rest_same = !memcmp(&a.w[1], &before.w[1], 28);
    return r;
}

static int cloexec(int f) {
    int fl = fcntl(f, F_GETFD);
    return fl >= 0 && (fl & FD_CLOEXEC);
}

int main(void) {
    if (!pick_node()) {
        puts("amdgpu render node: none");
        return 0;
    }
    puts("amdgpu render node: found");
    union { struct ctx_in in; uint32_t out[4]; } ca = {.in = {1, 0, 0, 0}};
    res("CTX ALLOC", io(CTX, &ca));
    ctx = ca.out[0];
    bo = create(4096);
    bo2 = create(4096);
    printf("objects: %s\n", bo && bo2 ? "yes" : "no");

    puts("-- BO_LIST");
    entries[0] = (struct list_entry){bo, 3};
    entries[1] = (struct list_entry){bo2, 99};
    list("CREATE two", CREATE, 0, 2, 8, entries);
    list("CREATE empty", CREATE, 0, 0, 8, entries);
    list("CREATE empty, NULL", CREATE, 0, 0, 8, NULL);
    entries[2] = (struct list_entry){bo + bo2 + 100, 0};
    list("CREATE unknown object", CREATE, 0, 3, 8, entries);
    list("CREATE entry size 12", CREATE, 0, 1, 12, entries);
    list("CREATE entry size 4", CREATE, 0, 1, 4, entries);
    list("CREATE entry size 0, none", CREATE, 0, 0, 0, entries);
    list("CREATE NULL entries", CREATE, 0, 1, 8, NULL);
    list("CREATE kernel entries", CREATE, 0, 1, 8, KERNEL_ADDR);
    list("DESTROY 3, entry size 12", DESTROY, 3, 1, 12, entries);
    list("DESTROY 3 again, NULL entries", DESTROY, 3, 1, 8, NULL);
    list("operation 3", 3, 1, 0, 8, entries);
    list("operation 99, NULL entries", 99, 1, 1, 8, NULL);
    list("UPDATE 1", UPDATE, 1, 1, 8, &entries[1]);
    list("UPDATE unknown", UPDATE, 77, 1, 8, entries);
    list("UPDATE 0", UPDATE, 0, 0, 8, entries);
    list("UPDATE 1 unknown object", UPDATE, 1, 1, 8, &entries[2]);
    list("UPDATE 1 entry size 12", UPDATE, 1, 1, 12, entries);
    list("UPDATE 1 NULL entries", UPDATE, 1, 1, 8, NULL);
    list("UPDATE unknown, NULL entries", UPDATE, 77, 1, 8, NULL);
    list("DESTROY 1", DESTROY, 1, 0, 8, entries);
    list("DESTROY 1 again", DESTROY, 1, 0, 8, entries);
    list("DESTROY unknown", DESTROY, 1234, 0, 8, entries);
    list("CREATE after destroy", CREATE, 0, 1, 8, entries);
    {
        /* A list handle and a BO_HANDLES chunk: EINVAL before the list is looked up; no job. */
        static struct chunk_ib ib = {0, 0, 1ull << 32, 16, 0, 0, 0};
        static struct list_in li = {0, 0, 1, 8, 0};
        li.ptr = (uintptr_t)entries;
        static struct chunk ch[2];
        ch[0] = (struct chunk){1, sizeof ib / 4, (uintptr_t)&ib};
        ch[1] = (struct chunk){6, sizeof li / 4, (uintptr_t)&li};
        uint64_t ptrs[2] = {(uintptr_t)&ch[0], (uintptr_t)&ch[1]};
        struct cs_in a = {ctx, 1, 2, 0, (uintptr_t)ptrs};
        res("CS list handle and BO_HANDLES", io(CS, &a));
    }

    puts("-- FENCE_TO_HANDLE");
    uint32_t h = 0;
    int same = 0;
    res("unknown context", to_handle(ctx + 100, 0, 0, 0, GET_SYNCOBJ, NULL, NULL));
    res("unknown context, what 9", to_handle(ctx + 100, 0, 0, 0, 9, NULL, NULL));
    res("not handed out", to_handle(ctx, 0, 0, 1, GET_SYNCOBJ, NULL, NULL));
    res("IP 99", to_handle(ctx, 99, 0, 0, GET_SYNCOBJ, NULL, NULL));
    res("compute ring 4", to_handle(ctx, 1, 4, 0, GET_SYNCOBJ, NULL, NULL));
    res("what 3", to_handle(ctx, 0, 0, 0, 3, NULL, NULL));
    res("seq 0, syncobj", to_handle(ctx, 0, 0, 0, GET_SYNCOBJ, &h, &same));
    printf("  handle nonzero %s, rest of the union as sent %s\n", h ? "yes" : "no", same ? "yes" : "no");
    {
        struct syncobj_wait w = {(uintptr_t)&h, 0, 1, 0, 0, 0, 0};
        res("  SYNCOBJ_WAIT on it", io(SYNCOBJ_WAIT, &w));
        uint32_t d[2] = {h, 0};
        res("  SYNCOBJ_DESTROY", io(SYNCOBJ_DESTROY, d));
    }
    res("last, syncobj", to_handle(ctx, 0, 0, ~0ull, GET_SYNCOBJ, &h, NULL));
    {
        uint32_t d[2] = {h, 0};
        io(SYNCOBJ_DESTROY, d);
    }
    res("seq 0, syncobj fd", to_handle(ctx, 0, 0, 0, GET_SYNCOBJ_FD, &h, &same));
    printf("  fd valid %s, cloexec %s, rest as sent %s\n", fcntl((int)h, F_GETFD) >= 0 ? "yes" : "no", cloexec((int)h) ? "yes" : "no", same ? "yes" : "no");
    close((int)h);
    res("seq 0, sync_file fd", to_handle(ctx, 2, 0, 0, GET_SYNC_FILE_FD, &h, &same));
    {
        struct pollfd p = {(int)h, POLLIN, 0};
        int n = poll(&p, 1, 0);
        printf("  cloexec %s, signaled %s, rest as sent %s\n", cloexec((int)h) ? "yes" : "no", n == 1 && (p.revents & POLLIN) ? "yes" : "no", same ? "yes" : "no");
    }
    close((int)h);

    puts("-- INFO GPUVM_FAULT");
    {
        uint8_t buf[24];
        memset(buf, 0xee, sizeof buf);
        struct info_req q = {(uintptr_t)buf, 16, GPUVM_FAULT, {0}};
        res("fresh fd", io(INFO, &q));
        int zero = 1;
        for (int i = 0; i < 16; i++)
            zero &= buf[i] == 0;
        printf("  16 bytes zero %s, byte 16 untouched %s\n", zero ? "yes" : "no", buf[16] == 0xee ? "yes" : "no");
        memset(buf, 0xee, sizeof buf);
        q.size = 8;
        res("8 bytes", io(INFO, &q));
        printf("  addr zero %s, status untouched %s\n", !buf[0] && !buf[7] ? "yes" : "no", buf[8] == 0xee ? "yes" : "no");
        q.size = 64;
        res("64 bytes", io(INFO, &q));
        q.ptr = 8;
        res("bad pointer", io(INFO, &q));
        q.ptr = 0;
        res("NULL pointer", io(INFO, &q));
    }
    puts("-- GEM_USERPTR refusals");
    {
        static uint8_t page[2 * 4096] __attribute__((aligned(4096)));
        uint64_t at = (uintptr_t)page;
        struct { const char *label; uint64_t addr, size; uint32_t flags; } u[] = {
            {"unaligned address, size 0", at + 1, 0, 9},
            {"unknown flag, size 0", at, 0, 0x19},
            {"flag bit 31", at, 4096, 0x80000009u},
            {"ANONONLY and VALIDATE only", at, 4096, 6},
            {"no flags, size 0", at, 0, 0},
            {"READONLY, past the mmap offset space", 0x10000, 2ull << 40, 1},
            {"READONLY, kernel address", 0xffff888000000000ull, 4096, 1},
            {"no flags, kernel address", 0xffff888000000000ull, 4096, 0},
            {"unknown flag, kernel address", 0xffff888000000000ull, 4096, 0x11},
            {"unaligned kernel address", 0xffff888000000001ull, 4096, 1},
            {"READONLY, kernel address, size 0", 0xffff888000000000ull, 0, 1},
            {"READONLY, range wraps", 0x7ffffffff000ull, 0xffff800000001000ull, 1},
            {"READONLY, ends at the user top + 4K", 0x7fffffffe000ull, 8192, 1},
            {"READONLY at 0, size 0", 0, 0, 1},
        };
        for (unsigned i = 0; i < sizeof u / sizeof u[0]; i++) {
            struct gem_userptr a = {u[i].addr, u[i].size, u[i].flags, 0};
            long r = io(GEM_USERPTR, &a);
            res(u[i].label, r);
            if (r == 0) {
                uint32_t c[2] = {a.handle, 0};
                io(GEM_CLOSE, c);
            }
        }
        void *gone = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        munmap(gone, 4096);
        res("unmapped argument", io(GEM_USERPTR, gone));
        res("kernel argument", io(GEM_USERPTR, KERNEL_ADDR));
    }

    union { struct ctx_in in; uint32_t out[4]; } cf = {.in = {2, 0, ctx, 0}};
    res("CTX FREE", io(CTX, &cf));
    close(fd);
    return 0;
}
