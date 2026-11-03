// DR-4: libdrm_amdgpu's device set-up, statically linked (host/tests/dr/libdrm/build.sh), against
// /dev/dri/renderD128: amdgpu_device_initialize, amdgpu_query_gpu_info, then every AMDGPU_INFO query Mesa makes
// at device creation through libdrm's own wrappers (MAX_IBS has none: raw), printed as
// "Q <query> a=<args> sz=<size>:<answer bytes>" like the capture of the real 780M
// (host/tests/dr/fixtures/780m-info.txt), and the derived gpu_info fields as "G name=value". Then a context
// (amdgpu_cs_ctx_create2 at NORMAL, LOW and HIGH) as RADV does. Exit 0 when every call worked.
#include <amdgpu.h>
#include <amdgpu_drm.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <xf86drm.h>

static int bad;

static void q(unsigned query, const unsigned a[4], unsigned size, const void *p, int r) {
    if (r) {
        printf("Q %#x a=%u,%u,%u,%u sz=%u: error %d\n", query, a[0], a[1], a[2], a[3], size, r);
        bad = 1;
        return;
    }
    printf("Q %#x a=%u,%u,%u,%u sz=%u:", query, a[0], a[1], a[2], a[3], size);
    for (unsigned i = 0; i < size; i++)
        printf("%02x", ((const unsigned char *)p)[i]);
    printf("\n");
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
    printf("amdgpu_device_initialize: %d (DRM %u.%u)\n", r, major, minor);
    if (r)
        return 1;

    struct amdgpu_gpu_info gi;
    r = amdgpu_query_gpu_info(dev, &gi);
    printf("amdgpu_query_gpu_info: %d\n", r);
    if (r)
        return 1;
    printf("G asic_id=%#x chip_rev=%#x chip_external_rev=%#x family_id=%u pci_rev_id=%#x\n", gi.asic_id, gi.chip_rev,
           gi.chip_external_rev, gi.family_id, gi.pci_rev_id);
    printf("G ids_flags=%#llx max_engine_clk=%llu max_memory_clk=%llu gpu_counter_freq=%u\n",
           (unsigned long long)gi.ids_flags, (unsigned long long)gi.max_engine_clk,
           (unsigned long long)gi.max_memory_clk, gi.gpu_counter_freq);
    printf("G num_shader_engines=%u num_shader_arrays_per_engine=%u rb_pipes=%u enabled_rb_pipes_mask=%#x "
           "num_hw_gfx_contexts=%u\n",
           gi.num_shader_engines, gi.num_shader_arrays_per_engine, gi.rb_pipes, gi.enabled_rb_pipes_mask,
           gi.num_hw_gfx_contexts);
    printf("G vram_type=%u vram_bit_width=%u ce_ram_size=%u vce_harvest_config=%u gb_addr_cfg=%#x\n", gi.vram_type,
           gi.vram_bit_width, gi.ce_ram_size, gi.vce_harvest_config, gi.gb_addr_cfg);
    printf("marketing name: %s\n", amdgpu_get_marketing_name(dev) ? amdgpu_get_marketing_name(dev) : "(none)");

    unsigned a[4] = {0};
    // Answers land in buffers filled with 0xa5, so bytes the kernel leaves out show.
    struct drm_amdgpu_info_device di;
    memset(&di, 0xa5, sizeof di);
    r = amdgpu_query_info(dev, AMDGPU_INFO_DEV_INFO, sizeof di, &di);
    q(AMDGPU_INFO_DEV_INFO, a, sizeof di, &di, r);
    for (unsigned t = 0; t < AMDGPU_HW_IP_NUM; t++) {
        struct drm_amdgpu_info_hw_ip ip;
        unsigned b[4] = {t, 0, 0, 0};
        memset(&ip, 0xa5, sizeof ip);
        r = amdgpu_query_hw_ip_info(dev, t, 0, &ip);
        q(AMDGPU_INFO_HW_IP_INFO, b, sizeof ip, &ip, r);
    }
    unsigned fws[] = {AMDGPU_INFO_FW_GFX_ME, AMDGPU_INFO_FW_GFX_MEC, AMDGPU_INFO_FW_GFX_PFP, AMDGPU_INFO_FW_VCN};
    for (unsigned i = 0; i < 4; i++) {
        uint32_t fw[2] = {0xa5a5a5a5, 0xa5a5a5a5};
        unsigned b[4] = {fws[i], 0, 0, 0};
        r = amdgpu_query_firmware_version(dev, fws[i], 0, 0, &fw[0], &fw[1]);
        q(AMDGPU_INFO_FW_VERSION, b, sizeof fw, fw, r);
    }
    struct drm_amdgpu_memory_info mem;
    memset(&mem, 0xa5, sizeof mem);
    r = amdgpu_query_info(dev, AMDGPU_INFO_MEMORY, sizeof mem, &mem);
    q(AMDGPU_INFO_MEMORY, a, sizeof mem, &mem, r);
    for (unsigned t = 0; t < 2; t++) {
        struct drm_amdgpu_info_video_caps caps;
        unsigned b[4] = {t, 0, 0, 0};
        memset(&caps, 0xa5, sizeof caps);
        r = amdgpu_query_video_caps_info(dev, t, sizeof caps, &caps);
        q(AMDGPU_INFO_VIDEO_CAPS, b, sizeof caps, &caps, r);
    }
    // AMDGPU_INFO_MAX_IBS (0x22): newer than libdrm's wrappers, as Mesa asks it.
    uint32_t max_ibs[AMDGPU_HW_IP_NUM];
    memset(max_ibs, 0xa5, sizeof max_ibs);
    struct drm_amdgpu_info req = {.return_pointer = (uintptr_t)max_ibs, .return_size = sizeof max_ibs, .query = 0x22};
    r = drmCommandWrite(fd, DRM_AMDGPU_INFO, &req, sizeof req);
    q(0x22, a, sizeof max_ibs, max_ibs, r);
    struct drm_amdgpu_info_vram_gtt vg;
    memset(&vg, 0xa5, sizeof vg);
    r = amdgpu_query_info(dev, AMDGPU_INFO_VRAM_GTT, sizeof vg, &vg);
    q(AMDGPU_INFO_VRAM_GTT, a, sizeof vg, &vg, r);

    // Contexts as RADV makes them: NORMAL and LOW for everyone, HIGH refused (EACCES) without CAP_SYS_NICE.
    int prios[] = {AMDGPU_CTX_PRIORITY_NORMAL, AMDGPU_CTX_PRIORITY_LOW, AMDGPU_CTX_PRIORITY_HIGH};
    for (unsigned i = 0; i < 3; i++) {
        amdgpu_context_handle ctx;
        r = amdgpu_cs_ctx_create2(dev, prios[i], &ctx);
        printf("ctx prio %d: %d\n", prios[i], r);
        if (r == 0) {
            uint64_t flags = ~0ull;
            int q2 = amdgpu_cs_query_reset_state2(ctx, &flags);
            printf("ctx prio %d: query_reset_state2 %d flags %#llx\n", prios[i], q2, (unsigned long long)flags);
            if (q2 || flags)
                bad = 1;
            amdgpu_cs_ctx_free(ctx);
        } else if (prios[i] <= AMDGPU_CTX_PRIORITY_NORMAL) {
            bad = 1;
        }
    }
    amdgpu_device_deinitialize(dev);
    close(fd);
    printf("libdrm-info: %s\n", bad ? "errors" : "ok");
    return bad;
}
