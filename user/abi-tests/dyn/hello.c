// AB-8: a dynamic glibc program (PT_INTERP /lib64/ld-linux-x86-64.so.2, libc.so.6 from the library set):
// what the kernel's ELF loader hands the dynamic loader (auxv), file-backed private mappings with RELRO made
// read-only, TLS, dlopen/dlsym, and the calls glibc falls back to without a vDSO.
#include "abi.h"
#include <dlfcn.h>
#include <elf.h>
#include <link.h>
#include <sched.h>
#include <sys/auxv.h>
#include <sys/random.h>
#include <sys/resource.h>
#include <sys/stat.h>

extern char _start[];
static __thread int tls_value = 42;

struct seen {
    const ElfW(Phdr) *main_phdr;
    ElfW(Addr) loader_base, main_relro, libc_relro;
    int objects;
};

static int each(struct dl_phdr_info *i, size_t size, void *arg) {
    (void)size;
    struct seen *s = arg;
    s->objects++;
    ElfW(Addr) relro = 0;
    for (int k = 0; k < i->dlpi_phnum; k++)
        if (i->dlpi_phdr[k].p_type == PT_GNU_RELRO)
            relro = i->dlpi_addr + i->dlpi_phdr[k].p_vaddr;
    if (!i->dlpi_name || !i->dlpi_name[0]) {
        if (!s->main_phdr) {
            s->main_phdr = i->dlpi_phdr;
            s->main_relro = relro;
        }
    } else if (strstr(i->dlpi_name, "ld-linux")) {
        s->loader_base = i->dlpi_addr;
    } else if (strstr(i->dlpi_name, "libc.so")) {
        s->libc_relro = relro;
    }
    return 0;
}

// The protection /proc/self/maps shows for the mapping holding `a` ("" when none does).
static const char *perms_at(ElfW(Addr) a) {
    static char p[5];
    FILE *f = fopen("/proc/self/maps", "r");
    char line[512];
    p[0] = 0;
    while (f && fgets(line, sizeof line, f)) {
        unsigned long lo, hi;
        char pr[5];
        if (sscanf(line, "%lx-%lx %4s", &lo, &hi, pr) == 3 && a >= lo && a < hi) {
            memcpy(p, pr, 5);
            break;
        }
    }
    if (f)
        fclose(f);
    return p;
}

// 1 MiB of stack per call: 20 calls need more than the default 8 MiB RLIMIT_STACK.
__attribute__((noinline)) static int deep(int n) {
    volatile char frame[1 << 20];
    frame[0] = (char)n;
    frame[sizeof frame - 1] = (char)n;
    return n == 0 ? frame[0] : deep(n - 1) + frame[sizeof frame - 1] - n;
}

// "stack": raise RLIMIT_STACK to 64 MiB and execve this program again ("stack2"): the new image's stack follows
// the limit (Linux grows it up to the limit; V2 maps it whole at exec).
static int stack_mode(char **argv) {
    if (strcmp(argv[1], "stack") == 0) {
        struct rlimit rl;
        getrlimit(RLIMIT_STACK, &rl);
        rl.rlim_cur = rl.rlim_max == RLIM_INFINITY || rl.rlim_max >= (64ul << 20) ? 64ul << 20 : rl.rlim_max;
        yes("setrlimit stack 64 MiB", setrlimit(RLIMIT_STACK, &rl) == 0 && rl.rlim_cur == (64ul << 20));
        fflush(stdout);
        char *again[] = {argv[0], "stack2", NULL};
        execv("/proc/self/exe", again);
        printf("execv: %s\n", strerror(errno));
        return 1;
    }
    yes("deep stack after exec", deep(20) == 0);
    printf("dyn hello: done\n");
    return 0;
}

int main(int argc, char **argv) {
    if (argc > 1 && strncmp(argv[1], "stack", 5) == 0)
        return stack_mode(argv);
    printf("dyn hello: argc=%d\n", argc);
    for (int i = 1; i < argc; i++)
        printf("arg %d: %s\n", i, argv[i]);
    struct seen s = {0};
    dl_iterate_phdr(each, &s);
    yes("at_base is the loader", getauxval(AT_BASE) != 0 && getauxval(AT_BASE) == s.loader_base);
    yes("at_entry is _start", getauxval(AT_ENTRY) == (unsigned long)_start);
    yes("at_phdr is the program's", getauxval(AT_PHDR) == (unsigned long)s.main_phdr);
    yes("at_phnum set", getauxval(AT_PHNUM) > 0);
    yes("at_random set", getauxval(AT_RANDOM) != 0);
    yes("at_execfn names the program", strstr((const char *)getauxval(AT_EXECFN), "dyn-hello") != NULL);
    yes("at_platform x86_64", strcmp((const char *)getauxval(AT_PLATFORM), "x86_64") == 0);
    yes("three objects or more", s.objects >= 3);
    yes("program relro read-only", s.main_relro && strncmp(perms_at(s.main_relro), "r--p", 4) == 0);
    yes("libc relro read-only", s.libc_relro && strncmp(perms_at(s.libc_relro), "r--p", 4) == 0);
    yes("program text r-xp", strncmp(perms_at((ElfW(Addr))main), "r-xp", 4) == 0);
    yes("tls initialised", tls_value == 42);
    void *h = dlopen("libc.so.6", RTLD_NOW | RTLD_NOLOAD);
    yes("dlopen libc", h != NULL);
    yes("dlsym puts", h && dlsym(h, "puts") == (void *)puts);
    struct timespec t;
    yes("clock_gettime", clock_gettime(CLOCK_MONOTONIC, &t) == 0 && (t.tv_sec || t.tv_nsec));
    yes("time", time(NULL) > 1700000000);
    yes("sched_getcpu", sched_getcpu() >= 0);
    struct stat st;
    yes("stat /", stat("/", &st) == 0 && S_ISDIR(st.st_mode));
    unsigned char r[16];
    yes("getrandom", getrandom(r, sizeof r, GRND_NONBLOCK) == sizeof r);
    struct rlimit rl;
    yes("getrlimit stack", getrlimit(RLIMIT_STACK, &rl) == 0 && rl.rlim_cur > 0);
    char *big = malloc(4 << 20);
    yes("malloc 4 MiB", big != NULL);
    if (big) {
        memset(big, 7, 4 << 20);
        free(big);
    }
    printf("dyn hello: done\n");
    return 0;
}
