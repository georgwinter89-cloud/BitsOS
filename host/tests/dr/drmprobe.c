/* DR-2: libdrm's own device discovery against the kernel (host/tests/dr/20-radv.sh). Built with the host's gcc
 * against libdrm (glibc); on V2 it runs with the staged Mesa tree's loader and its libdrm.so.2, on CachyOS with
 * the host's. The "drm:" lines are device-neutral and must be the same on both; the "amdgpu:" line is libdrm_amdgpu's
 * device set-up, which on V2 stops at the first ioctl the fake does not answer yet (DR-3's start). */
#include <amdgpu.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <xf86drm.h>

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    drmDevicePtr devs[64];
    int n = drmGetDevices2(0, devs, 64);
    printf("drm: drmGetDevices2 >= 1: %s\n", n >= 1 ? "yes" : "no");
    int amd = -1;
    for (int i = 0; i < n; i++)
        if (devs[i]->bustype == DRM_BUS_PCI && devs[i]->deviceinfo.pci->vendor_id == 0x1002 && (devs[i]->available_nodes & (1 << DRM_NODE_RENDER)) && amd < 0)
            amd = i;
    printf("drm: an AMD PCI device with a render node: %s\n", amd >= 0 ? "yes" : "no");
    if (amd < 0)
        return 0;
    drmDevicePtr d = devs[amd];
    const char *render = d->nodes[DRM_NODE_RENDER];
    printf("drm: render node is /dev/dri/renderD*: %s\n", !strncmp(render, "/dev/dri/renderD", 16) ? "yes" : "no");
    char slot[32];
    snprintf(slot, sizeof slot, "%04x:%02x:%02x.%u", d->businfo.pci->domain, d->businfo.pci->bus, d->businfo.pci->dev, d->businfo.pci->func);
    printf("drm: subsystem vendor read: %s\n", d->deviceinfo.pci->subvendor_id != 0 ? "yes" : "no");
    int fd = open(render, O_RDWR | O_CLOEXEC);
    printf("drm: open render node: %s\n", fd >= 0 ? "yes" : "no");
    if (fd < 0)
        return 1;
    drmDevicePtr one = NULL;
    int r = drmGetDevice2(fd, DRM_DEVICE_GET_PCI_REVISION, &one);
    printf("drm: drmGetDevice2 = %d\n", r);
    if (r == 0) {
        printf("drm: same device as the list: %s\n", drmDevicesEqual(one, d) ? "yes" : "no");
        printf("drm: revision read: %s\n", one->deviceinfo.pci->revision_id != 0 ? "yes" : "no");
        drmFreeDevice(&one);
    }
    drmVersionPtr v = drmGetVersion(fd);
    printf("drm: drmGetVersion name=%s major=%d minor>=54: %s\n", v ? v->name : "-", v ? v->version_major : -1, v && v->version_minor >= 54 ? "yes" : "no");
    drmFreeVersion(v);
    uint64_t cap = 0;
    r = drmGetCap(fd, DRM_CAP_PRIME, &cap);
    printf("drm: PRIME cap = %d value=%llu\n", r, (unsigned long long)cap);
    printf("drm: render node type: %s\n", drmGetNodeTypeFromFd(fd) == DRM_NODE_RENDER ? "yes" : "no");
    char *name = drmGetRenderDeviceNameFromFd(fd);
    printf("drm: render device name from fd matches: %s\n", name && !strcmp(name, render) ? "yes" : "no");
    free(name);
    drmFreeDevices(devs, n);
    uint32_t major = 0, minor = 0;
    amdgpu_device_handle h = NULL;
    r = amdgpu_device_initialize(fd, &major, &minor, &h);
    printf("amdgpu: amdgpu_device_initialize = %d (%s)\n", r, r ? strerror(-r) : "ok");
    if (r == 0)
        amdgpu_device_deinitialize(h);
    close(fd);
    return 0;
}
