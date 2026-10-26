/* MM-5b: madvise, mremap, mincore, shared anonymous memory and V8's 70 GB reservation through the real syscall
 * path (the kernel-side script crates/mm/abi/mm-ops2.txt covers the same calls in more cases). Modes:
 *   advise | remap | shared | file | msync | reserve <MB to commit>
 * Prints results, errno names and yes/no relations only (no addresses). No signal handler: nothing here faults. */
#include "abi.h"
#include <fcntl.h>
#include <sys/mman.h>

#define P 4096ul

static unsigned char vec[64];

static void core_line(const char *label, void *a, size_t len) {
    long r = syscall(SYS_mincore, a, len, vec);
    if (r) {
        printf("%s = -%s\n", label, ename(errno));
        return;
    }
    printf("%s = ", label);
    for (size_t i = 0; i < (len + P - 1) / P; i++) putchar('0' + (vec[i] & 1));
    putchar('\n');
}

static char *map(size_t len, int prot, int flags) {
    char *m = mmap(0, len, prot, flags, -1, 0);
    if (m == MAP_FAILED) {
        printf("mmap = -%s\n", ename(errno));
        exit(1);
    }
    return m;
}

static void advise(void) {
    char *m = map(8 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);
    for (int i = 0; i < 4; i++) m[i * P] = 0x10 + i;
    core_line("mincore after 4 stores", m, 8 * P);
    SYS("madvise(DONTNEED page 1)", SYS_madvise, m + P, P, MADV_DONTNEED);
    yes("page 1 reads zero", m[P] == 0);
    yes("page 2 kept", m[2 * P] == 0x12);
    SYS("madvise(unaligned)", SYS_madvise, m + 1, P, MADV_DONTNEED);
    SYS("madvise(advice 99)", SYS_madvise, m, P, 99);
    SYS("madvise(len 0, hwpoison)", SYS_madvise, m, 0, MADV_HWPOISON);
    SYS("madvise(hwpoison)", SYS_madvise, m, P, MADV_HWPOISON);
    SYS("munmap(hole)", SYS_munmap, m + 6 * P, 2 * P);
    m[0] = 1;
    SYS("madvise(DONTNEED over a hole)", SYS_madvise, m, 8 * P, MADV_DONTNEED);
    yes("the mapped part was dropped", m[0] == 0);
    SYS("madvise(FREE)", SYS_madvise, m + 3 * P, P, MADV_FREE);
    m[3 * P] = 7;
    yes("a store after FREE stays", m[3 * P] == 7);
    SYS("madvise(DONTFORK)", SYS_madvise, m, 2 * P, MADV_DONTFORK);
    SYS("madvise(WIPEONFORK)", SYS_madvise, m + 2 * P, 2 * P, MADV_WIPEONFORK);
    SYS("madvise(DOFORK)", SYS_madvise, m, 2 * P, MADV_DOFORK);
    SYS("madvise(KEEPONFORK)", SYS_madvise, m + 2 * P, 2 * P, MADV_KEEPONFORK);
    SYS("madvise(WILLNEED)", SYS_madvise, m, 6 * P, MADV_WILLNEED);
    SYS("madvise(REMOVE private)", SYS_madvise, m, P, MADV_REMOVE);
    SYS("madvise(POPULATE_WRITE)", SYS_madvise, m + 4 * P, 2 * P, MADV_POPULATE_WRITE);
    core_line("mincore after populate", m + 4 * P, 2 * P);
    SYS("mprotect(none)", SYS_mprotect, m + 4 * P, 2 * P, PROT_NONE);
    SYS("madvise(POPULATE_READ on PROT_NONE)", SYS_madvise, m + 4 * P, 2 * P, MADV_POPULATE_READ);
    core_line("mincore keeps PROT_NONE pages", m + 4 * P, 2 * P);
    SYS("mprotect(rw)", SYS_mprotect, m + 4 * P, 2 * P, PROT_READ | PROT_WRITE);
    SYS("munmap", SYS_munmap, m, 6 * P);
    core_line("mincore(unmapped)", m, P);
    core_line("mincore(unaligned)", m + 1, P);
}

static void remap(void) {
    char *a = map(2 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);
    a[0] = 0x77;
    a[P] = 0x78;
    char *g = mremap(a, 2 * P, 4 * P, 0);
    yes("grown in place or refused", g == a || g == MAP_FAILED);
    char *b = mremap(a, g == a ? 4 * P : 2 * P, 16 * P, MREMAP_MAYMOVE);
    yes("moved", b != MAP_FAILED);
    yes("content moved", b[0] == 0x77 && b[P] == 0x78);
    yes("the new tail reads zero", b[15 * P] == 0);
    SYS("mremap(unaligned)", SYS_mremap, b + 1, P, 2 * P, MREMAP_MAYMOVE, 0);
    SYS("mremap(bad flag)", SYS_mremap, b, P, 2 * P, 8, 0);
    SYS("mremap(FIXED without MAYMOVE)", SYS_mremap, b, P, 2 * P, MREMAP_FIXED, 0);
    SYS("mremap(new length 0)", SYS_mremap, b, P, 0, MREMAP_MAYMOVE, 0);
    SYS("mremap(old length past the mapping)", SYS_mremap, b, 32 * P, 40 * P, MREMAP_MAYMOVE, 0);
    SYS("mremap(duplicate private)", SYS_mremap, b, 0, P, MREMAP_MAYMOVE, 0);
    char *t = map(4 * P, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS);
    char *f = mremap(b, 2 * P, 2 * P, MREMAP_MAYMOVE | MREMAP_FIXED, t);
    yes("moved onto a mapping (MREMAP_FIXED)", f == t && f[0] == 0x77 && f[P] == 0x78);
    SYS("mremap(FIXED overlapping)", SYS_mremap, t, 2 * P, 2 * P, MREMAP_MAYMOVE | MREMAP_FIXED, t + P);
    SYS("mremap(FIXED below mmap_min_addr)", SYS_mremap, t, P, P, MREMAP_MAYMOVE | MREMAP_FIXED, 0x1000);
    char *d = mremap(t, P, P, MREMAP_MAYMOVE | MREMAP_DONTUNMAP, 0);
    yes("DONTUNMAP moved the page", d != MAP_FAILED && d[0] == 0x77);
    yes("and left the old range empty", t[0] == 0);
    SYS("munmap", SYS_munmap, d, P);
    SYS("munmap", SYS_munmap, t, 4 * P);
    SYS("munmap", SYS_munmap, b + 2 * P, 14 * P);
    /* Hostile lengths near 2^64 (`addr + new_len` would wrap): every one is refused, nothing is unmapped. */
    char *w = map(4 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);
    unsigned long huge = 0xffffffffffffe000ul, task = 0x7ffffffff000ul;
    SYS("mremap(huge shrink)", SYS_mremap, w + 2 * P, huge + P, huge, 0, 0);
    SYS("mremap(huge old length)", SYS_mremap, w + 2 * P, task + P, 2 * P, 0, 0);
    SYS("mremap(new length wraps to 0)", SYS_mremap, w + 2 * P, P, ~0ul, MREMAP_MAYMOVE, 0);
    SYS("mremap(new length past TASK_SIZE, unmapped)", SYS_mremap, 0x100000000000ul, P, task + P, 0, 0);
    SYS("mremap(new length TASK_SIZE)", SYS_mremap, w, P, task, MREMAP_MAYMOVE, 0);
    SYS("mremap(huge growth)", SYS_mremap, w + 2 * P, P, huge, MREMAP_MAYMOVE, 0);
    core_line("mincore after the hostile lengths", w, 4 * P);
    SYS("munmap", SYS_munmap, w, 4 * P);
}

static void shared(void) {
    char *s = map(4 * P, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS);
    s[0] = 0x66;
    SYS("madvise(DONTNEED shared)", SYS_madvise, s, P, MADV_DONTNEED);
    yes("shared data survives DONTNEED", s[0] == 0x66);
    SYS("madvise(FREE shared)", SYS_madvise, s, P, MADV_FREE);
    SYS("madvise(WIPEONFORK shared)", SYS_madvise, s, P, MADV_WIPEONFORK);
    core_line("mincore(shared)", s, 4 * P);
    char *dup = mremap(s, 0, P, MREMAP_MAYMOVE);
    yes("old length 0 duplicates it", dup != MAP_FAILED);
    s[0] = 0x5a;
    yes("the duplicate shows the same page", dup[0] == 0x5a);
    SYS("madvise(REMOVE shared)", SYS_madvise, s, P, MADV_REMOVE);
    yes("REMOVE reads zero after", s[0] == 0 && dup[0] == 0);
    SYS("munmap", SYS_munmap, dup, P);
    SYS("munmap", SYS_munmap, s, 4 * P);
}

/* MADV_REMOVE needs a shared mapping of an fd open for writing (CachyOS: VM_SHARED and VM_MAYWRITE): a read-only
 * fd must never zero the file, with PROT_READ on a writable fd it punches the hole. */
static void file(void) {
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-mm5b-%d", (int)getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    yes("file created", fd >= 0);
    if (fd < 0) return;
    char buf[2 * P];
    memset(buf, 0x3c, sizeof buf);
    yes("written", write(fd, buf, sizeof buf) == (long)sizeof buf);
    int ro = open(path, O_RDONLY);
    char *m = mmap(0, 2 * P, PROT_READ, MAP_SHARED, ro, 0);
    yes("shared read-only mapping of an O_RDONLY fd", m != MAP_FAILED);
    if (m == MAP_FAILED) return;
    SYS("madvise(REMOVE, O_RDONLY fd)", SYS_madvise, m, P, MADV_REMOVE);
    yes("the file is unchanged", m[0] == 0x3c && pread(fd, buf, 1, 0) == 1 && buf[0] == 0x3c);
    SYS("mprotect(write, O_RDONLY fd)", SYS_mprotect, m, P, PROT_READ | PROT_WRITE);
    SYS("munmap", SYS_munmap, m, 2 * P);
    char *p = mmap(0, 2 * P, PROT_READ, MAP_PRIVATE, ro, 0);
    yes("private mapping of an O_RDONLY fd", p != MAP_FAILED);
    SYS("madvise(REMOVE, private)", SYS_madvise, p, P, MADV_REMOVE);
    SYS("munmap", SYS_munmap, p, 2 * P);
    char *w = mmap(0, 2 * P, PROT_READ, MAP_SHARED, fd, 0);
    yes("shared read-only mapping of an O_RDWR fd", w != MAP_FAILED);
    if (w == MAP_FAILED) return;
    SYS("madvise(REMOVE, O_RDWR fd)", SYS_madvise, w, P, MADV_REMOVE);
    yes("page 0 reads zero, page 1 kept", w[0] == 0 && w[P] == 0x3c && pread(fd, buf, 1, 0) == 1 && buf[0] == 0);
    SYS("munmap", SYS_munmap, w, 2 * P);
    close(ro);
    close(fd);
    unlink(path);
}

/* msync's argument checks and holes (the kernel-side twin is ktest mm.as.msync_args). */
static void msync_args(void) {
    char *m = map(4 * P, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS);
    SYS("munmap(page 1)", SYS_munmap, m + P, P);
    SYS("msync(flag 8)", SYS_msync, m, P, 8);
    SYS("msync(unaligned start)", SYS_msync, m + 1, P, MS_ASYNC);
    SYS("msync(ASYNC|SYNC)", SYS_msync, m, P, MS_ASYNC | MS_SYNC);
    SYS("msync(length 0)", SYS_msync, m, 0, MS_SYNC);
    SYS("msync(length 2^64-1)", SYS_msync, m, ~0ul, MS_ASYNC);
    SYS("msync(length 2^64-4096)", SYS_msync, m, ~0ul - 0xfff, MS_ASYNC);
    SYS("msync(hole, ASYNC)", SYS_msync, m, 4 * P, MS_ASYNC);
    SYS("msync(hole, SYNC)", SYS_msync, m, 4 * P, MS_SYNC);
    SYS("msync(only the hole)", SYS_msync, m + P, P, MS_SYNC);
    SYS("msync(hole then mapping)", SYS_msync, m + P, 2 * P, MS_SYNC);
    SYS("msync(SYNC|INVALIDATE)", SYS_msync, m, P, MS_SYNC | MS_INVALIDATE);
    SYS("msync(unaligned length)", SYS_msync, m + 2 * P, 2 * P - 1, MS_SYNC);
    SYS("msync(no flags)", SYS_msync, m + 2 * P, 2 * P - 1, 0);
    SYS("munmap", SYS_munmap, m, 4 * P);
}

/* V8/Chromium: 70 GB reserved with 32 GB alignment, then `mb` MB committed by mprotect and two pages touched. */
static void reserve(long mb) {
    const unsigned long gb = 1ul << 30, res = 70 * gb, align = 32 * gb;
    char *raw = mmap(0, res + align, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    yes("102 GB PROT_NONE reserved", raw != MAP_FAILED);
    if (raw == MAP_FAILED) return;
    char *base = (char *)(((unsigned long)raw + align - 1) & ~(align - 1));
    if (base > raw) SYS("munmap(head)", SYS_munmap, raw, base - raw);
    else puts("munmap(head) = 0");
    SYS("munmap(tail)", SYS_munmap, base + res, raw + res + align - (base + res));
    yes("32 GB aligned", ((unsigned long)base & (align - 1)) == 0);
    core_line("mincore(reserve start)", base, 4 * P);
    unsigned long len = (unsigned long)mb << 20;
    SYS("mprotect(commit)", SYS_mprotect, base, len, PROT_READ | PROT_WRITE);
    /* CachyOS runs transparent huge pages "always": without this the first touch maps 2 MB. */
    SYS("madvise(NOHUGEPAGE)", SYS_madvise, base, len, MADV_NOHUGEPAGE);
    base[0] = 1;
    base[len - 1] = 2;
    yes("both ends usable", base[0] == 1 && base[len - 1] == 2);
    core_line("mincore(first pages)", base, 4 * P);
    SYS("madvise(DONTNEED commit)", SYS_madvise, base, len, MADV_DONTNEED);
    yes("reads zero after", base[0] == 0);
    char *dec = mmap(base, len, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED | MAP_NORESERVE, -1, 0);
    yes("decommitted in place (MAP_FIXED PROT_NONE)", dec == base);
    SYS("munmap(reserve)", SYS_munmap, base, res);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "";
    if (!strcmp(mode, "advise")) advise();
    else if (!strcmp(mode, "remap")) remap();
    else if (!strcmp(mode, "shared")) shared();
    else if (!strcmp(mode, "file")) file();
    else if (!strcmp(mode, "msync")) msync_args();
    else if (!strcmp(mode, "reserve")) reserve(argc > 2 ? atol(argv[2]) : 256);
    else {
        printf("usage: abi-mm5b advise|remap|shared|file|msync|reserve [MB]\n");
        return 2;
    }
    return 0;
}
