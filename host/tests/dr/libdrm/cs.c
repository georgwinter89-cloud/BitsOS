// DR-8: real libdrm_amdgpu's command-submission calls as RADV and radeonsi use them, on CachyOS's amdgpu and on
// V2's fake, without any job reaching a GPU (the host's GPU drives the desk's display): every amdgpu_cs_submit_raw2
// here is refused by the kernel before a job exists (a BO list handle no file has, checked after all chunks).
// Contexts at LOW and NORMAL, the reset query, fence status and WAIT_FENCES on numbers that need no job, and the
// chunk checks libdrm passes through. DR-9: BO lists (raw and by amdgpu_bo_handle) and FENCE_TO_HANDLE.
// Prints device-neutral lines only.
#include <amdgpu.h>
#include <amdgpu_drm.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

// A BO list handle no file has: Linux checks it after every chunk (amdgpu_cs_parser_bos), so a CS whose chunks
// pass gets ENOENT and never runs.
#define NO_LIST 0xdead

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
    amdgpu_context_handle low, ctx;
    check("amdgpu_cs_ctx_create2 LOW", amdgpu_cs_ctx_create2(dev, AMDGPU_CTX_PRIORITY_LOW, &low), 0);
    check("amdgpu_cs_ctx_create2 NORMAL", amdgpu_cs_ctx_create2(dev, AMDGPU_CTX_PRIORITY_NORMAL, &ctx), 0);
    uint64_t flags = 99;
    check("amdgpu_cs_query_reset_state2", amdgpu_cs_query_reset_state2(ctx, &flags), 0);
    printf("reset flags: %llu\n", (unsigned long long)flags);

    struct drm_amdgpu_cs_chunk_ib ib = {.ip_type = AMDGPU_HW_IP_GFX, .va_start = 1ull << 32, .ib_bytes = 16};
    struct drm_amdgpu_cs_chunk ch = {.chunk_id = AMDGPU_CHUNK_ID_IB, .length_dw = sizeof ib / 4, .chunk_data = (uintptr_t)&ib};
    uint64_t seq = 77;
    check("submit_raw2 no chunks", amdgpu_cs_submit_raw2(dev, ctx, 0, 0, &ch, &seq), -EINVAL);
    check("submit_raw2 unknown BO list", amdgpu_cs_submit_raw2(dev, ctx, NO_LIST, 1, &ch, &seq), -ENOENT);
    printf("sequence untouched: %s\n", seq == 77 ? "yes" : "no");
    ib.ring = 1;
    check("submit_raw2 GFX ring 1", amdgpu_cs_submit_raw2(dev, ctx, NO_LIST, 1, &ch, &seq), -EINVAL);
    ib.ring = 0;
    uint32_t so;
    check("amdgpu_cs_create_syncobj2", amdgpu_cs_create_syncobj2(dev, 0, &so), 0);
    struct drm_amdgpu_cs_chunk_sem sem = {.handle = so};
    struct drm_amdgpu_cs_chunk two[2] = {ch, {.chunk_id = AMDGPU_CHUNK_ID_SYNCOBJ_IN, .length_dw = 1, .chunk_data = (uintptr_t)&sem}};
    check("submit_raw2 in-syncobj without fence", amdgpu_cs_submit_raw2(dev, ctx, NO_LIST, 2, two, &seq), -EINVAL);
    two[1].chunk_id = AMDGPU_CHUNK_ID_SYNCOBJ_OUT;
    check("submit_raw2 out-syncobj", amdgpu_cs_submit_raw2(dev, ctx, NO_LIST, 2, two, &seq), -ENOENT);
    check("amdgpu_cs_syncobj_signal", amdgpu_cs_syncobj_signal(dev, &so, 1), 0);
    two[1].chunk_id = AMDGPU_CHUNK_ID_SYNCOBJ_IN;
    check("submit_raw2 signaled in-syncobj", amdgpu_cs_submit_raw2(dev, ctx, NO_LIST, 2, two, &seq), -ENOENT);
    check("amdgpu_cs_destroy_syncobj", amdgpu_cs_destroy_syncobj(dev, so), 0);

    // Fence numbers that need no job: 0 (libdrm's NULL submit, answered without the kernel) and "the last one"
    // (~0) of a context without jobs.
    struct amdgpu_cs_fence f = {.context = ctx, .ip_type = AMDGPU_HW_IP_GFX, .fence = 0};
    uint32_t expired = 7;
    check("query_fence_status seq 0", amdgpu_cs_query_fence_status(&f, 0, 0, &expired), 0);
    printf("expired: %u\n", expired);
    f.fence = ~0ull;
    check("query_fence_status last", amdgpu_cs_query_fence_status(&f, 0, 0, &expired), 0);
    printf("expired: %u\n", expired);
    f.fence = 1;
    check("query_fence_status not submitted", amdgpu_cs_query_fence_status(&f, 0, 0, &expired), -EINVAL);
    f.fence = ~0ull;
    f.ip_type = AMDGPU_HW_IP_COMPUTE;
    f.ring = 3;
    check("query_fence_status compute ring 3", amdgpu_cs_query_fence_status(&f, 0, 0, &expired), 0);
    f.ring = 4;
    check("query_fence_status compute ring 4", amdgpu_cs_query_fence_status(&f, 0, 0, &expired), -EINVAL);
    struct amdgpu_cs_fence fs[2] = {{.context = ctx, .ip_type = AMDGPU_HW_IP_GFX}, {.context = low, .ip_type = AMDGPU_HW_IP_DMA, .ring = 1}};
    uint32_t status = 9, first = 9;
    check("amdgpu_cs_wait_fences all", amdgpu_cs_wait_fences(fs, 2, true, 0, &status, &first), 0);
    printf("status %u\n", status);
    check("amdgpu_cs_wait_fences any", amdgpu_cs_wait_fences(fs, 2, false, 0, &status, &first), 0);
    printf("status %u first %u\n", status, first);
    fs[1].fence = 5;
    check("amdgpu_cs_wait_fences any, one not submitted", amdgpu_cs_wait_fences(fs, 2, false, 0, &status, &first), 0);
    printf("status %u first %u\n", status, first);
    check("amdgpu_cs_wait_fences all, one not submitted", amdgpu_cs_wait_fences(fs, 2, true, 0, &status, &first), -EINVAL);

    // DR-9: BO lists by handle (radeonsi's amdgpu_bo_list_create_raw + submit_raw2 path) and FENCE_TO_HANDLE;
    // still no job: the list CS is refused (a list handle with a BO_HANDLES chunk, or a destroyed list).
    amdgpu_bo_handle bos[2];
    struct amdgpu_bo_alloc_request req = {.alloc_size = 4096, .phys_alignment = 4096, .preferred_heap = AMDGPU_GEM_DOMAIN_GTT};
    check("amdgpu_bo_alloc", amdgpu_bo_alloc(dev, &req, &bos[0]), 0);
    check("amdgpu_bo_alloc 2", amdgpu_bo_alloc(dev, &req, &bos[1]), 0);
    struct drm_amdgpu_bo_list_entry ents[2];
    for (int i = 0; i < 2; i++) {
        uint32_t kms = 0;
        amdgpu_bo_export(bos[i], amdgpu_bo_handle_type_kms, &kms);
        ents[i] = (struct drm_amdgpu_bo_list_entry){.bo_handle = kms, .bo_priority = (uint32_t)i};
    }
    uint32_t lh = 0;
    check("amdgpu_bo_list_create_raw", amdgpu_bo_list_create_raw(dev, 2, ents, &lh), 0);
    printf("list handle: %u\n", lh);
    struct drm_amdgpu_bo_list_in lin = {.bo_number = 1, .bo_info_size = sizeof ents[0], .bo_info_ptr = (uintptr_t)ents};
    struct drm_amdgpu_cs_chunk with_list[2] = {ch, {.chunk_id = AMDGPU_CHUNK_ID_BO_HANDLES, .length_dw = sizeof lin / 4, .chunk_data = (uintptr_t)&lin}};
    check("submit_raw2 list handle and BO_HANDLES", amdgpu_cs_submit_raw2(dev, ctx, lh, 2, with_list, &seq), -EINVAL);
    check("amdgpu_bo_list_destroy_raw", amdgpu_bo_list_destroy_raw(dev, lh), 0);
    check("amdgpu_bo_list_destroy_raw again", amdgpu_bo_list_destroy_raw(dev, lh), 0);
    check("submit_raw2 destroyed BO list", amdgpu_cs_submit_raw2(dev, ctx, lh, 1, &ch, &seq), -ENOENT);
    amdgpu_bo_list_handle bl;
    uint8_t prios[2] = {1, 200};
    check("amdgpu_bo_list_create", amdgpu_bo_list_create(dev, 2, bos, prios, &bl), 0);
    check("amdgpu_bo_list_update", amdgpu_bo_list_update(bl, 1, bos, NULL), 0);
    check("amdgpu_bo_list_destroy", amdgpu_bo_list_destroy(bl), 0);
    struct amdgpu_cs_fence f0 = {.context = ctx, .ip_type = AMDGPU_HW_IP_GFX, .fence = 0};
    uint32_t sh = 0;
    check("amdgpu_cs_fence_to_handle syncobj", amdgpu_cs_fence_to_handle(dev, &f0, AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ, &sh), 0);
    check("amdgpu_cs_syncobj_wait on it", amdgpu_cs_syncobj_wait(dev, &sh, 1, 0, 0, NULL), 0);
    check("amdgpu_cs_destroy_syncobj it", amdgpu_cs_destroy_syncobj(dev, sh), 0);
    check("amdgpu_cs_fence_to_handle sync_file", amdgpu_cs_fence_to_handle(dev, &f0, AMDGPU_FENCE_TO_HANDLE_GET_SYNC_FILE_FD, &sh), 0);
    printf("sync_file fd: %s\n", fcntl((int)sh, F_GETFD) >= 0 ? "yes" : "no");
    close((int)sh);
    check("amdgpu_cs_fence_to_handle what 3", amdgpu_cs_fence_to_handle(dev, &f0, 3, &sh), -EINVAL);
    f0.fence = 1;
    check("amdgpu_cs_fence_to_handle not submitted", amdgpu_cs_fence_to_handle(dev, &f0, AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ, &sh), -EINVAL);
    check("amdgpu_bo_free", amdgpu_bo_free(bos[0]), 0);
    check("amdgpu_bo_free 2", amdgpu_bo_free(bos[1]), 0);

    check("amdgpu_cs_ctx_free", amdgpu_cs_ctx_free(ctx), 0);
    check("amdgpu_cs_ctx_free LOW", amdgpu_cs_ctx_free(low), 0);
    amdgpu_device_deinitialize(dev);
    close(fd);
    printf("libdrm-cs: %s\n", bad ? "errors" : "ok");
    return bad;
}
