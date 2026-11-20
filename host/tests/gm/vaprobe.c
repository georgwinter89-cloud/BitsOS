// GM-3v: finds and loads a VA-API driver the way libva does (each directory of LIBVA_DRIVERS_PATH, the file
// <LIBVA_DRIVER_NAME>_drv_video.so, dlopen RTLD_NOW|RTLD_GLOBAL, then the newest __vaDriverInit_1_N it exports) and
// calls it without a device: once with no DRM fd (-1) and once with /dev/null, which is no DRM node. A driver that
// answers both with a VA error and returns is plain; a crash or VA_STATUS_SUCCESS is not. Used by 80-vainfo-stage.sh,
// started by the Mesa tree's `run`, so the search path is run's and the driver's libraries come from the tree.
#include <dlfcn.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va_backend.h>
#include <va/va_backend_vpp.h>
#include <va/va_drmcommon.h>

typedef VAStatus (*init_fn)(VADriverContextP);

static VAStatus try_fd(init_fn init, int fd) {
    struct drm_state *st = calloc(1, sizeof(*st));
    VADriverContextP ctx = calloc(1, sizeof(*ctx));
    struct VADriverVTable *vt = calloc(1, sizeof(*vt));
    struct VADriverVTableVPP *vpp = calloc(1, sizeof(*vpp));
    if (!st || !ctx || !vt || !vpp) {
        printf("no memory\n");
        exit(1);
    }
    st->fd = fd;
    ctx->drm_state = st;
    ctx->display_type = VA_DISPLAY_DRM;
    ctx->vtable = vt;
    ctx->vtable_vpp = vpp;
    ctx->version_major = VA_MAJOR_VERSION;
    ctx->version_minor = VA_MINOR_VERSION;
    return init(ctx);
}

int main(void) {
    const char *dirs = getenv("LIBVA_DRIVERS_PATH"), *drv = getenv("LIBVA_DRIVER_NAME");
    if (!dirs || !drv) {
        printf("LIBVA_DRIVERS_PATH or LIBVA_DRIVER_NAME is not set\n");
        return 1;
    }
    char path[4096];
    void *h = NULL;
    for (const char *d = dirs; *d && !h;) {
        size_t n = strcspn(d, ":");
        snprintf(path, sizeof(path), "%.*s/%s_drv_video.so", (int)n, d, drv);
        h = dlopen(path, RTLD_NOW | RTLD_GLOBAL);
        if (!h)
            printf("dlopen %s: %s\n", path, dlerror());
        d += n + (d[n] == ':');
    }
    if (!h)
        return 1;
    printf("driver: %s\n", path);
    init_fn init = NULL;
    char name[64];
    for (int minor = VA_MINOR_VERSION; minor >= 0 && !init; minor--) {
        snprintf(name, sizeof(name), "__vaDriverInit_%d_%d", VA_MAJOR_VERSION, minor);
        init = (init_fn)dlsym(h, name);
    }
    if (!init) {
        printf("no __vaDriverInit_%d_* in %s\n", VA_MAJOR_VERSION, path);
        return 1;
    }
    printf("init: %s\n", name);
    VAStatus none = try_fd(init, -1);
    printf("no fd: status 0x%x\n", none);
    int fd = open("/dev/null", O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        printf("open /dev/null failed\n");
        return 1;
    }
    VAStatus null = try_fd(init, fd);
    printf("/dev/null: status 0x%x\n", null);
    close(fd);
    return none != VA_STATUS_SUCCESS && null != VA_STATUS_SUCCESS ? 0 : 1;
}
