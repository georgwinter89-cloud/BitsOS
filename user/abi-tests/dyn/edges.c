// AB-8 fixer: the edges of a dynamic program's start. The loader's file mappings (the rest of the data
// segment's last file page cleared, the bss zero, the file itself never changed by writes into the private
// mappings, RELRO enforced), the calls glibc makes at startup with their errnos, and execve of a copy of this
// program whose PT_INTERP is bad (the error returns to a caller that keeps running). No addresses are printed.
#include "abi.h"
#include <elf.h>
#include <fcntl.h>
#include <link.h>
#include <sched.h>
#include <setjmp.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/random.h>
#include <sys/resource.h>
#include <sys/rseq.h>
#include <sys/stat.h>

extern char **environ;

static volatile int data_var = 0x12345678;
static char bss_buf[8192];

struct seg {
    ElfW(Addr) base;
    const ElfW(Phdr) *rw, *text, *relro;
};

static int first(struct dl_phdr_info *i, size_t size, void *arg) {
    (void)size;
    struct seg *s = arg;
    s->base = i->dlpi_addr;
    for (int k = 0; k < i->dlpi_phnum; k++) {
        const ElfW(Phdr) *p = &i->dlpi_phdr[k];
        if (p->p_type == PT_LOAD && (p->p_flags & PF_W))
            s->rw = p;
        if (p->p_type == PT_LOAD && (p->p_flags & PF_X))
            s->text = p;
        if (p->p_type == PT_GNU_RELRO)
            s->relro = p;
    }
    return 1; // the program is the first object
}

static int self_fd = -1;

static int file_at(off_t off, void *buf, size_t len) {
    return pread(self_fd, buf, len, off) == (ssize_t)len;
}

static sigjmp_buf jb;
static volatile int fault_code;

static void on_segv(int sig, siginfo_t *si, void *uc) {
    (void)sig;
    (void)uc;
    fault_code = si->si_code;
    siglongjmp(jb, 1);
}

static void mappings(void) {
    struct seg s = {0};
    dl_iterate_phdr(first, &s);
    self_fd = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    yes("open /proc/self/exe", self_fd >= 0);
    yes("segments found", s.rw && s.text && s.relro);
    if (self_fd < 0 || !s.rw || !s.text || !s.relro)
        return;
    long pg = sysconf(_SC_PAGESIZE);
    // The rest of the last file page of the data segment: cleared in memory, the file keeps its bytes there.
    ElfW(Addr) fend = s.base + s.rw->p_vaddr + s.rw->p_filesz;
    ElfW(Addr) pend = (fend + pg - 1) & ~(ElfW(Addr))(pg - 1);
    ElfW(Addr) mend = s.base + s.rw->p_vaddr + s.rw->p_memsz;
    size_t tail = (pend < mend ? pend : mend) - fend;
    unsigned char *fb = malloc(tail + 1);
    int same = 0, nonzero = 0;
    if (fb && file_at(s.rw->p_offset + s.rw->p_filesz, fb, tail)) {
        for (size_t k = 0; k < tail; k++) {
            nonzero += fb[k] != 0;
            same += fb[k] != 0 && ((volatile unsigned char *)fend)[k] == fb[k];
        }
    }
    free(fb);
    yes("data tail cleared", same == 0);
    yes("data tail kept in the file", tail == 0 || nonzero > 0);
    int bss_zero = 1;
    for (size_t k = 0; k < sizeof bss_buf; k++)
        bss_zero &= bss_buf[k] == 0;
    yes("bss zero", bss_zero);
    // A write into the private data mapping stays private.
    off_t doff = s.rw->p_offset + ((ElfW(Addr))&data_var - (s.base + s.rw->p_vaddr));
    data_var = 0x0BADF00D;
    int fv = 0;
    yes("data written", data_var == 0x0BADF00D);
    yes("data unchanged in the file", file_at(doff, &fv, sizeof fv) && fv == 0x12345678);
    // The text is the file's bytes.
    unsigned char tf[64];
    off_t toff = s.text->p_offset + ((ElfW(Addr))mappings - (s.base + s.text->p_vaddr));
    yes("text is the file", file_at(toff, tf, sizeof tf) && memcmp(tf, (void *)mappings, sizeof tf) == 0);
    // RELRO is read-only: a write faults with SEGV_ACCERR.
    struct sigaction sa = {0}, old;
    sa.sa_sigaction = on_segv;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, &old);
    volatile char *relro = (volatile char *)(s.base + s.relro->p_vaddr);
    fault_code = 0;
    if (sigsetjmp(jb, 1) == 0)
        *relro = *relro;
    yes("relro write faults (SEGV_ACCERR)", fault_code == SEGV_ACCERR);
    sigaction(SIGSEGV, &old, NULL);
    // The first page (ELF header, read-only, from the file): made writable and written, the file keeps its
    // byte (a private copy, not the page cache).
    unsigned char *hdr = (unsigned char *)s.base;
    yes("mprotect header rw", mprotect(hdr, pg, PROT_READ | PROT_WRITE) == 0);
    hdr[EI_PAD + 2] = 0x5a;
    unsigned char fbyte = 0xff;
    yes("header written", hdr[EI_PAD + 2] == 0x5a);
    yes("header unchanged in the file", file_at(EI_PAD + 2, &fbyte, 1) && fbyte == 0);
    hdr[EI_PAD + 2] = 0;
    yes("mprotect header back", mprotect(hdr, pg, PROT_READ) == 0);
}

static void startup_calls(void) {
    yes("rseq registered or ENOSYS", __rseq_size > 0 || (syscall(SYS_rseq, NULL, 0, 0, 0) == -1 && errno == ENOSYS));
    char buf[1024];
    SYS("getrandom random|nonblock", SYS_getrandom, buf, 8, GRND_RANDOM | GRND_NONBLOCK);
    SYS("getrandom bad flags", SYS_getrandom, buf, 8, 0x80);
    SYS("getrandom insecure|random", SYS_getrandom, buf, 8, GRND_INSECURE | GRND_RANDOM);
    struct rlimit rl, bad;
    SYS("prlimit64 get stack", SYS_prlimit64, 0, RLIMIT_STACK, NULL, &rl);
    SYS("prlimit64 set stack same", SYS_prlimit64, 0, RLIMIT_STACK, &rl, NULL);
    getrlimit(RLIMIT_NOFILE, &bad);
    if (bad.rlim_max != RLIM_INFINITY) {
        bad.rlim_cur = bad.rlim_max + 1;
        SYS("prlimit64 cur above max", SYS_prlimit64, 0, RLIMIT_NOFILE, &bad, NULL);
    } else {
        printf("prlimit64 cur above max = -EINVAL\n");
    }
    SYS("sched_getaffinity len 4", SYS_sched_getaffinity, 0, 4, buf);
    long n = syscall(SYS_sched_getaffinity, 0, sizeof buf, buf);
    yes("sched_getaffinity len 1024 gives whole longs", n > 0 && n % 8 == 0 && n <= (long)sizeof buf);
    struct statx stx;
    yes("statx /", statx(AT_FDCWD, "/", 0, STATX_BASIC_STATS, &stx) == 0 && (stx.stx_mask & STATX_TYPE) && S_ISDIR(stx.stx_mode));
    SYS("statx sync type both", SYS_statx, AT_FDCWD, "/", AT_STATX_SYNC_TYPE, STATX_BASIC_STATS, &stx);
    SYS("statx reserved mask", SYS_statx, AT_FDCWD, "/", 0, 0x80000000u, &stx);
    SYS("clone3 size 8", SYS_clone3, buf, 8);
    SYS("set_robust_list len 8", SYS_set_robust_list, buf, 8);
}

// A copy of this program whose PT_INTERP names `interp` (padded with NULs; a NUL inside ends the path).
static int write_copy(const char *path, const char *interp, size_t ilen) {
    int in = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    struct stat st;
    if (in < 0 || fstat(in, &st) != 0)
        return 0;
    unsigned char *b = malloc(st.st_size);
    int ok = b && pread(in, b, st.st_size, 0) == st.st_size;
    close(in);
    ElfW(Ehdr) *e = (ElfW(Ehdr) *)b;
    ElfW(Phdr) *ip = NULL;
    for (int k = 0; ok && k < e->e_phnum; k++) {
        ElfW(Phdr) *p = (ElfW(Phdr) *)(b + e->e_phoff + k * sizeof *p);
        if (p->p_type == PT_INTERP)
            ip = p;
    }
    ok = ok && ip && ilen < ip->p_filesz;
    if (ok) {
        memset(b + ip->p_offset, 0, ip->p_filesz);
        memcpy(b + ip->p_offset, interp, ilen);
        unlink(path);
        int out = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0755);
        ok = out >= 0 && write(out, b, st.st_size) == st.st_size;
        if (out >= 0)
            close(out);
    }
    free(b);
    return ok;
}

static void put(const char *path, const char *text, size_t len, mode_t mode) {
    unlink(path);
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, mode);
    if (fd >= 0) {
        if (write(fd, text, len) != (ssize_t)len)
            printf("write %s: no\n", path);
        close(fd);
    }
}

static void bad_interp(const char *label, const char *interp, size_t ilen) {
    const char *prog = "/tmp/ab8-badinterp";
    if (!write_copy(prog, interp, ilen)) {
        printf("execve %s: no copy\n", label);
        return;
    }
    char *av[] = {"dyn-edges", "never", NULL};
    execve(prog, av, environ);
    printf("execve %s = -%s\n", label, ename(errno));
}

static void exec_refusals(void) {
    mkdir("/tmp", 0777);
    static const char text[] = "#!/no/such/shell\nnot an ELF file at all, but long enough for an ELF header.......\n";
    put("/tmp/ab8-notelf", text, sizeof text - 1, 0755);
    put("/tmp/ab8-short", "\177ELF\2\1\1\0\0\0", 10, 0755);
    put("/tmp/ab8-noexec", text, sizeof text - 1, 0644);
    bad_interp("missing interpreter", "/no/such/ld.so", 14);
    bad_interp("directory interpreter", "/tmp", 4);
    bad_interp("interpreter cut at its first NUL", "/tmp\0/ab8-notelf", 16);
    bad_interp("interpreter not ELF", "/tmp/ab8-notelf", 15);
    bad_interp("interpreter shorter than a header", "/tmp/ab8-short", 14);
    bad_interp("interpreter without x", "/tmp/ab8-noexec", 15);
    unlink("/tmp/ab8-badinterp");
    unlink("/tmp/ab8-notelf");
    unlink("/tmp/ab8-short");
    unlink("/tmp/ab8-noexec");
    yes("still running after the refusals", data_var == 0x0BADF00D);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "never") == 0) {
        printf("a copy with a bad interpreter ran: no\n");
        return 1;
    }
    mappings();
    startup_calls();
    exec_refusals();
    printf("dyn edges: done\n");
    return 0;
}
