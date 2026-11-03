/* DR-2: the DRM render node as libdrm and Mesa meet it: the node itself, the core ioctls with the
 * size-tolerant copy rules (linux-6.6/drivers/gpu/drm/drm_ioctl.c:812-890), the render node's refusals, and the
 * /sys walk of libdrm's drmGetDevices2 (gx-scout-uapi.md §2); DR-4: the INFO and CTX rules. Takes the first
 * /dev/dri/renderD* whose driver is
 * "amdgpu" (on CachyOS the real GPU, on V2 under QEMU the fake). Prints only device-neutral facts: no ids,
 * slots, paths of the host's PCI tree, or version numbers beyond Mesa's floor. */
#include "abi.h"
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>

struct drm_version {
    int major, minor, patch;
    int pad;
    unsigned long name_len;
    char *name;
    unsigned long date_len;
    char *date;
    unsigned long desc_len;
    char *desc;
};
struct drm_unique { unsigned long len; char *unique; };
struct drm_client { int idx, auth; unsigned long pid, uid, magic, iocs; };
struct drm_get_cap { uint64_t capability, value; };
struct drm_gem_close { uint32_t handle, pad; };

#define IOC(dir, nr, size) ((unsigned long)(dir) << 30 | (unsigned long)(size) << 16 | 0x64u << 8 | (nr))
#define RW 3u
#define W 1u
#define R 2u
#define VERSION IOC(RW, 0x00, 64)
#define GET_UNIQUE IOC(RW, 0x01, 16)
#define GET_MAGIC IOC(R, 0x02, 4)
#define GET_CLIENT IOC(RW, 0x05, 40)
#define GET_CAP IOC(RW, 0x0c, 16)
#define SET_MASTER IOC(0, 0x1e, 0)
#define GEM_CLOSE IOC(W, 0x09, 8)

static int fd = -1;
static char node[64], sysnode[64];
static unsigned minor_no;

static long io(unsigned long cmd, void *arg) {
    return ioctl(fd, cmd, arg) == -1 ? -errno : 0;
}

static void res(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, ename((int)-r));
    else
        printf("%s = %ld\n", label, r);
}

/* The whole file as text (NUL-terminated), or NULL. */
static char *slurp(const char *path, char *buf, size_t size) {
    int f = open(path, O_RDONLY | O_CLOEXEC);
    if (f < 0)
        return NULL;
    ssize_t n = read(f, buf, size - 1);
    close(f);
    if (n < 0)
        return NULL;
    buf[n] = 0;
    return buf;
}

static int is_hex(const char *s, int digits) {
    if (s[0] != '0' || s[1] != 'x')
        return 0;
    for (int i = 0; i < digits; i++)
        if (!strchr("0123456789abcdef", s[2 + i]))
            return 0;
    return s[2 + digits] == '\n' && s[3 + digits] == 0;
}

static int pick_node(void) {
    DIR *d = opendir("/dev/dri");
    if (!d)
        return 0;
    char best[64] = "";
    struct dirent *e;
    while ((e = readdir(d))) {
        if (strncmp(e->d_name, "renderD", 7))
            continue;
        char p[64];
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
    strcpy(node, best);
    fd = open(node, O_RDWR | O_CLOEXEC);
    return fd >= 0;
}

static void node_checks(void) {
    struct stat st;
    yes("stat", stat(node, &st) == 0);
    yes("char device", S_ISCHR(st.st_mode));
    yes("mode 0666", (st.st_mode & 07777) == 0666);
    yes("major 226", major(st.st_rdev) == 226);
    minor_no = minor(st.st_rdev);
    yes("render minor 128..191", minor_no >= 128 && minor_no < 192);
    char want[32];
    snprintf(want, sizeof want, "renderD%u", minor_no);
    yes("name matches minor", !strcmp(strrchr(node, '/') + 1, want));
    struct stat dst;
    yes("/dev/dri is a directory", stat("/dev/dri", &dst) == 0 && S_ISDIR(dst.st_mode));
    struct stat fst;
    yes("fstat matches stat", fstat(fd, &fst) == 0 && fst.st_rdev == st.st_rdev && S_ISCHR(fst.st_mode));
    snprintf(sysnode, sizeof sysnode, "/sys/dev/char/226:%u", minor_no);
}

static void version_checks(void) {
    struct drm_version v = {0};
    res("VERSION lengths", io(VERSION, &v));
    yes("major 3", v.major == 3);
    yes("minor >= 54", v.minor >= 54);
    /* The date is "20150101" in Linux 6.6 and "0" since DRIVER_DATE went (6.14): only its presence counts. */
    printf("name_len=%lu desc_len=%lu date_len>0=%s\n", v.name_len, v.desc_len, v.date_len > 0 ? "yes" : "no");
    char name[16], date[16], desc[16];
    memset(name, 0x55, sizeof name);
    memset(date, 0x55, sizeof date);
    memset(desc, 0x55, sizeof desc);
    v.name = name, v.name_len = 3;
    v.date = date, v.date_len = sizeof date;
    v.desc = desc, v.desc_len = sizeof desc;
    res("VERSION strings", io(VERSION, &v));
    printf("name=%.3s after=%#x name_len=%lu\n", name, (unsigned char)name[3], v.name_len);
    printf("desc=%.*s nul_after_date=%s\n", (int)v.desc_len, desc, v.date_len < sizeof date && date[v.date_len] == 0x55 ? "no" : "yes");
    struct drm_version bad = {.name_len = 6, .name = KERNEL_ADDR};
    res("VERSION bad name buffer", io(VERSION, &bad));
    printf("lengths back after EFAULT: name_len=%lu major=%d\n", bad.name_len, bad.major);
    res("VERSION null argument", io(VERSION, NULL));

    /* The caller's size decides: 32 bytes in and out (the rest of the caller's memory untouched). */
    unsigned char buf[128];
    memset(buf, 0, 32);
    memset(buf + 32, 0x77, 96);
    res("VERSION size 32", io(IOC(RW, 0, 32), buf));
    struct drm_version *h = (struct drm_version *)buf;
    printf("size 32: major=%d name_len=%lu tail_untouched=%s\n", h->major, h->name_len, buf[32] == 0x77 && buf[63] == 0x77 ? "yes" : "no");
    memset(buf, 0, 80);
    memset(buf + 64, 0x33, 16);
    memset(buf + 80, 0x44, 48);
    res("VERSION size 80", io(IOC(RW, 0, 80), buf));
    printf("size 80: major=%d extra_kept=%s beyond_kept=%s\n", h->major, buf[64] == 0x33 && buf[79] == 0x33 ? "yes" : "no", buf[80] == 0x44 ? "yes" : "no");
    memset(buf, 0x11, 64);
    res("VERSION no direction bits", io(IOC(0, 0, 64), buf));
    printf("no direction: untouched=%s\n", buf[0] == 0x11 && buf[16] == 0x11 ? "yes" : "no");
    memset(buf, 0, 64);
    res("VERSION write-only", io(IOC(W, 0, 64), buf));
    printf("write-only: major=%d\n", h->major);
    res("VERSION read-only", io(IOC(R, 0, 64), buf));
    printf("read-only: major=%d name_len=%lu\n", h->major, h->name_len);
    /* The upper half of the command register is not part of the number. */
    res("VERSION upper bits", syscall(SYS_ioctl, fd, (1ul << 32) | VERSION, &v) == -1 ? -errno : 0);
}

static void cap_checks(void) {
    static const struct { unsigned cap; const char *name; } caps[] = {
        {1, "DUMB_BUFFER"}, {2, "VBLANK_HIGH_CRTC"}, {3, "DUMB_PREFERRED_DEPTH"}, {4, "DUMB_PREFER_SHADOW"},
        {5, "PRIME"}, {6, "TIMESTAMP_MONOTONIC"}, {7, "ASYNC_PAGE_FLIP"}, {0x10, "ADDFB2_MODIFIERS"},
        {0x11, "PAGE_FLIP_TARGET"}, {0x12, "CRTC_IN_VBLANK_EVENT"}, {0x13, "SYNCOBJ"}, {0x14, "SYNCOBJ_TIMELINE"},
        {0, "0 (none)"}, {0x7f, "0x7f (unknown)"},
    };
    for (unsigned i = 0; i < sizeof caps / sizeof caps[0]; i++) {
        struct drm_get_cap c = {.capability = caps[i].cap, .value = 0xdeadbeef};
        long r = io(GET_CAP, &c);
        if (r < 0)
            printf("GET_CAP %s = -%s value=%llu\n", caps[i].name, ename((int)-r), (unsigned long long)c.value);
        else
            printf("GET_CAP %s = %llu\n", caps[i].name, (unsigned long long)c.value);
    }
    struct drm_get_cap wide = {.capability = 1ull << 32 | 5};
    res("GET_CAP 64-bit capability", io(GET_CAP, &wide));
    res("GET_CAP null argument", io(GET_CAP, NULL));
    res("GET_CAP kernel argument", io(GET_CAP, KERNEL_ADDR));
}

static void refusal_checks(void) {
    char ubuf[64];
    struct drm_unique u = {.len = sizeof ubuf, .unique = ubuf};
    res("GET_UNIQUE on a render node", io(GET_UNIQUE, &u));
    res("GET_UNIQUE null argument", io(GET_UNIQUE, NULL));
    struct drm_client cl = {0};
    res("GET_CLIENT on a render node", io(GET_CLIENT, &cl));
    unsigned magic;
    res("GET_MAGIC on a render node", io(GET_MAGIC, &magic));
    res("SET_MASTER on a render node", io(SET_MASTER, NULL));
    struct drm_gem_close gc = {.handle = 0x7fff0000};
    res("GEM_CLOSE unknown handle", io(GEM_CLOSE, &gc));
    char any[64] = {0};
    res("type 'E' number", ioctl(fd, 0x80044500ul, any) == -1 ? -errno : 0);
    res("core number 0xfe", io(IOC(R, 0xfe, 8), any));
    res("driver number 0x9f", io(IOC(W, 0x9f, 8), any));
    res("AMDGPU_SCHED (master only)", io(IOC(W, 0x55, 16), any));
}

static void file_checks(void) {
    char b[8];
    int fl = fcntl(fd, F_GETFL);
    fcntl(fd, F_SETFL, fl | O_NONBLOCK);
    res("read without events (nonblocking)", read(fd, b, sizeof b) == -1 ? -errno : 0);
    fcntl(fd, F_SETFL, fl);
    res("write", write(fd, "x", 1) == -1 ? -errno : 0);
    /* 6.6 (no llseek in amdgpu's fops) says ESPIPE, CachyOS's newer kernel EPERM: only that it fails. */
    yes("lseek fails", lseek(fd, 0, SEEK_CUR) == -1);
    struct pollfd p = {.fd = fd, .events = POLLIN | POLLOUT};
    res("poll (no events)", poll(&p, 1, 0));
    printf("poll revents=%#x\n", p.revents);
    int fd2 = open(node, O_RDWR | O_CLOEXEC);
    struct drm_version v = {0};
    yes("second open answers", fd2 >= 0 && ioctl(fd2, VERSION, &v) == 0 && v.major == 3);
    close(fd2);
    char path[64], text[512];
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    char *t = slurp(path, text, sizeof text);
    yes("fdinfo drm-driver", t && strstr(t, "drm-driver:\tamdgpu\n"));
    yes("fdinfo drm-client-id", t && strstr(t, "drm-client-id:\t"));
    yes("fdinfo drm-pdev", t && strstr(t, "drm-pdev:\t"));
}

/* DR-4: AMDGPU_INFO and AMDGPU_CTX rules that hold on every amdgpu (amdgpu_kms.c:535, amdgpu_ctx.c:664); no chip
 * values. */
struct amdgpu_info { uint64_t ptr; uint32_t size, query; uint32_t a[4]; };
struct amdgpu_ctx { uint32_t op, flags, id; int32_t prio; };
#define AMDGPU_INFO IOC(W, 0x45, 32)
#define AMDGPU_CTX IOC(RW, 0x42, 16)

static long info(uint32_t query, uint32_t a0, uint32_t a1, uint32_t a2, void *out, uint32_t size) {
    struct amdgpu_info q = {.ptr = (uintptr_t)out, .size = size, .query = query, .a = {a0, a1, a2, 0}};
    return io(AMDGPU_INFO, &q);
}

static long ctx(uint32_t op, uint32_t flags, uint32_t id, int32_t prio, struct amdgpu_ctx *c) {
    *c = (struct amdgpu_ctx){.op = op, .flags = flags, .id = id, .prio = prio};
    return io(AMDGPU_CTX, c);
}

static void amdgpu_checks(void) {
    uint32_t v[2] = {0, 0};
    long r = info(0x00, 0, 0, 0, v, 4);
    printf("INFO ACCEL_WORKING = %ld value=%u\n", r, v[0]);
    res("INFO size 0", info(0x00, 0, 0, 0, v, 0));
    res("INFO null pointer", info(0x00, 0, 0, 0, NULL, 4));
    res("INFO kernel pointer", info(0x00, 0, 0, 0, KERNEL_ADDR, 4));
    res("INFO unknown query 0x7f", info(0x7f, 0, 0, 0, v, 4));
    r = info(0x03, 0, 0, 0, v, 4);
    printf("INFO HW_IP_COUNT GFX = %ld count=%u\n", r, v[0]);
    res("INFO HW_IP_COUNT type 10", info(0x03, 10, 0, 0, v, 4));
    unsigned char ip[40];
    memset(ip, 0x5a, sizeof ip);
    res("INFO HW_IP_INFO GFX", info(0x02, 0, 0, 0, ip, 32));
    yes("HW_IP_INFO GFX has rings, nothing past return_size", ip[24] != 0 && ip[32] == 0x5a && ip[39] == 0x5a);
    res("INFO HW_IP_INFO instance 1", info(0x02, 0, 1, 0, ip, 32));
    res("INFO HW_IP_INFO type 10", info(0x02, 10, 0, 0, ip, 32));
    res("INFO FW_VERSION ME instance 1", info(0x0e, 4, 1, 0, v, 8));
    res("INFO FW_VERSION unknown type 0x7f", info(0x0e, 0x7f, 0, 0, v, 8));
    res("INFO READ_MMR_REG 129 dwords", info(0x15, 0x263e, 129, 0xffffffffu, v, 4));
    res("INFO READ_MMR_REG SE 4", info(0x15, 0x263e, 1, 4, v, 4));
    unsigned char di[16];
    memset(di, 0x5a, sizeof di);
    res("INFO DEV_INFO 6 bytes", info(0x16, 0, 0, 0, di, 6));
    yes("DEV_INFO 6 bytes leaves the rest", di[6] == 0x5a && di[15] == 0x5a);

    struct amdgpu_ctx c, d;
    res("CTX ALLOC NORMAL", ctx(1, 0, 0, 0, &c));
    yes("CTX id > 0", c.op > 0);
    res("CTX ALLOC LOW", ctx(1, 0, 0, -512, &d));
    yes("CTX second id differs", d.op > 0 && d.op != c.op);
    struct amdgpu_ctx s;
    res("CTX QUERY_STATE2", ctx(4, 0, c.op, 0, &s));
    printf("CTX QUERY_STATE2 flags=%#llx\n", (unsigned long long)((uint64_t)s.flags << 32 | s.op));
    res("CTX GET_STABLE_PSTATE flags 1", ctx(5, 1, c.op, 0, &s));
    res("CTX SET_STABLE_PSTATE flags 0x10", ctx(6, 0x10, c.op, 0, &s));
    res("CTX SET_STABLE_PSTATE 5", ctx(6, 5, c.op, 0, &s));
    res("CTX QUERY_STATE2 unknown id", ctx(4, 0, 0x7ffffff0, 0, &s));
    res("CTX unknown op 9", ctx(9, 0, c.op, 0, &s));
    uint32_t id = c.op;
    res("CTX FREE", ctx(2, 0, id, 0, &c));
    res("CTX FREE again", ctx(2, 0, id, 0, &c));
    res("CTX FREE second", ctx(2, 0, d.op, 0, &d));
}

/* libdrm's walk (xf86drm.c drmParseSubsystemType, drmGetDevice2 -> drmParsePciBusInfo/DeviceInfo). */
static void sysfs_checks(void) {
    char p[256], q[256], t[512], real[4096], real2[4096];
    ssize_t n = readlink(sysnode, q, sizeof q - 1);
    q[n > 0 ? n : 0] = 0;
    char tail[32];
    snprintf(tail, sizeof tail, "/drm/renderD%u", minor_no);
    yes("dev/char link is relative into devices", n > 0 && !strncmp(q, "../../devices/pci", 17));
    yes("dev/char link ends in drm/renderD<minor>", n > 0 && strlen(q) > strlen(tail) && !strcmp(q + strlen(q) - strlen(tail), tail));
    snprintf(p, sizeof p, "%s/device/drm", sysnode);
    struct stat st;
    yes("device/drm exists", stat(p, &st) == 0 && S_ISDIR(st.st_mode));
    snprintf(p, sizeof p, "%s/device/subsystem", sysnode);
    n = readlink(p, q, sizeof q - 1);
    q[n > 0 ? n : 0] = 0;
    yes("device/subsystem ends in /pci", n > 4 && !strcmp(q + n - 4, "/pci"));
    yes("device/subsystem leads to /sys/bus/pci", realpath(p, real) && !strcmp(real, "/sys/bus/pci"));
    snprintf(p, sizeof p, "%s/device", sysnode);
    char *dev = realpath(p, real);
    unsigned dom, bus, slot, fn;
    const char *base = dev ? strrchr(dev, '/') + 1 : "";
    yes("device realpath is a PCI slot name", dev && sscanf(base, "%04x:%02x:%02x.%u", &dom, &bus, &slot, &fn) == 4 && strlen(base) == 12);
    yes("device realpath is below /sys/devices/pci", dev && !strncmp(dev, "/sys/devices/pci", 16));
    char devdir[4096];
    snprintf(devdir, sizeof devdir, "%s", dev ? dev : "/nonexistent");
    snprintf(p, sizeof p, "%s/uevent", devdir);
    char *ue = slurp(p, t, sizeof t);
    char slotline[64];
    snprintf(slotline, sizeof slotline, "PCI_SLOT_NAME=%s\n", base);
    yes("uevent has PCI_SLOT_NAME of the directory", ue && strstr(ue, slotline));
    yes("uevent starts with DRIVER=amdgpu", ue && !strncmp(ue, "DRIVER=amdgpu\n", 14));
    yes("uevent has PCI_ID 1002:", ue && strstr(ue, "\nPCI_ID=1002:"));
    yes("uevent has PCI_SUBSYS_ID", ue && strstr(ue, "\nPCI_SUBSYS_ID="));
    yes("uevent has PCI_CLASS=30000 or 38000", ue && (strstr(ue, "\nPCI_CLASS=30000\n") || strstr(ue, "\nPCI_CLASS=38000\n")));
    yes("uevent has MODALIAS=pci:v00001002", ue && strstr(ue, "\nMODALIAS=pci:v00001002d"));
    char vendor[32] = "", device[32] = "";
    snprintf(p, sizeof p, "%s/vendor", devdir);
    yes("vendor 0x1002", slurp(p, vendor, sizeof vendor) && !strcmp(vendor, "0x1002\n"));
    snprintf(p, sizeof p, "%s/device", devdir);
    yes("device 0x<4 hex>", slurp(p, device, sizeof device) && is_hex(device, 4));
    snprintf(p, sizeof p, "%s/subsystem_vendor", devdir);
    yes("subsystem_vendor 0x<4 hex>", slurp(p, t, sizeof t) && is_hex(t, 4));
    snprintf(p, sizeof p, "%s/subsystem_device", devdir);
    yes("subsystem_device 0x<4 hex>", slurp(p, t, sizeof t) && is_hex(t, 4));
    snprintf(p, sizeof p, "%s/revision", devdir);
    char revision[32] = "";
    yes("revision 0x<2 hex>", slurp(p, revision, sizeof revision) && is_hex(revision, 2));
    snprintf(p, sizeof p, "%s/class", devdir);
    yes("class 0x03xxxx", slurp(p, t, sizeof t) && !strncmp(t, "0x03", 4) && is_hex(t, 6));
    snprintf(p, sizeof p, "%s/config", devdir);
    unsigned char cfg[64];
    int cf = open(p, O_RDONLY | O_CLOEXEC);
    yes("config gives 64 bytes", cf >= 0 && read(cf, cfg, 64) == 64);
    if (cf >= 0)
        close(cf);
    yes("config vendor matches", cfg[0] == 0x02 && cfg[1] == 0x10);
    yes("config device matches", strtoul(device, NULL, 16) == (unsigned)(cfg[2] | cfg[3] << 8));
    yes("config revision matches", strtoul(revision, NULL, 16) == cfg[8]);
    yes("config class display", cfg[11] == 0x03);
    snprintf(p, sizeof p, "/sys/class/drm/renderD%u", minor_no);
    n = readlink(p, q, sizeof q - 1);
    q[n > 0 ? n : 0] = 0;
    yes("class/drm link is relative into devices", n > 0 && !strncmp(q, "../../devices/pci", 17));
    yes("class/drm and dev/char lead to one place", realpath(p, real) && realpath(sysnode, real2) && !strcmp(real, real2));
    snprintf(p, sizeof p, "%s/dev", sysnode);
    snprintf(q, sizeof q, "226:%u\n", minor_no);
    yes("dev attribute", slurp(p, t, sizeof t) && !strcmp(t, q));
    snprintf(p, sizeof p, "%s/uevent", sysnode);
    snprintf(q, sizeof q, "MAJOR=226\nMINOR=%u\nDEVNAME=dri/renderD%u\nDEVTYPE=drm_minor\n", minor_no, minor_no);
    yes("minor uevent", slurp(p, t, sizeof t) && !strcmp(t, q));
    snprintf(p, sizeof p, "%s/subsystem", sysnode);
    yes("minor subsystem is /sys/class/drm", realpath(p, real) && !strcmp(real, "/sys/class/drm"));
    snprintf(p, sizeof p, "%s/device/drm/renderD%u", sysnode, minor_no);
    yes("device/drm/renderD<minor> is the node directory", realpath(p, real) && realpath(sysnode, real2) && !strcmp(real, real2));
    yes("class/drm/version", slurp("/sys/class/drm/version", t, sizeof t) && !strcmp(t, "drm 1.1.0 20060810\n"));
    DIR *d = opendir("/sys/class/drm");
    int listed = 0;
    struct dirent *e;
    snprintf(q, sizeof q, "renderD%u", minor_no);
    while (d && (e = readdir(d)))
        listed |= !strcmp(e->d_name, q);
    if (d)
        closedir(d);
    yes("class/drm lists the node", listed);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (!pick_node()) {
        printf("amdgpu render node: none\n");
        return 0;
    }
    printf("amdgpu render node: found\n");
    node_checks();
    version_checks();
    cap_checks();
    refusal_checks();
    file_checks();
    sysfs_checks();
    amdgpu_checks();
    close(fd);
    return 0;
}
