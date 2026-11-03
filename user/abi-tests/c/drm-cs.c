/* DR-8: AMDGPU_CS, WAIT_CS, WAIT_FENCES and the context queries of an amdgpu render-node fd, without any GPU work:
 * every CS here names a BO list handle no file has, which Linux checks after all chunks (amdgpu_cs.c:833), so a CS
 * whose chunks pass ends there (ENOENT; with a valid BO_HANDLES chunk too: EINVAL) and no job ever exists. The
 * checks in Linux's order: parser_init, pass1 (copy-in, chunk sizes, entities, IB counts, the user fence object,
 * BO lists), pass2 (preemption, dependencies, syncobjs in and out, the shadow chunk); WAIT_CS and WAIT_FENCES on
 * numbers that need no job. Takes the first amdgpu render node. Prints results and errno names only. No case
 * makes the kernel allocate more than a page or so (Linux WARNs above INT_MAX). */
#include "abi.h"
#include <dirent.h>
#include <fcntl.h>
#include <sys/ioctl.h>

#define IOC(dir, ty, nr, size) ((unsigned long)(dir) << 30 | (unsigned long)(size) << 16 | (unsigned long)(ty) << 8 | (nr))
#define D(dir, nr, size) IOC(dir, 0x64u, nr, size)

struct drm_version { int major, minor, patch, pad; unsigned long name_len; char *name; unsigned long date_len; char *date; unsigned long desc_len; char *desc; };
struct gem_create { uint64_t size, align, domains, flags; };
struct syncobj_create { uint32_t handle, flags; };
struct syncobj_array { uint64_t handles; uint32_t count, pad; };
struct ctx_in { uint32_t op, flags, ctx_id; int32_t priority; };
struct cs_in { uint32_t ctx_id, bo_list_handle, num_chunks, flags; uint64_t chunks; };
struct chunk { uint32_t id, length_dw; uint64_t data; };
struct chunk_ib { uint32_t pad, flags; uint64_t va; uint32_t bytes, ip, inst, ring; };
struct chunk_dep { uint32_t ip, inst, ring, ctx; uint64_t handle; };
struct chunk_fence { uint32_t handle, offset; };
struct chunk_syncobj { uint32_t handle, flags; uint64_t point; };
struct chunk_shadow { uint64_t shadow, csa, gds, flags; };
struct list_in { uint32_t op, handle, number, entry_size; uint64_t ptr; };
struct list_entry { uint32_t handle, priority; };
struct wait_cs_in { uint64_t handle, timeout; uint32_t ip, inst, ring, ctx; };
struct fence { uint32_t ctx, ip, inst, ring; uint64_t seq; };
struct wait_fences_in { uint64_t fences; uint32_t count, all; uint64_t timeout; };

#define VERSION D(3u, 0x00, sizeof(struct drm_version))
#define SYNCOBJ_CREATE D(3u, 0xbf, 8)
#define SYNCOBJ_SIGNAL D(3u, 0xc5, 16)
#define GEM_CREATE D(3u, 0x40, 32)
#define CTX D(3u, 0x42, 16)
#define CS D(3u, 0x44, 24)
#define WAIT_CS D(3u, 0x49, 32)
#define WAIT_FENCES D(3u, 0x52, 24)

#define GTT 2u
#define GFX 0u
#define COMPUTE 1u
#define DMA 2u
#define UVD 3u
#define VPE 9u
#define C_IB 1u
#define C_FENCE 2u
#define C_DEPS 3u
#define C_IN 4u
#define C_OUT 5u
#define C_LIST 6u
#define C_SCHED 7u
#define C_TL_WAIT 8u
#define C_TL_SIGNAL 9u
#define C_SHADOW 10u
#define PREEMPT 4u
#define CE 1u
#define WAIT_FOR_SUBMIT 2u
#define NO_LIST 0xdeadu
#define LAST (~0ull)

static int fd = -1;
static uint32_t ctx, bo, bo2;

static const char *name(int e) {
    return e == ETIME ? "ETIME" : e == ECANCELED ? "ECANCELED" : ename(e);
}

static long io(unsigned long cmd, void *arg) {
    return ioctl(fd, cmd, arg) == -1 ? -errno : 0;
}

static void res(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, name((int)-r));
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

static uint32_t syncobj(void) {
    struct syncobj_create s = {0, 0};
    return io(SYNCOBJ_CREATE, &s) == 0 ? s.handle : 0;
}

static long ctx_op(uint32_t op, uint32_t id, int32_t prio, uint64_t *out) {
    union { struct ctx_in in; uint64_t out[2]; } a = {.in = {op, 0, id, prio}};
    long r = io(CTX, &a);
    if (out)
        *out = a.out[0];
    return r;
}

/* Up to 200 chunks of up to 64 bytes each. */
static struct chunk chunks[200];
static uint64_t ptrs[200];
static unsigned char data[16][64];
static unsigned n;

static void add(uint32_t id, const void *d, uint32_t bytes) {
    unsigned k = n < 16 ? n : 15;
    memcpy(data[k], d, bytes);
    chunks[n] = (struct chunk){id, bytes / 4, (uintptr_t)data[k]};
    ptrs[n] = (uintptr_t)&chunks[n];
    n++;
}

static void ib(uint32_t ip, uint32_t ring, uint32_t flags) {
    struct chunk_ib c = {0, flags, 1ull << 32, 16, ip, 0, ring};
    add(C_IB, &c, sizeof c);
}

static struct list_entry entry;

static void list(void) {
    entry = (struct list_entry){bo, 0};
    struct list_in l = {0, 0, 1, sizeof entry, (uintptr_t)&entry};
    add(C_LIST, &l, sizeof l);
}

/* An in-syncobj no file has: pass 2 answers ENOENT, so a case whose pass-1 chunk passed shows ENOENT. */
static void unknown_in(void) {
    static uint32_t h = 4444;
    add(C_IN, &h, 4);
}

static long submit_on(uint32_t c, uint32_t list_handle) {
    struct cs_in a = {c, list_handle, n, 0, (uintptr_t)ptrs};
    long r = io(CS, &a);
    n = 0;
    return r;
}

static long submit(void) {
    return submit_on(ctx, NO_LIST);
}

static long wait_cs(uint32_t c, uint32_t ip, uint32_t inst, uint32_t ring, uint64_t seq) {
    union { struct wait_cs_in in; uint64_t out[4]; } a = {.in = {seq, 0, ip, inst, ring, c}};
    long r = io(WAIT_CS, &a);
    return r < 0 ? r : (long)a.out[0];
}

static void wait_fences(const char *label, struct fence *fs, uint32_t count, int all) {
    union { struct wait_fences_in in; uint32_t out[6]; } a = {.in = {(uintptr_t)fs, count, all, 0}};
    long r = io(WAIT_FENCES, &a);
    if (r < 0)
        res(label, r);
    else
        printf("%s = 0 status %u first %s\n", label, a.out[0], a.out[1] == ~0u ? "none" : a.out[1] == 0 ? "0" : "1");
}

int main(void) {
    if (!pick_node()) {
        puts("amdgpu render node: none");
        return 0;
    }
    puts("amdgpu render node: found");
    res("CTX ALLOC", ctx_op(1, 0, 0, NULL));
    uint64_t out = 0;
    ctx_op(1, 0, -512, &out);
    uint32_t low = (uint32_t)out;
    ctx_op(1, 0, 0, &out);
    ctx = (uint32_t)out;
    res("CTX QUERY_STATE2", ctx_op(4, ctx, 0, &out));
    printf("  reset flags: %llu\n", (unsigned long long)out);
    bo = create(4096);
    bo2 = create(8192);
    printf("objects: %s\n", bo && bo2 ? "yes" : "no");

    puts("-- parser_init");
    res("CS no chunks", submit());
    ib(GFX, 0, 0);
    res("CS unknown context", submit_on(ctx + 100, NO_LIST));
    ib(GFX, 0, 0);
    res("CS valid IB, unknown BO list", submit());

    puts("-- pass1");
    {
        struct cs_in a = {ctx, NO_LIST, 1, 0, 0};
        res("CS chunk array NULL", io(CS, &a));
        uint64_t bad = 8;
        a.chunks = (uintptr_t)&bad;
        res("CS chunk pointer bad", io(CS, &a));
        struct chunk c = {C_IB, 8, 8};
        uint64_t p = (uintptr_t)&c;
        a.chunks = (uintptr_t)&p;
        res("CS chunk data bad", io(CS, &a));
    }
    ib(GFX, 0, 0);
    add(99, data[0], 8);
    res("CS unknown chunk id", submit());
    {
        struct chunk_ib c = {0, 0, 1ull << 32, 16, GFX, 0, 0};
        add(C_IB, &c, 28);
        res("CS IB chunk of 7 dwords", submit());
    }
    ib(99, 0, 0);
    res("CS IP type 99", submit());
    ib(10, 0, 0);
    res("CS IP type 10", submit());
    ib(GFX, 1, 0);
    res("CS GFX ring 1", submit());
    ib(COMPUTE, 4, 0);
    res("CS compute ring 4", submit());
    ib(DMA, 2, 0);
    res("CS DMA ring 2", submit());
    {
        struct chunk_ib c = {0, 0, 1ull << 32, 16, GFX, 1, 0};
        add(C_IB, &c, sizeof c);
        res("CS IP instance 1", submit());
    }
    {
        struct chunk_fence f = {bo, 0};
        add(C_FENCE, &f, sizeof f);
        res("CS no IB", submit());
        ib(GFX, 0, 0);
        add(C_FENCE, &f, 4);
        res("CS fence chunk of 1 dword", submit());
        struct chunk_fence cases[] = {{0, 0}, {bo2, 0}, {bo, 4089}, {bo, 4088}, {bo, 3}};
        const char *labels[] = {"CS fence handle 0", "CS fence object of two pages", "CS fence offset 4089",
                                "CS fence offset 4088", "CS fence offset 3"};
        for (unsigned i = 0; i < 5; i++) {
            ib(GFX, 0, 0);
            add(C_FENCE, &cases[i], sizeof cases[i]);
            res(labels[i], submit());
        }
    }
    {
        ib(GFX, 0, 0);
        struct list_in l = {0, 0, 1, 8, (uintptr_t)&entry};
        add(C_LIST, &l, 20);
        res("CS BO_HANDLES of 5 dwords", submit());
        ib(GFX, 0, 0);
        list();
        res("CS BO_HANDLES and a BO list handle", submit());
        struct list_entry unknown = {12345, 0};
        struct list_entry longer[2] = {{bo, 7}, {0xffff, 0xffff}};
        /* unk: an unknown in-syncobj follows (ENOENT when the list passed). */
        struct { const char *label; uint32_t number, size; void *ptr; int unk; } ls[] = {
            {"CS BO_HANDLES unknown object", 1, 8, &unknown, 0},
            {"CS BO_HANDLES bad pointer", 1, 8, (void *)8, 0},
            {"CS BO_HANDLES entry size 0", 1, 0, &unknown, 1},
            {"CS BO_HANDLES entry size 0, no entries", 0, 0, (void *)8, 1},
            {"CS BO_HANDLES entry size 12", 1, 12, longer, 1},
            {"CS BO_HANDLES entry size 4, bad pointer", 1, 4, (void *)8, 1},
            {"CS BO_HANDLES no entries", 0, 8, longer, 1},
            {"CS BO_HANDLES priority 99", 1, 8, longer, 1},
        };
        longer[0].priority = 99;
        for (unsigned i = 0; i < sizeof ls / sizeof ls[0]; i++) {
            ib(GFX, 0, 0);
            struct list_in li = {0, 0, ls[i].number, ls[i].size, (uintptr_t)ls[i].ptr};
            add(C_LIST, &li, sizeof li);
            if (ls[i].unk)
                unknown_in();
            res(ls[i].label, submit());
        }
        ib(GFX, 0, 0);
        list();
        list();
        unknown_in();
        res("CS two BO_HANDLES chunks", submit());
        struct chunk_fence f = {bo, 0};
        ib(GFX, 0, 0);
        add(C_FENCE, &f, sizeof f);
        add(C_FENCE, &f, sizeof f);
        unknown_in();
        res("CS two fence chunks", submit());
        struct chunk_shadow sh = {0, 0, 0, 0};
        ib(GFX, 0, 0);
        add(C_SHADOW, &sh, 8);
        struct list_in bad = {0, 0, 1, 8, (uintptr_t)&unknown};
        add(C_LIST, &bad, sizeof bad);
        res("CS shadow chunk of 2 dwords, then an unknown object", submit());
    }
    {
        /* IBs per entity (amdgpu_ring_max_ibs) and entities per CS (AMDGPU_CS_GANG_SIZE). */
        struct { const char *label; uint32_t ip, count; } lim[] = {
            {"CS 192 GFX IBs", GFX, 192}, {"CS 193 GFX IBs", GFX, 193}, {"CS 125 compute IBs", COMPUTE, 125},
            {"CS 126 compute IBs", COMPUTE, 126}, {"CS 49 DMA IBs", DMA, 49}, {"CS 50 DMA IBs", DMA, 50},
        };
        for (unsigned i = 0; i < sizeof lim / sizeof lim[0]; i++) {
            for (unsigned k = 0; k < lim[i].count; k++)
                ib(lim[i].ip, 0, 0);
            res(lim[i].label, submit());
        }
        for (uint32_t r = 0; r < 4; r++)
            ib(COMPUTE, r, 0);
        res("CS gang of 4 entities", submit());
        for (uint32_t r = 0; r < 4; r++)
            ib(COMPUTE, r, 0);
        ib(GFX, 0, 0);
        res("CS gang of 5 entities", submit());
    }

    puts("-- pass2");
    ib(GFX, 0, PREEMPT);
    ib(GFX, 0, PREEMPT);
    res("CS two preemptible IBs", submit());
    ib(GFX, 0, PREEMPT);
    unknown_in();
    res("CS one preemptible IB", submit());
    ib(GFX, 0, PREEMPT);
    ib(GFX, 0, PREEMPT | CE);
    res("CS preemptible DE and CE IBs", submit());
    ib(GFX, 0, CE);
    unknown_in();
    res("CS CE IB", submit());
    ib(COMPUTE, 0, CE);
    unknown_in();
    res("CS CE flag on compute", submit());
    {
        struct list_in bad = {0, 0, 1, 8, (uintptr_t)&entry};
        static struct list_entry none = {12345, 0};
        bad.ptr = (uintptr_t)&none;
        ib(GFX, 0, CE);
        add(C_LIST, &bad, sizeof bad);
        res("CS CE IB, then an unknown object (pass order)", submit());
    }
    {
        struct { const char *label; uint32_t id; struct chunk_dep d; int with_list; } deps[] = {
            {"CS dependency on a number not handed out", C_DEPS, {GFX, 0, 0, 0, 1}, 0},
            {"CS dependency on the last of none", C_DEPS, {GFX, 0, 0, 0, LAST}, 1},
            {"CS dependency on 0", C_DEPS, {GFX, 0, 0, 0, 0}, 1},
            {"CS dependency of an unknown context", C_DEPS, {GFX, 0, 0, 999, 0}, 0},
            {"CS dependency on GFX ring 1", C_DEPS, {GFX, 0, 1, 0, 0}, 0},
            {"CS scheduled dependency on the last", C_SCHED, {GFX, 0, 0, 0, LAST}, 1},
            {"CS scheduled dependency not handed out", C_SCHED, {COMPUTE, 0, 2, 0, 3}, 0},
        };
        for (unsigned i = 0; i < sizeof deps / sizeof deps[0]; i++) {
            ib(GFX, 0, 0);
            if (!deps[i].d.ctx)
                deps[i].d.ctx = ctx;
            add(deps[i].id, &deps[i].d, sizeof deps[i].d);
            if (deps[i].with_list)
                list();
            res(deps[i].label, submit());
        }
    }
    uint32_t empty = syncobj(), sig = syncobj();
    {
        struct syncobj_array s = {(uintptr_t)&sig, 1, 0};
        res("SYNCOBJ_SIGNAL", io(SYNCOBJ_SIGNAL, &s));
        struct { const char *label; uint32_t id, h; int with_list; } sems[] = {
            {"CS in-syncobj unknown", C_IN, 4444, 1},
            {"CS in-syncobj without fence", C_IN, empty, 0},
            {"CS in-syncobj signaled", C_IN, sig, 1},
            {"CS out-syncobj unknown", C_OUT, 4444, 0},
            {"CS out-syncobj", C_OUT, empty, 1},
        };
        for (unsigned i = 0; i < sizeof sems / sizeof sems[0]; i++) {
            ib(GFX, 0, 0);
            add(sems[i].id, &sems[i].h, 4);
            if (sems[i].with_list)
                list();
            res(sems[i].label, submit());
        }
        struct { const char *label; uint32_t id; struct chunk_syncobj s; int with_list; } tls[] = {
            {"CS timeline wait unknown", C_TL_WAIT, {4444, 0, 1}, 1},
            {"CS timeline wait point 3 without fence", C_TL_WAIT, {empty, 0, 3}, 0},
            {"CS timeline wait point 0 signaled", C_TL_WAIT, {sig, 0, 0}, 1},
            {"CS timeline wait for submit (5 s)", C_TL_WAIT, {empty, WAIT_FOR_SUBMIT, 1}, 0},
            {"CS timeline signal unknown", C_TL_SIGNAL, {4444, 0, 5}, 0},
            {"CS timeline signal point 5", C_TL_SIGNAL, {empty, 0, 5}, 1},
        };
        for (unsigned i = 0; i < sizeof tls / sizeof tls[0]; i++) {
            ib(GFX, 0, 0);
            add(tls[i].id, &tls[i].s, sizeof tls[i].s);
            if (tls[i].with_list)
                list();
            res(tls[i].label, submit());
        }
        ib(GFX, 0, 0);
        add(C_OUT, &empty, 0);
        add(C_OUT, &empty, 4);
        res("CS two out-syncobj chunks, the first empty", submit());
        ib(GFX, 0, 0);
        add(C_OUT, &empty, 4);
        struct chunk_syncobj t = {empty, 0, 2};
        add(C_TL_SIGNAL, &t, sizeof t);
        res("CS out-syncobj and timeline signal", submit());
    }
    {
        struct chunk_shadow s = {0, 0, 0, 2};
        ib(GFX, 0, 0);
        add(C_SHADOW, &s, sizeof s);
        res("CS shadow flags 2", submit());
        s.flags = 1;
        ib(GFX, 0, 0);
        add(C_SHADOW, &s, sizeof s);
        res("CS shadow INIT_SHADOW", submit());
    }
    {
        ib(GFX, 0, 0);
        struct cs_in a = {ctx, NO_LIST, n, 0xffffffffu, (uintptr_t)ptrs};
        res("CS flags all ones", io(CS, &a));
        n = 0;
    }

    puts("-- WAIT_CS");
    res("WAIT_CS 1 (none handed out)", wait_cs(ctx, GFX, 0, 0, 1));
    res("WAIT_CS the last of none", wait_cs(ctx, GFX, 0, 0, LAST));
    res("WAIT_CS 0", wait_cs(ctx, GFX, 0, 0, 0));
    res("WAIT_CS unknown context", wait_cs(ctx + 100, GFX, 0, 0, LAST));
    res("WAIT_CS IP 99", wait_cs(ctx, 99, 0, 0, LAST));
    res("WAIT_CS IP 10", wait_cs(ctx, 10, 0, 0, LAST));
    res("WAIT_CS GFX ring 1", wait_cs(ctx, GFX, 0, 1, LAST));
    res("WAIT_CS instance 1", wait_cs(ctx, GFX, 1, 0, LAST));
    res("WAIT_CS compute ring 3", wait_cs(ctx, COMPUTE, 0, 3, LAST));
    res("WAIT_CS compute ring 4", wait_cs(ctx, COMPUTE, 0, 4, LAST));
    res("WAIT_CS DMA ring 1", wait_cs(ctx, DMA, 0, 1, LAST));
    res("WAIT_CS DMA ring 2", wait_cs(ctx, DMA, 0, 2, LAST));
    res("WAIT_CS UVD ring 0", wait_cs(ctx, UVD, 0, 0, LAST));
    res("WAIT_CS UVD ring 1", wait_cs(ctx, UVD, 0, 1, LAST));
    res("WAIT_CS VPE ring 0", wait_cs(ctx, VPE, 0, 0, LAST));
    res("WAIT_CS LOW context", wait_cs(low, GFX, 0, 0, LAST));

    puts("-- WAIT_FENCES");
    {
        struct fence fs[2] = {{ctx, GFX, 0, 0, LAST}, {ctx, GFX, 0, 0, 5}};
        wait_fences("WAIT_FENCES none, any", fs, 0, 0);
        wait_fences("WAIT_FENCES none, all", fs, 0, 1);
        wait_fences("WAIT_FENCES bad pointer", (struct fence *)8, 1, 1);
        wait_fences("WAIT_FENCES one, any", fs, 1, 0);
        wait_fences("WAIT_FENCES one, all", fs, 1, 1);
        wait_fences("WAIT_FENCES done then not handed out, any", fs, 2, 0);
        wait_fences("WAIT_FENCES done then not handed out, all", fs, 2, 1);
        struct fence rev[2] = {fs[1], fs[0]};
        wait_fences("WAIT_FENCES not handed out first, any", rev, 2, 0);
        struct fence unk = {ctx + 100, GFX, 0, 0, 0};
        wait_fences("WAIT_FENCES unknown context", &unk, 1, 1);
        struct fence ring = {ctx, COMPUTE, 0, 9, 0};
        wait_fences("WAIT_FENCES compute ring 9", &ring, 1, 0);
    }

    puts("-- CTX");
    res("CTX FREE", ctx_op(2, ctx, 0, NULL));
    res("CTX FREE again", ctx_op(2, ctx, 0, NULL));
    ib(GFX, 0, 0);
    res("CS on the freed context", submit());
    res("WAIT_CS on the freed context", wait_cs(ctx, GFX, 0, 0, LAST));
    close(fd);
    return 0;
}
