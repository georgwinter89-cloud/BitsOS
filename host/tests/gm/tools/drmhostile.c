// GM-1c: odd and hostile DRM calls for drmtrace.so to survive, run with the tracer preloaded (20-radv-fake.sh,
// 30-drmtrace-tools.sh): bad pointers and counts the kernel refuses, an ioctl of type 'd' on a file that is no DRM
// node, fork() while another thread is inside the tracer, and a buffer whose CPU mapping lost its front and tail
// before a CS runs an IB in the part still mapped.
//   drmhostile [--node PATH] [--host]
// --host: only calls that make nothing on the GPU (VERSION, GET_CAP, INFO), for a host whose GPU drives a display.
// Without it (the fake GPU in QEMU) also GEM, GPU VA, a context and CS calls; the one CS that is meant to run is a
// single NOP packet. Prints "drmhostile: <name> ok" per check, the facts the caller compares with the trace
// ("drmhostile: devnull fd=<n>", "drmhostile: ib va=0x<hex> data=<hex>"), then "drmhostile: ok <n> checks", exit 0;
// the first failed check ends it with exit 1. A check is about what the program sees: it gets an error, not a signal.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <drm.h>
#include <amdgpu_drm.h>

// An address no program has mapped (the first page stays unmapped on Linux and V2).
#define BAD ((uint64_t)16)
#define IB_VA 0x10000000ull

static int checks;
static void ok(const char *what) { checks++; printf("drmhostile: %s ok\n", what); fflush(stdout); }
static void fail(const char *what, int ret, int e) {
    printf("drmhostile: FAIL %s (ret=%d errno=%d)\n", what, ret, e);
    exit(1);
}
// The call must be refused (any errno, or the one given), not answered.
static void refused(const char *what, int ret, int want) {
    if (ret != -1 || (want && errno != want)) fail(what, ret, errno);
    ok(what);
}

static int fd;
static volatile int hammer_stop;
static void *hammer(void *unused) {
    (void)unused;
    while (!hammer_stop) {
        struct drm_get_cap c = { .capability = DRM_CAP_SYNCOBJ };
        ioctl(fd, DRM_IOCTL_GET_CAP, &c);
    }
    return NULL;
}

// fork() from one thread while another one keeps making DRM calls (so the tracer's lock is often held): each child
// makes one DRM call and ends; a child that hangs in the tracer is killed after 10 s and fails the check.
static void fork_check(void) {
    pthread_t t;
    if (pthread_create(&t, NULL, hammer, NULL)) fail("fork: thread", -1, errno);
    for (int i = 0; i < 32; i++) {
        pid_t p = fork();
        if (p < 0) fail("fork", -1, errno);
        if (p == 0) {
            struct drm_get_cap c = { .capability = DRM_CAP_SYNCOBJ };
            ioctl(fd, DRM_IOCTL_GET_CAP, &c);
            _exit(0);
        }
        int st = 0;
        struct timespec end, now;
        clock_gettime(CLOCK_MONOTONIC, &end);
        end.tv_sec += 10;
        while (waitpid(p, &st, WNOHANG) == 0) {
            clock_gettime(CLOCK_MONOTONIC, &now);
            if (now.tv_sec > end.tv_sec || (now.tv_sec == end.tv_sec && now.tv_nsec > end.tv_nsec)) {
                kill(p, SIGKILL);
                waitpid(p, &st, 0);
                fail("fork: a child hung in its first DRM call", -1, 0);
            }
            usleep(2000);
        }
        if (!WIFEXITED(st) || WEXITSTATUS(st)) fail("fork: a child did not end with exit 0", -1, st);
    }
    hammer_stop = 1;
    pthread_join(t, NULL);
    ok("fork while another thread is in a DRM call (32 children)");
}

static void host_checks(void) {
    errno = 0;
    refused("INFO with an argument pointer that is not mapped", ioctl(fd, DRM_IOCTL_AMDGPU_INFO, (void *)(uintptr_t)BAD), EFAULT);
    struct drm_amdgpu_info in = { .return_pointer = BAD, .return_size = 64, .query = AMDGPU_INFO_DEV_INFO };
    refused("INFO DEV_INFO into a pointer that is not mapped", ioctl(fd, DRM_IOCTL_AMDGPU_INFO, &in), EFAULT);
    int null = open("/dev/null", O_RDWR | O_CLOEXEC);
    if (null < 0) fail("open /dev/null", null, errno);
    printf("drmhostile: devnull fd=%d\n", null);
    // A number where a pointer would be: the tracer must hand it on unread (ENOTTY from /dev/null).
    refused("an ioctl of type 'd' with a number on /dev/null", ioctl(null, DRM_IOCTL_AMDGPU_INFO, (void *)(uintptr_t)0x1234), ENOTTY);
    close(null);
    void *a = mmap(NULL, 2 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (a == MAP_FAILED) fail("mmap anonymous", -1, errno);
    void *b = mremap(a, 2 * 4096, 4 * 4096, MREMAP_MAYMOVE);
    if (b == MAP_FAILED) fail("mremap through the tracer", -1, errno);
    munmap(b, 4 * 4096);
    ok("mremap of a mapping the tracer does not know");
    fork_check();
}

static uint32_t gem_create(uint64_t size) {
    union drm_amdgpu_gem_create g = { .in = { .bo_size = size, .alignment = 4096, .domains = AMDGPU_GEM_DOMAIN_GTT,
                                              .domain_flags = AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED } };
    int r = ioctl(fd, DRM_IOCTL_AMDGPU_GEM_CREATE, &g);
    if (r) fail("GEM_CREATE", r, errno);
    return g.out.handle;
}

static void cs_checks(void) {
    union drm_amdgpu_ctx ctx = { .in = { .op = AMDGPU_CTX_OP_ALLOC_CTX } };
    int r = ioctl(fd, DRM_IOCTL_AMDGPU_CTX, &ctx);
    if (r) fail("CTX alloc", r, errno);
    uint32_t c = ctx.out.alloc.ctx_id;

    union drm_amdgpu_cs cs = { .in = { .ctx_id = c, .num_chunks = 1, .chunks = BAD } };
    refused("CS with a chunk array that is not mapped", ioctl(fd, DRM_IOCTL_AMDGPU_CS, &cs), 0);
    uint64_t bad_chunk = BAD;
    cs.in.chunks = (uint64_t)(uintptr_t)&bad_chunk;
    refused("CS with a chunk pointer that is not mapped", ioctl(fd, DRM_IOCTL_AMDGPU_CS, &cs), 0);
    // 2^30 dwords: 4 GiB, which a 32-bit product would wrap to 0.
    struct drm_amdgpu_cs_chunk huge = { .chunk_id = AMDGPU_CHUNK_ID_IB, .length_dw = 1u << 30, .chunk_data = BAD };
    uint64_t huge_p = (uint64_t)(uintptr_t)&huge;
    cs.in.chunks = (uint64_t)(uintptr_t)&huge_p;
    refused("CS with a chunk of 2^30 dwords that is not mapped", ioctl(fd, DRM_IOCTL_AMDGPU_CS, &cs), 0);
    union drm_amdgpu_bo_list bl = { .in = { .operation = AMDGPU_BO_LIST_OP_CREATE, .bo_number = 0x7fffffff,
                                            .bo_info_size = 8, .bo_info_ptr = BAD } };
    refused("BO_LIST with 2^31 entries that are not mapped", ioctl(fd, DRM_IOCTL_AMDGPU_BO_LIST, &bl), 0);
    union drm_amdgpu_wait_fences wf = { .in = { .fences = BAD, .fence_count = 0x7fffffff, .wait_all = 1 } };
    refused("WAIT_FENCES with 2^31 fences that are not mapped", ioctl(fd, DRM_IOCTL_AMDGPU_WAIT_FENCES, &wf), 0);
    struct drm_syncobj_wait sw = { .handles = BAD, .count_handles = 1u << 30 };
    refused("SYNCOBJ_WAIT with 2^30 handles that are not mapped", ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &sw), 0);

    // Three pages mapped, the first and the last unmapped again; the IB lies in the middle one.
    uint32_t h = gem_create(3 * 4096);
    union drm_amdgpu_gem_mmap m = { .in = { .handle = h } };
    if ((r = ioctl(fd, DRM_IOCTL_AMDGPU_GEM_MMAP, &m))) fail("GEM_MMAP", r, errno);
    char *p = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)m.out.addr_ptr);
    if (p == MAP_FAILED) fail("mmap of the buffer", -1, errno);
    struct drm_amdgpu_gem_va va = { .handle = h, .operation = AMDGPU_VA_OP_MAP, .va_address = IB_VA, .map_size = 3 * 4096,
                                    .flags = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE | AMDGPU_VM_PAGE_EXECUTABLE };
    if ((r = ioctl(fd, DRM_IOCTL_AMDGPU_GEM_VA, &va))) fail("GEM_VA map", r, errno);
    uint32_t ib[16];
    ib[0] = 3u << 30 | 14u << 16 | 0x10u << 8;  // PM4 type-3 NOP with 15 dwords after the header
    for (int i = 1; i < 16; i++) ib[i] = 0xd0d00000u + (uint32_t)i;
    // The IB is written after the unmaps: the tracer's copy taken at an unmap holds 0xee there, so only a mapping
    // it still follows right gives these dwords.
    memset(p, 0xee, 3 * 4096);
    if (munmap(p, 4096)) fail("munmap of the front page", -1, errno);
    if (munmap(p + 2 * 4096, 4096)) fail("munmap of the tail page", -1, errno);
    ok("munmap of the front and the tail of a buffer's mapping");
    memcpy(p + 4096, ib, sizeof ib);
    printf("drmhostile: ib va=0x%llx data=", IB_VA + 4096);
    for (size_t i = 0; i < sizeof ib; i++) printf("%02x", ((unsigned char *)ib)[i]);
    printf("\n");

    struct drm_amdgpu_bo_list_entry e = { .bo_handle = h };
    struct drm_amdgpu_bo_list_in list = { .operation = ~0u, .list_handle = ~0u, .bo_number = 1, .bo_info_size = sizeof e,
                                          .bo_info_ptr = (uint64_t)(uintptr_t)&e };
    struct drm_amdgpu_cs_chunk_ib cib = { .ip_type = AMDGPU_HW_IP_GFX, .va_start = IB_VA + 4096, .ib_bytes = sizeof ib };
    struct drm_amdgpu_cs_chunk ch[2] = {
        { .chunk_id = AMDGPU_CHUNK_ID_BO_HANDLES, .length_dw = sizeof list / 4, .chunk_data = (uint64_t)(uintptr_t)&list },
        { .chunk_id = AMDGPU_CHUNK_ID_IB, .length_dw = sizeof cib / 4, .chunk_data = (uint64_t)(uintptr_t)&cib },
    };
    uint64_t chp[2] = { (uint64_t)(uintptr_t)&ch[0], (uint64_t)(uintptr_t)&ch[1] };
    union drm_amdgpu_cs good = { .in = { .ctx_id = c, .num_chunks = 2, .chunks = (uint64_t)(uintptr_t)chp } };
    if ((r = ioctl(fd, DRM_IOCTL_AMDGPU_CS, &good))) fail("CS of a NOP IB in the part still mapped", r, errno);
    ok("CS of a NOP IB in the part still mapped");
    // An absolute CLOCK_MONOTONIC timeout 5 s ahead: the NOP is done long before, so a replay gets the same answer.
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    union drm_amdgpu_wait_cs w = { .in = { .handle = good.out.handle, .ip_type = AMDGPU_HW_IP_GFX, .ctx_id = c,
                                           .timeout = (uint64_t)now.tv_sec * 1000000000ull + (uint64_t)now.tv_nsec + 5000000000ull } };
    if ((r = ioctl(fd, DRM_IOCTL_AMDGPU_WAIT_CS, &w)) || w.out.status) fail("WAIT_CS of the NOP CS", r, errno);
    ok("WAIT_CS of the NOP CS");

    munmap(p + 4096, 4096);
    struct drm_gem_close gc = { .handle = h };
    ioctl(fd, DRM_IOCTL_GEM_CLOSE, &gc);
    union drm_amdgpu_ctx fr = { .in = { .op = AMDGPU_CTX_OP_FREE_CTX, .ctx_id = c } };
    ioctl(fd, DRM_IOCTL_AMDGPU_CTX, &fr);
}

int main(int argc, char **argv) {
    const char *node = "/dev/dri/renderD128";
    int host = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--node") && i + 1 < argc) node = argv[++i];
        else if (!strcmp(argv[i], "--host")) host = 1;
        else { fprintf(stderr, "usage: drmhostile [--node PATH] [--host]\n"); return 2; }
    }
    fd = open(node, O_RDWR | O_CLOEXEC);
    if (fd < 0) fail(node, fd, errno);
    host_checks();
    if (!host) cs_checks();
    printf("drmhostile: ok %d checks\n", checks);
    return 0;
}
