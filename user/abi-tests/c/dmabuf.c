/* DR-5: dma-buf fds as a Wayland client and a compositor meet them before any GPU renders: /dev/udmabuf turns a
 * sealed memfd into a dma-buf (lavapipe's way), and the dma-buf fd answers fstat, lseek (the size), mmap (the
 * memfd's own pages), poll, DMA_BUF_IOCTL_SYNC, SET_NAME, EXPORT/IMPORT_SYNC_FILE and fdinfo. Prints results and
 * errno names only (no fd numbers, inode numbers or device numbers). Argument "mmap": only the mapping cases
 * (the memfd and the buffer share their pages, MAP_PRIVATE refused, a list buffer's pages, the pages outlive the
 * memfd). */
#include "abi.h"
#include <fcntl.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>

struct ub_create { uint32_t memfd, flags; uint64_t offset, size; };
struct ub_item { uint32_t memfd, pad; uint64_t offset, size; };
struct ub_list { uint32_t flags, count; struct ub_item list[3]; };
struct db_sync { uint64_t flags; };
struct db_sync_file { uint32_t flags; int32_t fd; };
struct sync_file_info { char name[32]; int32_t status; uint32_t flags, num_fences, pad; uint64_t info; };

#define IOC(dir, ty, nr, size) ((unsigned long)(dir) << 30 | (unsigned long)(size) << 16 | (unsigned long)(ty) << 8 | (nr))
#define UDMABUF_CREATE IOC(1u, 'u', 0x42, sizeof(struct ub_create))
#define UDMABUF_CREATE_LIST IOC(1u, 'u', 0x43, 8)
#define DB_SYNC IOC(1u, 'b', 0, sizeof(struct db_sync))
#define DB_SET_NAME_A IOC(1u, 'b', 1, 4)
#define DB_SET_NAME_B IOC(1u, 'b', 1, 8)
#define DB_EXPORT IOC(3u, 'b', 2, sizeof(struct db_sync_file))
#define DB_IMPORT IOC(1u, 'b', 3, sizeof(struct db_sync_file))
#define SYNC_INFO IOC(3u, 0x3eu, 4, sizeof(struct sync_file_info))

#define SYNC_READ 1u
#define SYNC_WRITE 2u
#define SYNC_RW 3u
#define SYNC_END 4u
#define CLOEXEC 1u
#define PAGE 4096l

static const char *en(int e) {
    return e == EBADFD ? "EBADFD" : ename(e);
}

static long res(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, en(errno));
    else
        printf("%s = ok\n", label);
    return r;
}

static void val(const char *label, long r) {
    if (r < 0)
        printf("%s = -%s\n", label, en(errno));
    else
        printf("%s = %ld\n", label, r);
}

static int memfd(long pages, unsigned seals) {
    int m = memfd_create("abi-dmabuf", MFD_ALLOW_SEALING);
    if (m < 0 || ftruncate(m, pages * PAGE) != 0)
        return -1;
    if (seals && fcntl(m, F_ADD_SEALS, seals) != 0)
        return -1;
    return m;
}

static long create(int dev, int fd, unsigned flags, uint64_t off, uint64_t size) {
    struct ub_create c = { (uint32_t)fd, flags, off, size };
    return ioctl(dev, UDMABUF_CREATE, &c);
}

/* The fdinfo lines dma_buf_show_fdinfo adds (pos/flags/mnt_id/ino are the VFS's and left out). */
static void fdinfo(int fd) {
    char path[64], buf[512];
    snprintf(path, sizeof path, "/proc/self/fdinfo/%d", fd);
    FILE *f = fopen(path, "r");
    if (!f) {
        printf("fdinfo = -%s\n", en(errno));
        return;
    }
    while (fgets(buf, sizeof buf, f))
        if (!strncmp(buf, "size:", 5) || !strncmp(buf, "count:", 6) || !strncmp(buf, "exp_name:", 9) || !strncmp(buf, "name:", 5))
            printf("fdinfo %s", buf);
    fclose(f);
}

static void link_of(const char *label, int fd) {
    char path[64], buf[128];
    snprintf(path, sizeof path, "/proc/self/fd/%d", fd);
    ssize_t n = readlink(path, buf, sizeof buf - 1);
    if (n < 0) {
        printf("%s = -%s\n", label, en(errno));
        return;
    }
    buf[n] = 0;
    printf("%s = %s\n", label, buf);
}

static void sync_call(int buf, const char *label, uint64_t flags) {
    struct db_sync s = { flags };
    res(label, ioctl(buf, DB_SYNC, &s));
}

static void errors(int dev) {
    int plain = memfd(4, 0);
    int write_sealed = memfd(4, F_SEAL_SHRINK | F_SEAL_WRITE);
    int ok = memfd(4, F_SEAL_SHRINK);
    int p[2];
    if (plain < 0 || write_sealed < 0 || ok < 0 || pipe(p) != 0) {
        printf("setup = -%s\n", en(errno));
        return;
    }
    res("create without F_SEAL_SHRINK", create(dev, plain, 0, 0, 4 * PAGE));
    res("create with F_SEAL_WRITE", create(dev, write_sealed, 0, 0, 4 * PAGE));
    res("create offset unaligned", create(dev, ok, 0, 1, PAGE));
    res("create size unaligned", create(dev, ok, 0, 0, PAGE + 1));
    res("create size 0", create(dev, ok, 0, 0, 0));
    res("create past the memfd's end", create(dev, ok, 0, 3 * PAGE, 2 * PAGE));
    res("create from a pipe", create(dev, p[0], 0, 0, PAGE));
    res("create from fd -1", create(dev, -1, 0, 0, PAGE));
    res("create from a closed fd", create(dev, 1000, 0, 0, PAGE));
    res("create bad pointer", ioctl(dev, UDMABUF_CREATE, KERNEL_ADDR));
    res("udmabuf unknown ioctl", ioctl(dev, IOC(1u, 'u', 0x44, 8), 0));
    struct ub_list l = { 0, 0, { { 0 } } };
    res("create_list count 0", ioctl(dev, UDMABUF_CREATE_LIST, &l));
    close(plain);
    close(write_sealed);
    close(ok);
    close(p[0]);
    close(p[1]);
}

static void file_calls(int buf) {
    struct stat st;
    if (fstat(buf, &st) == 0)
        printf("fstat mode = 0%o, size = %ld, blocks = %ld, nlink = %ld\n", st.st_mode, (long)st.st_size, (long)st.st_blocks, (long)st.st_nlink);
    else
        printf("fstat = -%s\n", en(errno));
    val("fd flags", fcntl(buf, F_GETFD));
    val("fl accmode", fcntl(buf, F_GETFL) & O_ACCMODE);
    val("lseek SEEK_END 0", lseek(buf, 0, SEEK_END));
    val("lseek SEEK_SET 0", lseek(buf, 0, SEEK_SET));
    val("lseek SEEK_CUR 0", lseek(buf, 0, SEEK_CUR));
    val("lseek SEEK_END 1", lseek(buf, 1, SEEK_END));
    val("lseek SEEK_SET 4096", lseek(buf, 4096, SEEK_SET));
    char c;
    val("read", read(buf, &c, 1));
    val("write", write(buf, &c, 1));
    link_of("link", buf);
    fdinfo(buf);
}

static void mapping(int buf, int m) {
    char *a = mmap(0, 4 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, buf, 0);
    if (a == MAP_FAILED) {
        printf("mmap shared = -%s\n", en(errno));
    } else {
        printf("mmap shared = ok\n");
        memcpy(a + PAGE + 5, "through-dmabuf", 14);
        char got[16] = { 0 };
        pread(m, got, 14, PAGE + 5);
        printf("memfd sees the dma-buf write = %s\n", memcmp(got, "through-dmabuf", 14) ? "no" : "yes");
        pwrite(m, "through-memfd", 13, 3 * PAGE);
        printf("dma-buf sees the memfd write = %s\n", memcmp(a + 3 * PAGE, "through-memfd", 13) ? "no" : "yes");
        munmap(a, 4 * PAGE);
    }
    void *b = mmap(0, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE, buf, 0);
    printf("mmap private = %s\n", b == MAP_FAILED ? en(errno) : "ok");
    b = mmap(0, 4 * PAGE, PROT_READ, MAP_SHARED, buf, PAGE);
    printf("mmap past the end = %s\n", b == MAP_FAILED ? en(errno) : "ok");
    b = mmap(0, PAGE, PROT_READ, MAP_SHARED, buf, 3 * PAGE);
    printf("mmap last page = %s\n", b == MAP_FAILED ? en(errno) : "ok");
    if (b != MAP_FAILED)
        munmap(b, PAGE);
}

static void syncs(int buf) {
    sync_call(buf, "sync end before start", SYNC_END | SYNC_RW);
    sync_call(buf, "sync flags 0", 0);
    sync_call(buf, "sync flags END only", SYNC_END);
    sync_call(buf, "sync unknown flag", SYNC_RW | 8);
    sync_call(buf, "sync start rw", SYNC_RW);
    sync_call(buf, "sync end rw", SYNC_END | SYNC_RW);
    sync_call(buf, "sync start read", SYNC_READ);
    sync_call(buf, "sync end write", SYNC_END | SYNC_WRITE);
    res("sync bad pointer", ioctl(buf, DB_SYNC, KERNEL_ADDR));
    res("dma-buf unknown ioctl", ioctl(buf, IOC(1u, 'b', 9, 8), 0));
}

static void fences(int buf) {
    struct pollfd pf = { buf, POLLIN | POLLOUT, 0 };
    int n = poll(&pf, 1, 0);
    printf("poll = %d, revents = 0x%x\n", n, pf.revents);
    struct db_sync_file e = { 0, -1 };
    res("export flags 0", ioctl(buf, DB_EXPORT, &e));
    e.flags = SYNC_RW | 4;
    res("export flags 7", ioctl(buf, DB_EXPORT, &e));
    res("export bad pointer", ioctl(buf, DB_EXPORT, KERNEL_ADDR));
    e.flags = SYNC_WRITE;
    if (res("export write", ioctl(buf, DB_EXPORT, &e)) == 0) {
        val("export fd flags", fcntl(e.fd, F_GETFD));
        struct sync_file_info info = { { 0 } };
        res("export sync_file info", ioctl(e.fd, SYNC_INFO, &info));
        printf("export status = %d, num_fences = %u\n", info.status, info.num_fences);
        struct pollfd sp = { e.fd, POLLIN, 0 };
        printf("export poll = %d\n", poll(&sp, 1, 0));
        struct db_sync_file i = { SYNC_READ, e.fd };
        res("import read", ioctl(buf, DB_IMPORT, &i));
        i.flags = SYNC_WRITE;
        res("import write", ioctl(buf, DB_IMPORT, &i));
        i.flags = 0;
        res("import flags 0", ioctl(buf, DB_IMPORT, &i));
        i.flags = 8 | SYNC_READ;
        res("import flags 9", ioctl(buf, DB_IMPORT, &i));
        close(e.fd);
    }
    struct db_sync_file i = { SYNC_RW, -1 };
    res("import fd -1", ioctl(buf, DB_IMPORT, &i));
    i.fd = buf;
    res("import a dma-buf fd", ioctl(buf, DB_IMPORT, &i));
    pf.revents = 0;
    n = poll(&pf, 1, 0);
    printf("poll after import = %d, revents = 0x%x\n", n, pf.revents);
}

static void names(int buf) {
    res("set_name_b", ioctl(buf, DB_SET_NAME_B, "abi-name"));
    link_of("link named", buf);
    fdinfo(buf);
    res("set_name_a", ioctl(buf, DB_SET_NAME_A, "second"));
    fdinfo(buf);
    res("set_name bad pointer", ioctl(buf, DB_SET_NAME_B, KERNEL_ADDR));
    /* strndup_user(buf, DMA_BUF_NAME_LEN): 32 bytes without a NUL are EINVAL, 31 and the NUL fit. */
    char longname[40];
    memset(longname, 'n', sizeof longname);
    res("set_name 32 bytes no NUL", ioctl(buf, DB_SET_NAME_B, longname));
    longname[31] = 0;
    res("set_name 31 bytes", ioctl(buf, DB_SET_NAME_B, longname));
    link_of("link long name", buf);
}

static void list(int dev, int maps) {
    int m1 = memfd(2, F_SEAL_SHRINK), m2 = memfd(3, F_SEAL_SHRINK | F_SEAL_GROW);
    if (m1 < 0 || m2 < 0) {
        printf("list setup = -%s\n", en(errno));
        return;
    }
    pwrite(m1, "first", 5, PAGE);
    pwrite(m2, "second", 6, 2 * PAGE);
    struct ub_list l = { CLOEXEC, 2, { { (uint32_t)m1, 0, PAGE, PAGE }, { (uint32_t)m2, 0, PAGE, 2 * PAGE } } };
    int b = ioctl(dev, UDMABUF_CREATE_LIST, &l);
    if (res("create_list", b) < 0)
        return;
    close(m1);
    close(m2);
    val("list size", lseek(b, 0, SEEK_END));
    if (!maps) {
        close(b);
        return;
    }
    char *a = mmap(0, 3 * PAGE, PROT_READ, MAP_SHARED, b, 0);
    if (a == MAP_FAILED) {
        printf("list mmap = -%s\n", en(errno));
    } else {
        printf("list page 0 = %.5s, page 2 = %.6s\n", a, a + 2 * PAGE);
        munmap(a, 3 * PAGE);
    }
    close(b);
}

int main(int argc, char **argv) {
    int maps = argc > 1 && !strcmp(argv[1], "mmap");
    int dev = open("/dev/udmabuf", O_RDWR | O_CLOEXEC);
    if (dev < 0) {
        printf("open /dev/udmabuf = -%s\n", en(errno));
        return 0;
    }
    struct stat st;
    printf("udmabuf is a char device = %s\n", fstat(dev, &st) == 0 && S_ISCHR(st.st_mode) ? "yes" : "no");
    if (!maps)
        errors(dev);
    int m = memfd(4, F_SEAL_SHRINK);
    int buf = (int)create(dev, m, 0, 0, 4 * PAGE);
    if (res("create", buf) < 0)
        return 0;
    if (maps) {
        mapping(buf, m);
        list(dev, 1);
        close(m);
        char *a = mmap(0, PAGE, PROT_READ, MAP_SHARED, buf, PAGE);
        printf("after the memfd closed: %s\n", a == MAP_FAILED ? en(errno) : (memcmp(a + 5, "through-dmabuf", 14) ? "other data" : "same pages"));
        return 0;
    }
    int buf2 = (int)create(dev, m, CLOEXEC, PAGE, PAGE);
    res("create cloexec at offset", buf2);
    if (buf2 >= 0) {
        val("cloexec fd flags", fcntl(buf2, F_GETFD));
        struct stat a, b;
        fstat(buf, &a);
        fstat(buf2, &b);
        printf("two buffers have two inodes = %s\n", a.st_ino != b.st_ino && a.st_dev == b.st_dev ? "yes" : "no");
        close(buf2);
    }
    file_calls(buf);
    syncs(buf);
    fences(buf);
    names(buf);
    list(dev, 0);
    close(m);
    close(buf);
    close(dev);
    return 0;
}
