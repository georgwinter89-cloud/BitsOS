/* DR-6: GEM objects and PRIME on the amdgpu render node as libdrm and Mesa meet them, without any GPU work:
 * GEM_CREATE's checks and retries (amdgpu_gem.c:278), the create info GEM_OP reports back, GEM_MMAP and the render
 * node's mmap at the fake offset (drm_gem.c:1095: exact start, fits, the file's own objects), GEM_WAIT_IDLE on an
 * idle object, GEM_METADATA, GEM_OP, GEM_USERPTR's argument checks, GEM_CLOSE, and PRIME export/import
 * (drm_prime.c: one file per buffer, the same handle back, the same object in a second file, foreign dma-bufs
 * from /dev/udmabuf). Takes the first amdgpu render node. Prints results, errno names and yes/no relations
 * only: no handles, offsets, inode numbers or chip facts. */
#include "abi.h"
#include <dirent.h>
#include <fcntl.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>

#define IOC(dir, ty, nr, size) ((unsigned long)(dir) << 30 | (unsigned long)(size) << 16 | (unsigned long)(ty) << 8 | (nr))
#define D(dir, nr, size) IOC(dir, 0x64u, nr, size)

struct drm_version { int major, minor, patch, pad; unsigned long name_len; char *name; unsigned long date_len; char *date; unsigned long desc_len; char *desc; };
struct gem_create { uint64_t size, align, domains, flags; };
struct gem_op { uint32_t handle, op; uint64_t value; };
struct gem_md { uint32_t handle, op; uint64_t flags, tiling; uint32_t size; uint32_t data[64]; };
struct gem_wait { uint32_t handle, flags; uint64_t timeout; };
struct gem_userptr { uint64_t addr, size; uint32_t flags, handle; };
struct gem_close { uint32_t handle, pad; };
struct prime { uint32_t handle, flags; int32_t fd; };
struct ub_create { uint32_t memfd, flags; uint64_t offset, size; };

#define VERSION D(3u, 0x00, sizeof(struct drm_version))
#define GEM_CLOSE D(1u, 0x09, 8)
#define GEM_FLINK D(3u, 0x0a, 8)
#define GEM_OPEN D(3u, 0x0b, 16)
#define PRIME_TO_FD D(3u, 0x2d, 12)
#define PRIME_TO_HANDLE D(3u, 0x2e, 12)
#define GEM_CREATE D(3u, 0x40, 32)
#define GEM_MMAP D(3u, 0x41, 8)
#define GEM_METADATA D(3u, 0x46, sizeof(struct gem_md))
#define GEM_WAIT D(3u, 0x47, 16)
#define GEM_OP D(3u, 0x50, 16)
#define GEM_USERPTR D(3u, 0x51, 24)
#define UDMABUF_CREATE IOC(1u, 'u', 0x42, sizeof(struct ub_create))
#define DB_SET_NAME_B IOC(1u, 'b', 1, 8)

#define CPU 1u
#define GTT 2u
#define VRAM 4u
#define GDS 8u
#define F_CPU_ACCESS 1u
#define F_NO_CPU 2u
#define F_USWC 4u
#define F_ALWAYS_VALID 0x40u
#define F_EXPLICIT 0x80u
/* The create flags every amdgpu reports back as given: not VRAM_CLEARED (newer kernels set it on every create)
 * or VRAM_WIPE_ON_RELEASE (added with RAS). Doorbell objects are left out too: 6.6 refuses them, 7.x not. */
#define F_SHOWN 0x10f7ull
#define CLOEXEC 0x80000u
#define RDWR 2u
#define PAGE 4096l

static int fd = -1, fd2 = -1;
static char node[64];

static long io(int f, unsigned long cmd, void *arg) {
    return ioctl(f, cmd, arg) == -1 ? -errno : 0;
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
    fd = open(best, O_RDWR | O_CLOEXEC);
    fd2 = open(best, O_RDWR | O_CLOEXEC);
    return fd >= 0 && fd2 >= 0;
}

/* GEM_CREATE: the result, the handle in *h. */
static long create(int f, uint64_t size, uint64_t align, uint64_t domains, uint64_t flags, uint32_t *h) {
    union { struct gem_create in; uint32_t out[8]; } a = {.in = {size, align, domains, flags}};
    long r = io(f, GEM_CREATE, &a);
    *h = r == 0 ? a.out[0] : 0;
    return r;
}

static long info(int f, uint32_t h, struct gem_create *ci) {
    memset(ci, 0xa5, sizeof *ci);
    struct gem_op o = {h, 0, (uintptr_t)ci};
    return io(f, GEM_OP, &o);
}

static void close_h(int f, uint32_t h) {
    struct gem_close c = {h, 0};
    io(f, GEM_CLOSE, &c);
}

static long offset_of(int f, uint32_t h, uint64_t *off) {
    union { uint32_t in[2]; uint64_t out; } a = {.in = {h, 0}};
    long r = io(f, GEM_MMAP, &a);
    *off = a.out;
    return r;
}

static void create_checks(void) {
    uint32_t h;
    res("CREATE size 0", create(fd, 0, 0, GTT, 0, &h));
    res("CREATE unknown flag 1<<30", create(fd, PAGE, 0, GTT, 1ull << 30, &h));
    res("CREATE unknown domain 0x80", create(fd, PAGE, 0, 0x80, 0, &h));
    res("CREATE domain bit 32", create(fd, PAGE, 0, GTT | 1ull << 32, 0, &h));
    res("CREATE GDS always valid", create(fd, 1, 0, GDS, F_ALWAYS_VALID, &h));
    res("CREATE GDS 2^40 entries", create(fd, 1ull << 40, 0, GDS, 0, &h));
    res("CREATE GTT 2^62", create(fd, 1ull << 62, 0, GTT, 0, &h));
    res("CREATE VRAM 2^62", create(fd, 1ull << 62, 0, VRAM, 0, &h));
    res("CREATE VRAM|GTT 2^62", create(fd, 1ull << 62, 0, VRAM | GTT, 0, &h));

    struct { uint64_t size, align, domains, flags; } ok[] = {
        {1, 0, GTT, 0},
        {PAGE + 1, 1, GTT, F_USWC},
        {3 * PAGE, 3 * PAGE, GTT, F_EXPLICIT},
        {PAGE, 1 << 16, VRAM, F_CPU_ACCESS},
        {PAGE, PAGE, VRAM | GTT, 0},
        {PAGE, 0, CPU, 0},
        {PAGE, 0, 0, 0},
        {PAGE, 0, GTT, F_NO_CPU},
    };
    for (unsigned i = 0; i < sizeof ok / sizeof ok[0]; i++) {
        char l[96];
        snprintf(l, sizeof l, "CREATE size %llu align %llu domains %llu flags %#llx", (unsigned long long)ok[i].size,
                 (unsigned long long)ok[i].align, (unsigned long long)ok[i].domains, (unsigned long long)ok[i].flags);
        long r = create(fd, ok[i].size, ok[i].align, ok[i].domains, ok[i].flags, &h);
        res(l, r);
        if (r)
            continue;
        yes("  handle > 0", h > 0);
        struct gem_create ci;
        long q = info(fd, h, &ci);
        if (q)
            res("  GET_GEM_CREATE_INFO", q);
        else
            printf("  info size=%llu align=%llu domains=%llu flags=%#llx\n", (unsigned long long)ci.size,
                   (unsigned long long)ci.align, (unsigned long long)ci.domains, (unsigned long long)(ci.flags & F_SHOWN));
        close_h(fd, h);
    }
}

/* An address nothing is mapped at. */
static void *unmapped(void) {
    void *p = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p != MAP_FAILED)
        munmap(p, PAGE);
    return p;
}

static void mmap_checks(void) {
    uint32_t a, b, c, other;
    uint64_t oa = 0, ob = 0, oc = 0, oo = 0;
    int empty = open(node, O_RDWR | O_CLOEXEC);
    void *e = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, empty, (off_t)(1ull << 46));
    printf("mmap of a file without objects = %s\n", e == MAP_FAILED ? ename(errno) : "mapped");
    close(empty);
    create(fd, 2 * PAGE, 0, GTT, 0, &a);
    create(fd, PAGE, 0, VRAM, F_CPU_ACCESS, &b);
    create(fd, PAGE, 0, GTT, F_NO_CPU, &c);
    create(fd2, PAGE, 0, GTT, 0, &other);
    res("MMAP GTT object", offset_of(fd, a, &oa));
    res("MMAP VRAM object", offset_of(fd, b, &ob));
    yes("offsets at 4 GiB or above, page-aligned, distinct", oa >= 1ull << 32 && ob >= 1ull << 32 && !(oa & 4095) && !(ob & 4095) && oa != ob);
    res("MMAP no CPU access", offset_of(fd, c, &oc));
    yes("MMAP no CPU access gives offset 0", oc == 0);
    res("MMAP unknown handle", offset_of(fd, 0x7fff0000, &oc));
    yes("MMAP unknown handle gives 0", oc == 0);
    offset_of(fd2, other, &oo);

    char *p = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)oa);
    yes("mmap of the GTT object", p != MAP_FAILED);
    if (p != MAP_FAILED) {
        yes("new object reads zero", p[0] == 0 && p[2 * PAGE - 1] == 0);
        memset(p, 0x3c, 2 * PAGE);
        struct gem_create ci;
        info(fd, a, &ci);
        yes("a CPU fault remembers CPU access", ci.flags & F_CPU_ACCESS);
        char *q = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, (off_t)oa);
        yes("a second mapping sees the bytes", q != MAP_FAILED && q[PAGE - 1] == 0x3c);
        if (q != MAP_FAILED)
            munmap(q, PAGE);
        munmap(p, 2 * PAGE);
    }
    char *v = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)ob);
    yes("mmap of the VRAM object", v != MAP_FAILED);
    if (v != MAP_FAILED) {
        v[7] = 9;
        yes("VRAM object keeps a byte", v[7] == 9);
        munmap(v, PAGE);
    }
    void *x = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, (off_t)(oa + PAGE));
    printf("mmap inside an object = %s\n", x == MAP_FAILED ? ename(errno) : "mapped");
    x = mmap(NULL, 3 * PAGE, PROT_READ, MAP_SHARED, fd, (off_t)oa);
    printf("mmap past an object = %s\n", x == MAP_FAILED ? ename(errno) : "mapped");
    x = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, (off_t)oo);
    printf("mmap of another file's object = %s\n", x == MAP_FAILED ? ename(errno) : "mapped");
    x = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, (off_t)(1ull << 46));
    printf("mmap of an offset no object has = %s\n", x == MAP_FAILED ? ename(errno) : "mapped");

    struct gem_wait w = {a, 0, 0};
    res("WAIT_IDLE idle, timeout 0", io(fd, GEM_WAIT, &w));
    printf("WAIT_IDLE status=%u domain=%u\n", w.handle, w.flags);
    w = (struct gem_wait){a, 0, ~0ull};
    res("WAIT_IDLE idle, no timeout", io(fd, GEM_WAIT, &w));
    printf("WAIT_IDLE status=%u\n", w.handle);
    w = (struct gem_wait){0x7fff0000, 0, 0};
    res("WAIT_IDLE unknown handle", io(fd, GEM_WAIT, &w));
    close_h(fd, a);
    close_h(fd, b);
    close_h(fd, c);
    close_h(fd2, other);
}

static void metadata_checks(void) {
    uint32_t h;
    create(fd, PAGE, 0, GTT, 0, &h);
    struct gem_md m;
    memset(&m, 0, sizeof m);
    m.handle = 0x7fff0000;
    m.op = 2;
    res("METADATA unknown handle", io(fd, GEM_METADATA, &m));
    memset(&m, 0x5a, sizeof m);
    m.handle = h;
    m.op = 2;
    res("METADATA GET fresh", io(fd, GEM_METADATA, &m));
    printf("METADATA fresh tiling=%#llx size=%u flags=%#llx data0=%#x\n", (unsigned long long)m.tiling, m.size,
           (unsigned long long)m.flags, m.data[0]);
    m = (struct gem_md){.handle = h, .op = 1, .flags = 5, .tiling = 0x1234ull << 40 | 27, .size = 300};
    res("METADATA SET 300 bytes", io(fd, GEM_METADATA, &m));
    m.size = 6;
    m.data[0] = 0x11223344;
    m.data[1] = 0x55667788;
    res("METADATA SET 6 bytes", io(fd, GEM_METADATA, &m));
    memset(&m, 0x5a, sizeof m);
    m.handle = h;
    m.op = 2;
    res("METADATA GET", io(fd, GEM_METADATA, &m));
    printf("METADATA tiling=%#llx size=%u flags=%#llx data0=%#x data1=%#x data2=%#x\n", (unsigned long long)m.tiling,
           m.size, (unsigned long long)m.flags, m.data[0], m.data[1], m.data[2]);
    m = (struct gem_md){.handle = h, .op = 1, .flags = 9, .tiling = 0, .size = 0};
    res("METADATA SET 0 bytes", io(fd, GEM_METADATA, &m));
    memset(&m, 0, sizeof m);
    m.handle = h;
    m.op = 2;
    io(fd, GEM_METADATA, &m);
    printf("METADATA after clear size=%u flags=%#llx tiling=%#llx\n", m.size, (unsigned long long)m.flags, (unsigned long long)m.tiling);
    m.op = 7;
    res("METADATA unknown op", io(fd, GEM_METADATA, &m));

    struct gem_op o = {0x7fff0000, 0, 0};
    res("OP unknown handle", io(fd, GEM_OP, &o));
    o = (struct gem_op){h, 9, 0};
    res("OP unknown op", io(fd, GEM_OP, &o));
    o = (struct gem_op){h, 0, (uintptr_t)KERNEL_ADDR};
    res("OP GET_GEM_CREATE_INFO kernel pointer", io(fd, GEM_OP, &o));
    void *gone = unmapped();
    o = (struct gem_op){h, 0, (uintptr_t)gone};
    res("OP GET_GEM_CREATE_INFO unmapped pointer", io(fd, GEM_OP, &o));
    res("METADATA unmapped argument", io(fd, GEM_METADATA, gone));
    res("OP unmapped argument", io(fd, GEM_OP, gone));
    struct gem_create ci;
    o = (struct gem_op){h, 1, VRAM};
    res("OP SET_PLACEMENT VRAM", io(fd, GEM_OP, &o));
    info(fd, h, &ci);
    printf("OP after SET_PLACEMENT VRAM domains=%llu\n", (unsigned long long)ci.domains);
    o = (struct gem_op){h, 1, 0xff};
    res("OP SET_PLACEMENT 0xff", io(fd, GEM_OP, &o));
    info(fd, h, &ci);
    printf("OP after SET_PLACEMENT 0xff domains=%llu\n", (unsigned long long)ci.domains);

    struct gem_userptr u = {0x10001, PAGE, 1, 0};
    res("USERPTR unaligned address", io(fd, GEM_USERPTR, &u));
    u = (struct gem_userptr){0x10000, 100, 1, 0};
    res("USERPTR unaligned size", io(fd, GEM_USERPTR, &u));
    u = (struct gem_userptr){0x10000, PAGE, 0x10, 0};
    res("USERPTR unknown flag", io(fd, GEM_USERPTR, &u));
    u = (struct gem_userptr){0x10000, PAGE, 0, 0};
    res("USERPTR writable without REGISTER", io(fd, GEM_USERPTR, &u));
    u = (struct gem_userptr){0x10000, 0, 1, 0};
    res("USERPTR size 0", io(fd, GEM_USERPTR, &u));

    struct gem_close c = {h, 0};
    res("GEM_CLOSE", io(fd, GEM_CLOSE, &c));
    res("GEM_CLOSE again", io(fd, GEM_CLOSE, &c));
    res("METADATA after close", io(fd, GEM_METADATA, &m));
}

static long to_fd(int f, uint32_t h, uint32_t flags, int *out) {
    struct prime p = {h, flags, -1};
    long r = io(f, PRIME_TO_FD, &p);
    *out = p.fd;
    return r;
}

static long to_handle(int f, int buf, uint32_t *h) {
    struct prime p = {0, 0, buf};
    long r = io(f, PRIME_TO_HANDLE, &p);
    *h = p.handle;
    return r;
}

static void fdinfo_name(int b) {
    char path[64], line[256];
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", b);
    FILE *f = fopen(path, "r");
    if (!f)
        return;
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, "exp_name:", 9) || !strncmp(line, "size:", 5))
            printf("dma-buf fdinfo %s", line);
    fclose(f);
}

static void prime_checks(void) {
    uint32_t h, h2, back, other;
    int b1 = -1, b2 = -1, b3 = -1;
    create(fd, 3 * PAGE, 0, GTT, 0, &h);
    res("PRIME_HANDLE_TO_FD bad flags", to_fd(fd, h, 1, &b1));
    res("PRIME_HANDLE_TO_FD unknown handle", to_fd(fd, 0x7fff0000, CLOEXEC | RDWR, &b1));
    res("PRIME_HANDLE_TO_FD", to_fd(fd, h, CLOEXEC | RDWR, &b1));
    yes("fd >= 0, close-on-exec", b1 >= 0 && (fcntl(b1, F_GETFD) & FD_CLOEXEC));
    yes("lseek SEEK_END is the size", lseek(b1, 0, SEEK_END) == 3 * PAGE && lseek(b1, 0, SEEK_SET) == 0);
    res("PRIME_HANDLE_TO_FD again (no CLOEXEC)", to_fd(fd, h, RDWR, &b2));
    struct stat s1, s2;
    yes("two fds, one buffer (same inode)", b2 >= 0 && b2 != b1 && fstat(b1, &s1) == 0 && fstat(b2, &s2) == 0 && s1.st_ino == s2.st_ino);
    yes("second fd without close-on-exec", b2 >= 0 && !(fcntl(b2, F_GETFD) & FD_CLOEXEC));
    yes("open for reading and writing", (fcntl(b1, F_GETFL) & O_ACCMODE) == O_RDWR);
    fdinfo_name(b1);
    res("SET_NAME_B", io(b1, DB_SET_NAME_B, "abi"));
    struct pollfd pf = {b1, POLLIN | POLLOUT, 0};
    res("poll idle buffer", poll(&pf, 1, 0));
    printf("poll revents=%#x\n", pf.revents);

    res("PRIME_FD_TO_HANDLE own export", to_handle(fd, b1, &back));
    yes("the same handle back", back == h);
    res("PRIME_FD_TO_HANDLE second fd", to_handle(fd, b2, &back));
    yes("the same handle again", back == h);
    res("PRIME_FD_TO_HANDLE in a second file", to_handle(fd2, b1, &other));
    struct gem_create ci;
    yes("the second file sees the object", info(fd2, other, &ci) == 0 && ci.size == 3 * PAGE && ci.domains == GTT);
    res("PRIME_FD_TO_HANDLE in the second file again", to_handle(fd2, b2, &h2));
    yes("same handle in the second file", h2 == other);

    char *p = mmap(NULL, 3 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, b1, 0);
    yes("mmap of the dma-buf", p != MAP_FAILED);
    uint64_t off = 0;
    offset_of(fd2, other, &off);
    char *q = mmap(NULL, 3 * PAGE, PROT_READ, MAP_SHARED, fd2, (off_t)off);
    yes("mmap of the object through the second file", q != MAP_FAILED);
    if (p != MAP_FAILED && q != MAP_FAILED) {
        p[2 * PAGE + 5] = 0x77;
        yes("both mappings are one memory", q[2 * PAGE + 5] == 0x77);
    }
    if (q != MAP_FAILED)
        munmap(q, 3 * PAGE);

    close_h(fd, h);
    close_h(fd2, other);
    res("PRIME_FD_TO_HANDLE after every handle went", to_handle(fd, b1, &back));
    yes("a handle again", back > 0);
    struct gem_wait w = {back, 0, 0};
    res("WAIT_IDLE on the imported object", io(fd, GEM_WAIT, &w));
    if (p != MAP_FAILED) {
        uint64_t o2 = 0;
        offset_of(fd, back, &o2);
        char *r = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, fd, (off_t)(o2));
        yes("the object kept its bytes", r != MAP_FAILED && r[0] == 0 && p[2 * PAGE + 5] == 0x77);
        if (r != MAP_FAILED)
            munmap(r, PAGE);
        munmap(p, 3 * PAGE);
    }
    close_h(fd, back);
    close(b1);
    close(b2);

    /* Read-only export: the buffer's file is read-only, writable maps are refused. */
    create(fd, PAGE, 0, GTT, 0, &h);
    res("PRIME_HANDLE_TO_FD read-only", to_fd(fd, h, CLOEXEC, &b3));
    yes("open for reading only", b3 >= 0 && (fcntl(b3, F_GETFL) & O_ACCMODE) == O_RDONLY);
    void *x = mmap(NULL, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, b3, 0);
    printf("writable mmap of a read-only export = %s\n", x == MAP_FAILED ? ename(errno) : "mapped");
    int b4 = -1;
    to_fd(fd, h, CLOEXEC | RDWR, &b4);
    yes("a later RDWR export shares the read-only file", b4 >= 0 && (fcntl(b4, F_GETFL) & O_ACCMODE) == O_RDONLY);
    close(b4);
    close(b3);
    close_h(fd, h);

    create(fd, PAGE, 0, GTT, F_ALWAYS_VALID, &h);
    res("PRIME_HANDLE_TO_FD always-valid object", to_fd(fd, h, CLOEXEC | RDWR, &b3));
    close_h(fd, h);
    create(fd, PAGE, 0, GTT, F_NO_CPU, &h);
    res("PRIME_HANDLE_TO_FD no-CPU object", to_fd(fd, h, CLOEXEC | RDWR, &b3));
    x = mmap(NULL, PAGE, PROT_READ, MAP_SHARED, b3, 0);
    printf("mmap of a no-CPU export = %s\n", x == MAP_FAILED ? ename(errno) : "mapped");
    close(b3);
    close_h(fd, h);

    res("PRIME_FD_TO_HANDLE fd -1", to_handle(fd, -1, &back));
    int null = open("/dev/null", O_RDWR | O_CLOEXEC);
    res("PRIME_FD_TO_HANDLE /dev/null", to_handle(fd, null, &back));
    close(null);
    int pp[2];
    if (pipe(pp) == 0) {
        res("PRIME_FD_TO_HANDLE pipe", to_handle(fd, pp[0], &back));
        close(pp[0]);
        close(pp[1]);
    }

    /* A foreign dma-buf: a udmabuf over a sealed memfd. */
    int u = open("/dev/udmabuf", O_RDWR | O_CLOEXEC);
    int m = memfd_create("abi-drm-gem", MFD_ALLOW_SEALING);
    int ub = -1;
    if (u >= 0 && m >= 0 && ftruncate(m, 2 * PAGE) == 0 && fcntl(m, F_ADD_SEALS, F_SEAL_SHRINK) == 0) {
        struct ub_create c = {(uint32_t)m, 1, 0, 2 * PAGE};
        ub = ioctl(u, UDMABUF_CREATE, &c);
    }
    if (ub < 0) {
        printf("udmabuf: none\n");
    } else {
        res("PRIME_FD_TO_HANDLE udmabuf", to_handle(fd, ub, &h));
        res("PRIME_FD_TO_HANDLE udmabuf again", to_handle(fd, ub, &back));
        yes("same handle for the udmabuf", h == back);
        long q = info(fd, h, &ci);
        if (q)
            res("udmabuf object info", q);
        else
            printf("udmabuf object size=%llu align=%llu domains=%llu flags=%#llx\n", (unsigned long long)ci.size,
                   (unsigned long long)ci.align, (unsigned long long)ci.domains, (unsigned long long)(ci.flags & F_SHOWN));
        struct gem_op o = {h, 1, VRAM};
        res("SET_PLACEMENT VRAM on an import", io(fd, GEM_OP, &o));
        o = (struct gem_op){h, 1, GTT};
        res("SET_PLACEMENT GTT on an import", io(fd, GEM_OP, &o));
        res("PRIME_HANDLE_TO_FD of the import", to_fd(fd, h, CLOEXEC | RDWR, &b3));
        struct stat a, b;
        yes("re-export gives the udmabuf itself", b3 >= 0 && fstat(b3, &a) == 0 && fstat(ub, &b) == 0 && a.st_ino == b.st_ino);
        if (b3 >= 0)
            fdinfo_name(b3);
        close(b3);
        close_h(fd, h);
        close(ub);
    }
    if (m >= 0)
        close(m);
    if (u >= 0)
        close(u);
}

/* GEM names are DRM_AUTH without RENDER_ALLOW: a render node refuses both (drm_ioctl.c). */
static void name_checks(void) {
    uint32_t h;
    create(fd, PAGE, 0, GTT, 0, &h);
    struct { uint32_t handle, name; } fl = {h, 0};
    res("GEM_FLINK on a render node", io(fd, GEM_FLINK, &fl));
    struct { uint32_t name, handle; uint64_t size; } op = {1, 0, 0};
    res("GEM_OPEN on a render node", io(fd, GEM_OPEN, &op));
    close_h(fd, h);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (!pick_node()) {
        printf("amdgpu render node: none\n");
        return 0;
    }
    printf("amdgpu render node: found\n");
    create_checks();
    mmap_checks();
    metadata_checks();
    prime_checks();
    name_checks();
    close(fd2);
    close(fd);
    return 0;
}
