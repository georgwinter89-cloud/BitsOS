/* GX-8b: real work on an amdgpu render node through raw ioctls (no libdrm): GEM create, VA map, IBs on the SDMA, GFX and
 * compute rings, WAIT_CS, the results read back by the CPU, two fds in two VMs that do not see each other, a fault on one
 * that does not hurt the other. The same binary on V2's fake engines in QEMU and on the 780M.
 *
 * It runs only with the word `run` or `hang`: abi-diff runs every program on CachyOS too, and an IB that faults on the
 * compute ring would stop a queue of the machine's real GPU. Without the word it says so and leaves. `hang` submits a GFX
 * job that never ends: the device is lost afterwards. Prints verdicts only: no numbers of the machine. */
#include "abi.h"
#include <dirent.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>

#define IOC(dir, ty, nr, size) ((unsigned long)(dir) << 30 | (unsigned long)(size) << 16 | (unsigned long)(ty) << 8 | (nr))
#define D(dir, nr, size) IOC(dir, 0x64u, nr, size)

struct drm_version { int major, minor, patch, pad; unsigned long name_len; char *name; unsigned long date_len; char *date; unsigned long desc_len; char *desc; };
struct gem_create { uint64_t size, align, domains, flags; };
struct gem_va { uint32_t handle, pad, op, flags; uint64_t va, offset, size; };
struct ctx_in { uint32_t op, flags, ctx_id; int32_t priority; };
struct cs_in { uint32_t ctx_id, bo_list_handle, num_chunks, flags; uint64_t chunks; };
struct chunk { uint32_t id, length_dw; uint64_t data; };
struct chunk_ib { uint32_t pad, flags; uint64_t va; uint32_t bytes, ip, inst, ring; };
struct chunk_dep { uint32_t ip, inst, ring, ctx; uint64_t handle; };
struct chunk_fence { uint32_t handle, offset; };
struct list_in { uint32_t op, handle, number, entry_size; uint64_t ptr; };
struct list_entry { uint32_t handle, priority; };
struct wait_cs_in { uint64_t handle, timeout; uint32_t ip, inst, ring, ctx; };
struct info_req { uint64_t ptr; uint32_t size, query; uint32_t u[4]; };
struct vm_fault { uint64_t addr; uint32_t status, vmhub; };

#define VERSION D(3u, 0x00, sizeof(struct drm_version))
#define GEM_CREATE D(3u, 0x40, 32)
#define GEM_MMAP D(3u, 0x41, 8)
#define CTX D(3u, 0x42, 16)
#define CS D(3u, 0x44, 24)
#define INFO D(1u, 0x45, 32)
#define GEM_VA D(1u, 0x48, 40)
#define WAIT_CS D(3u, 0x49, 32)

#define GPUVM_FAULT 0x23u
#define GTT 2u
#define VRAM 4u
#define MAP 1u
#define R 2u
#define W 4u
#define X 8u
#define GFX 0u
#define COMPUTE 1u
#define DMA 2u
#define C_IB 1u
#define C_FENCE 2u
#define C_DEPS 3u
#define C_LIST 6u
#define P 4096ull
#define VA_CODE 0x400000000ull
#define VA_DATA (VA_CODE + 0x200000ull)
#define VA_UF (VA_CODE + 0x400000ull)
#define VA_VRAM (VA_CODE + 0x600000ull)
#define VA_HOLE (VA_CODE + 0x100000ull)

/* PM4 and SDMA words the program writes itself: this is what Mesa's command streams come down to. */
#define PKT3(op, n) (3u << 30 | (uint32_t)(n) << 16 | (uint32_t)(op) << 8)
#define PM4_PAD 0xffff1000u
#define OP_NOP 0x10u
#define OP_WRITE_DATA 0x37u
#define OP_RELEASE_MEM 0x49u
#define WR_CONFIRM (1u << 20)
#define DST_MEM (5u << 8)
#define RM_FLUSH 0x0671f514u

/* One fd's world: its context, objects and their CPU views. */
struct world {
    int fd;
    uint32_t ctx, code, data, uf, vram;
    uint32_t *code_p, *data_p;
    uint64_t *uf_p;
    uint32_t next_ib;
};

static const char *name(int e) {
    return e == ETIME ? "ETIME" : e == ECANCELED ? "ECANCELED" : ename(e);
}

static long io(int f, unsigned long cmd, void *arg) {
    return ioctl(f, cmd, arg) == -1 ? -errno : 0;
}

static void verdict(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, name((int)-r));
    else
        printf("%s = %ld\n", label, r);
}

static int open_node(void) {
    DIR *d = opendir("/dev/dri");
    if (!d)
        return -1;
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
    return best[0] ? open(best, O_RDWR | O_CLOEXEC) : -1;
}

static uint32_t bo(int f, uint64_t size, uint32_t domain) {
    union { struct gem_create in; uint32_t out[8]; } a = {.in = {size, P, domain, 0}};
    return io(f, GEM_CREATE, &a) == 0 ? a.out[0] : 0;
}

static void *cpu(int f, uint32_t h, uint64_t size) {
    union { uint32_t in; uint64_t out; } a = {.in = h};
    if (io(f, GEM_MMAP, &a) < 0)
        return NULL;
    void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, f, (off_t)a.out);
    return p == MAP_FAILED ? NULL : p;
}

static long map(int f, uint32_t h, uint64_t va, uint64_t size, uint32_t flags) {
    struct gem_va a = {.handle = h, .op = MAP, .flags = flags, .va = va, .size = size};
    return io(f, GEM_VA, &a);
}

static uint32_t new_ctx(int f) {
    union { struct ctx_in in; uint64_t out[2]; } a = {.in = {1, 0, 0, 0}};
    return io(f, CTX, &a) == 0 ? (uint32_t)a.out[0] : 0;
}

static int world_up(struct world *w, int f) {
    memset(w, 0, sizeof *w);
    w->fd = f;
    w->ctx = new_ctx(f);
    w->code = bo(f, 64 * P, GTT);
    w->data = bo(f, P, GTT);
    w->uf = bo(f, P, GTT);
    w->vram = bo(f, P, VRAM);
    if (!w->ctx || !w->code || !w->data || !w->uf || !w->vram)
        return 0;
    w->code_p = cpu(f, w->code, 64 * P);
    w->data_p = cpu(f, w->data, P);
    w->uf_p = cpu(f, w->uf, P);
    if (!w->code_p || !w->data_p || !w->uf_p)
        return 0;
    return !map(f, w->code, VA_CODE, 64 * P, R | W | X) && !map(f, w->data, VA_DATA, P, R | W) && !map(f, w->uf, VA_UF, P, R | W) && !map(f, w->vram, VA_VRAM, P, R | W);
}

/* The words into the code object (on 32 bytes, as SDMA wants); their VA. */
static uint64_t put_ib(struct world *w, const uint32_t *code, uint32_t n) {
    uint32_t at = w->next_ib;
    w->next_ib = (at + ((n * 4 + 31) & ~31u)) % (64 * P - 4096);
    memcpy((char *)w->code_p + at, code, n * 4);
    return VA_CODE + at;
}

static uint32_t pad_pm4(uint32_t *c, uint32_t n) {
    while (n % 8)
        c[n++] = PM4_PAD;
    return n;
}

/* NOP with a body, WRITE_DATA of `value` at `va`, RELEASE_MEM of `rel` at `va2`: the brief's GFX and compute job. */
static uint32_t pm4_job(uint32_t *c, uint64_t va, uint32_t value, uint64_t va2, uint32_t rel) {
    uint32_t n = 0;
    c[n++] = PKT3(OP_NOP, 0);
    c[n++] = 0;
    c[n++] = PKT3(OP_WRITE_DATA, 3);
    c[n++] = DST_MEM | WR_CONFIRM;
    c[n++] = (uint32_t)va;
    c[n++] = (uint32_t)(va >> 32);
    c[n++] = value;
    if (va2) {
        c[n++] = PKT3(OP_RELEASE_MEM, 6);
        c[n++] = RM_FLUSH;
        c[n++] = 1u << 29;
        c[n++] = (uint32_t)va2;
        c[n++] = (uint32_t)(va2 >> 32);
        c[n++] = rel;
        c[n++] = 0;
        c[n++] = 0;
    }
    return pad_pm4(c, n);
}

static uint32_t sdma_write(uint32_t *c, uint64_t va, uint32_t value) {
    uint32_t n = 0;
    c[n++] = 2u; /* WRITE, LINEAR */
    c[n++] = (uint32_t)va;
    c[n++] = (uint32_t)(va >> 32);
    c[n++] = 0;
    c[n++] = value;
    while (n % 8)
        c[n++] = 0;
    return n;
}

static uint32_t sdma_copy(uint32_t *c, uint64_t src, uint64_t dst, uint32_t bytes) {
    uint32_t n = 0;
    c[n++] = 1u; /* COPY, LINEAR */
    c[n++] = bytes - 1;
    c[n++] = 0;
    c[n++] = (uint32_t)src;
    c[n++] = (uint32_t)(src >> 32);
    c[n++] = (uint32_t)dst;
    c[n++] = (uint32_t)(dst >> 32);
    while (n % 8)
        c[n++] = 0;
    return n;
}

/* One IB, the BO list, the user fence, an optional dependency on an earlier job. Returns the sequence number or -errno. */
static long long submit(struct world *w, uint32_t ip, const uint32_t *code, uint32_t n, uint64_t dep) {
    uint64_t va = put_ib(w, code, n);
    struct chunk_ib ib = {0, 0, va, n * 4, ip, 0, 0};
    struct chunk_fence fe = {w->uf, 0};
    struct list_entry es[4] = {{w->code, 0}, {w->data, 0}, {w->uf, 0}, {w->vram, 0}};
    struct list_in li = {0, 0, 4, sizeof es[0], (uintptr_t)es};
    struct chunk_dep dd = {ip, 0, 0, w->ctx, dep};
    struct chunk ch[4] = {
        {C_IB, sizeof ib / 4, (uintptr_t)&ib},
        {C_FENCE, sizeof fe / 4, (uintptr_t)&fe},
        {C_LIST, sizeof li / 4, (uintptr_t)&li},
        {C_DEPS, sizeof dd / 4, (uintptr_t)&dd},
    };
    uint64_t ptrs[4] = {(uintptr_t)&ch[0], (uintptr_t)&ch[1], (uintptr_t)&ch[2], (uintptr_t)&ch[3]};
    union { struct cs_in in; uint64_t out; } a = {.in = {w->ctx, 0, dep ? 4 : 3, 0, (uintptr_t)ptrs}};
    long r = io(w->fd, CS, &a);
    return r < 0 ? r : (long long)a.out;
}

/* WAIT_CS with a deadline past every ring's job timeout: 0 ended, 1 still running, -errno. */
static long wait_job(struct world *w, uint32_t ip, long long seq) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    uint64_t end = (uint64_t)(t.tv_sec + 70) * 1000000000ull + (uint64_t)t.tv_nsec;
    union { struct wait_cs_in in; uint64_t out[4]; } a = {.in = {(uint64_t)seq, end, ip, 0, 0, w->ctx}};
    long r = io(w->fd, WAIT_CS, &a);
    return r < 0 ? r : (long)a.out[0];
}

/* A job from submit to its end, with one verdict line. Clears the user fence first, so its check means this job. */
static long run_job(struct world *w, const char *label, uint32_t ip, const uint32_t *code, uint32_t n) {
    w->uf_p[0] = 0;
    long long s = submit(w, ip, code, n, 0);
    if (s < 0) {
        verdict(label, s);
        return s;
    }
    long r = wait_job(w, ip, s);
    verdict(label, r);
    if (r == 0)
        yes("  user fence holds its number", w->uf_p[0] == (uint64_t)s);
    return r;
}

static int fault_seen(int f) {
    struct vm_fault v = {0, 0, 0};
    struct info_req q = {(uintptr_t)&v, sizeof v, GPUVM_FAULT, {0}};
    return io(f, INFO, &q) == 0 && v.status != 0 && v.addr == (VA_HOLE & ~0xfffull);
}

/* A value in the data page by each kind of ring. */
static void writes(struct world *w) {
    uint32_t c[64];
    uint32_t n = sdma_write(c, VA_DATA, 0x11110001);
    run_job(w, "SDMA WRITE_LINEAR", DMA, c, n);
    yes("  the CPU reads it", w->data_p[0] == 0x11110001);

    n = pm4_job(c, VA_DATA + 8, 0x22220002, VA_DATA + 12, 0x22220003);
    run_job(w, "GFX NOP, WRITE_DATA, RELEASE_MEM", GFX, c, n);
    yes("  the CPU reads the write", w->data_p[2] == 0x22220002);
    yes("  the CPU reads the release", w->data_p[3] == 0x22220003);

    n = pm4_job(c, VA_DATA + 16, 0x33330004, VA_DATA + 20, 0x33330005);
    run_job(w, "compute NOP, WRITE_DATA, RELEASE_MEM", COMPUTE, c, n);
    yes("  the CPU reads the write", w->data_p[4] == 0x33330004);
    yes("  the CPU reads the release", w->data_p[5] == 0x33330005);

    /* VRAM, which a program of the fake cannot map: there and back by SDMA. */
    n = sdma_write(c, VA_VRAM + 64, 0x44440006);
    run_job(w, "SDMA into VRAM", DMA, c, n);
    n = sdma_copy(c, VA_VRAM + 64, VA_DATA + 24, 4);
    run_job(w, "SDMA COPY_LINEAR out of VRAM", DMA, c, n);
    yes("  the CPU reads VRAM's value", w->data_p[6] == 0x44440006);

    /* A dependency on the ring's own last job, then an unsignaled number across rings. */
    n = pm4_job(c, VA_DATA + 28, 0x55550007, 0, 0);
    long long a = submit(w, GFX, c, n, 0);
    n = pm4_job(c, VA_DATA + 32, 0x55550008, 0, 0);
    long long b = submit(w, GFX, c, n, (uint64_t)a);
    verdict("GFX job after a job it depends on", b < 0 ? b : wait_job(w, GFX, b));
    yes("  both ran", w->data_p[7] == 0x55550007 && w->data_p[8] == 0x55550008);
}

/* A job that waits for memory that never reads so; its context, a new one, the device. */
static int hang(void) {
    int f = open_node();
    struct world w;
    if (f < 0 || !world_up(&w, f)) {
        puts("amdgpu render node: none");
        return 1;
    }
    uint32_t c[16];
    uint64_t at = VA_DATA + 0x100;
    uint32_t n = 0;
    c[n++] = PKT3(0x3cu, 5);
    c[n++] = 0x13;
    c[n++] = (uint32_t)at;
    c[n++] = (uint32_t)(at >> 32);
    c[n++] = 0x12345678;
    c[n++] = 0xffffffffu;
    c[n++] = 4;
    n = pad_pm4(c, n);
    run_job(&w, "GFX job that never ends", GFX, c, n);
    union { struct ctx_in in; uint64_t out[2]; } q = {.in = {4, 0, w.ctx, 0}};
    verdict("the context's QUERY_STATE2", io(f, CTX, &q));
    printf("  flags: reset %d guilty %d\n", q.out[0] & 1 ? 1 : 0, q.out[0] & 4 ? 1 : 0);
    w.ctx = new_ctx(f);
    n = sdma_write(c, VA_DATA, 1);
    long long s = submit(&w, DMA, c, n, 0);
    verdict("a new context's CS on another ring", s < 0 ? (long)s : 0);
    close(f);
    puts("done");
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "hang"))
        return hang();
    if (argc < 2 || strcmp(argv[1], "run")) {
        puts("abi-drm-jobs: runs GPU work only with the word run");
        return 0;
    }
    int f1 = open_node(), f2 = open_node();
    if (f1 < 0 || f2 < 0) {
        puts("amdgpu render node: none");
        return 1;
    }
    puts("amdgpu render node: found");
    struct world a, b;
    yes("first fd: objects mapped", world_up(&a, f1));
    yes("second fd: objects mapped", world_up(&b, f2));
    puts("-- first fd");
    writes(&a);

    puts("-- two VMs, one VA");
    uint32_t c[64];
    uint32_t n = sdma_write(c, VA_DATA, 0x66660001);
    run_job(&b, "second fd writes the same VA", DMA, c, n);
    yes("  it reads its own", b.data_p[0] == 0x66660001);
    yes("  the first fd's page is unmoved", a.data_p[0] == 0x11110001);

    puts("-- a fault on one fd");
    n = pm4_job(c, VA_HOLE, 0x77770001, 0, 0);
    run_job(&b, "GFX write to an unmapped page", GFX, c, n);
    n = pm4_job(c, VA_DATA + 36, 0x77770002, 0, 0);
    run_job(&b, "the same context's next GFX job", GFX, c, n);
    yes("  it ran", b.data_p[9] == 0x77770002);
    yes("  the fault is the second fd's", fault_seen(f2));
    yes("  the first fd has none", !fault_seen(f1));
    n = pm4_job(c, VA_DATA + 40, 0x77770003, 0, 0);
    run_job(&a, "first fd's GFX job meanwhile", GFX, c, n);
    yes("  it ran", a.data_p[10] == 0x77770003);
    n = sdma_write(c, VA_HOLE, 0x77770004);
    run_job(&b, "SDMA write to an unmapped page", DMA, c, n);
    n = sdma_write(c, VA_DATA + 44, 0x77770005);
    run_job(&b, "the same context's next SDMA job", DMA, c, n);
    yes("  it ran", b.data_p[11] == 0x77770005);

    /* CPC stops at its fault (CONTRACTS 6b): the job ends in an error, the context is guilty, the rest goes on. */
    puts("-- a fault on the compute ring");
    n = pm4_job(c, VA_HOLE, 0x88880001, 0, 0);
    run_job(&b, "compute write to an unmapped page", COMPUTE, c, n);
    {
        union { struct ctx_in in; uint64_t out[2]; } q = {.in = {4, 0, b.ctx, 0}};
        verdict("the context's QUERY_STATE2", io(f2, CTX, &q));
        printf("  flags: reset %d guilty %d\n", q.out[0] & 1 ? 1 : 0, q.out[0] & 4 ? 1 : 0);
    }
    n = pm4_job(c, VA_DATA + 48, 0x88880002, 0, 0);
    {
        long long s = submit(&b, COMPUTE, c, n, 0);
        verdict("the guilty context's next CS", s < 0 ? (long)s : 0);
    }
    n = pm4_job(c, VA_DATA + 52, 0x88880003, 0, 0);
    run_job(&a, "first fd's compute job after it", COMPUTE, c, n);
    yes("  it ran", a.data_p[13] == 0x88880003);
    b.ctx = new_ctx(f2);
    n = pm4_job(c, VA_DATA + 56, 0x88880004, 0, 0);
    run_job(&b, "second fd's new context on the compute ring", COMPUTE, c, n);
    yes("  it ran", b.data_p[14] == 0x88880004);
    close(f1);
    close(f2);
    puts("done");
    return 0;
}
