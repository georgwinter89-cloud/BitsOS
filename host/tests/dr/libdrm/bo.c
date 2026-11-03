// DR-6: real libdrm_amdgpu's buffer calls as radeonsi and RADV use them, on CachyOS's amdgpu and on V2's fake:
// amdgpu_bo_alloc (GTT, VRAM, USWC), amdgpu_bo_cpu_map (GEM_MMAP + mmap of the render node), set/query metadata,
// amdgpu_bo_query_info, export as dma-buf fd and KMS handle, import back (libdrm finds its own object again),
// wait_for_idle, free. DR-7: libdrm's VA manager and amdgpu_bo_va_op (GEM_VA MAP/UNMAP/REPLACE/CLEAR) in the low
// and the high range, as RADV binds memory. Prints device-neutral lines only (no handles, addresses or chip numbers).
#include <amdgpu.h>
#include <amdgpu_drm.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <xf86drm.h>

// Create flags every amdgpu reports back as given (newer kernels add VRAM_CLEARED, RAS adds WIPE_ON_RELEASE).
#define SHOWN 0x10f7ull

static int bad;

static void check(const char *what, int r, int want) {
    printf("%s: %d\n", what, r);
    if (r != want)
        bad = 1;
}

int main(int argc, char **argv) {
    const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    int fd = open(node, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        printf("open %s: %s\n", node, strerror(errno));
        return 1;
    }
    uint32_t major, minor;
    amdgpu_device_handle dev;
    int r = amdgpu_device_initialize(fd, &major, &minor, &dev);
    check("amdgpu_device_initialize", r, 0);
    if (r)
        return 1;

    struct { uint64_t size, align; uint32_t heap; uint64_t flags; } reqs[] = {
        {3 * 4096 + 1, 4096, AMDGPU_GEM_DOMAIN_GTT, 0},
        {65536, 65536, AMDGPU_GEM_DOMAIN_GTT, AMDGPU_GEM_CREATE_CPU_GTT_USWC},
        {8192, 4096, AMDGPU_GEM_DOMAIN_VRAM, AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED},
    };
    for (unsigned i = 0; i < sizeof reqs / sizeof reqs[0]; i++) {
        struct amdgpu_bo_alloc_request q = {.alloc_size = reqs[i].size, .phys_alignment = reqs[i].align,
                                            .preferred_heap = reqs[i].heap, .flags = reqs[i].flags};
        amdgpu_bo_handle bo;
        printf("== bo %u: size %llu heap %u flags %#llx\n", i, (unsigned long long)q.alloc_size, q.preferred_heap,
               (unsigned long long)q.flags);
        check("amdgpu_bo_alloc", amdgpu_bo_alloc(dev, &q, &bo), 0);
        void *cpu = NULL;
        r = amdgpu_bo_cpu_map(bo, &cpu);
        check("amdgpu_bo_cpu_map", r, 0);
        if (!r) {
            unsigned char *p = cpu;
            int zero = p[0] == 0 && p[reqs[i].size - 1] == 0;
            memset(p, 0x40 + i, reqs[i].size);
            printf("fresh memory reads zero: %s\n", zero ? "yes" : "no");
        }
        struct amdgpu_bo_metadata md = {.flags = 3, .tiling_info = 0x55ull << 32 | 9, .size_metadata = 8};
        md.umd_metadata[0] = 0xcafe;
        md.umd_metadata[1] = 0xf00d;
        check("amdgpu_bo_set_metadata", amdgpu_bo_set_metadata(bo, &md), 0);
        struct amdgpu_bo_info info;
        memset(&info, 0, sizeof info);
        check("amdgpu_bo_query_info", amdgpu_bo_query_info(bo, &info), 0);
        printf("info size=%llu align=%llu heap=%u flags=%#llx md flags=%llu tiling=%#llx size=%u data=%#x,%#x\n",
               (unsigned long long)info.alloc_size, (unsigned long long)info.phys_alignment, info.preferred_heap,
               (unsigned long long)(info.alloc_flags & SHOWN), (unsigned long long)info.metadata.flags,
               (unsigned long long)info.metadata.tiling_info, info.metadata.size_metadata, info.metadata.umd_metadata[0],
               info.metadata.umd_metadata[1]);
        uint32_t kms = 0, dmabuf = 0;
        check("amdgpu_bo_export kms", amdgpu_bo_export(bo, amdgpu_bo_handle_type_kms, &kms), 0);
        check("amdgpu_bo_export dma_buf_fd", amdgpu_bo_export(bo, amdgpu_bo_handle_type_dma_buf_fd, &dmabuf), 0);
        printf("kms handle > 0: %s\n", kms > 0 ? "yes" : "no");
        printf("dma-buf size by lseek: %lld\n", (long long)lseek((int)dmabuf, 0, SEEK_END));
        struct amdgpu_bo_import_result imp;
        memset(&imp, 0, sizeof imp);
        check("amdgpu_bo_import dma_buf_fd", amdgpu_bo_import(dev, amdgpu_bo_handle_type_dma_buf_fd, dmabuf, &imp), 0);
        printf("import gives the same object: %s, size %llu\n", imp.buf_handle == bo ? "yes" : "no",
               (unsigned long long)imp.alloc_size);
        if (imp.buf_handle)
            amdgpu_bo_free(imp.buf_handle);
        // A program on the other side maps the dma-buf and sees the bytes written through libdrm's map.
        unsigned char *m = mmap(NULL, reqs[i].size, PROT_READ, MAP_SHARED, (int)dmabuf, 0);
        printf("dma-buf map sees the bytes: %s\n", m != MAP_FAILED && m[0] == 0x40 + i ? "yes" : "no");
        if (m != MAP_FAILED)
            munmap(m, reqs[i].size);
        close((int)dmabuf);
        bool busy = true;
        check("amdgpu_bo_wait_for_idle", amdgpu_bo_wait_for_idle(bo, 0, &busy), 0);
        printf("busy: %s\n", busy ? "yes" : "no");
        // amdgpu_bo_va_op rounds the size up to pages, the raw call does not (CLEAR wants whole pages).
        uint64_t whole = (reqs[i].size + 4095) & ~4095ull;
        for (int hi = 0; hi < 2; hi++) {
            uint64_t va = 0;
            amdgpu_va_handle vh = NULL;
            check(hi ? "amdgpu_va_range_alloc high" : "amdgpu_va_range_alloc", amdgpu_va_range_alloc(dev, amdgpu_gpu_va_range_general,
                  reqs[i].size, 0, 0, &va, &vh, hi ? AMDGPU_VA_RANGE_HIGH : 0), 0);
            check("amdgpu_bo_va_op MAP", amdgpu_bo_va_op(bo, 0, reqs[i].size, va, 0, AMDGPU_VA_OP_MAP), 0);
            check("amdgpu_bo_va_op MAP again", amdgpu_bo_va_op(bo, 0, reqs[i].size, va, 0, AMDGPU_VA_OP_MAP), -EINVAL);
            check("amdgpu_bo_va_op_raw REPLACE", amdgpu_bo_va_op_raw(dev, bo, 0, 4096, va, AMDGPU_VM_PAGE_READABLE, AMDGPU_VA_OP_REPLACE), 0);
            check("amdgpu_bo_va_op_raw CLEAR", amdgpu_bo_va_op_raw(dev, NULL, 0, whole, va, 0, AMDGPU_VA_OP_CLEAR), 0);
            check("amdgpu_bo_va_op UNMAP after CLEAR", amdgpu_bo_va_op(bo, 0, reqs[i].size, va, 0, AMDGPU_VA_OP_UNMAP), -ENOENT);
            check("amdgpu_bo_va_op MAP again", amdgpu_bo_va_op(bo, 0, reqs[i].size, va, 0, AMDGPU_VA_OP_MAP), 0);
            check("amdgpu_bo_va_op UNMAP", amdgpu_bo_va_op(bo, 0, reqs[i].size, va, 0, AMDGPU_VA_OP_UNMAP), 0);
            check("amdgpu_va_range_free", amdgpu_va_range_free(vh), 0);
        }
        check("amdgpu_bo_cpu_unmap", amdgpu_bo_cpu_unmap(bo), 0);
        check("amdgpu_bo_free", amdgpu_bo_free(bo), 0);
    }
    amdgpu_device_deinitialize(dev);
    close(fd);
    printf("libdrm-bo: %s\n", bad ? "errors" : "ok");
    return bad;
}
