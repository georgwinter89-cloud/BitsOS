/* AB-2: prints what a program finds at its start (argv, envp, auxiliary vector), normalised so the same text
 * comes out on CachyOS and on BitsOS V2: values that differ between machines (addresses, the CPU's feature
 * words) are printed as the check they pass ("AT_PHDR=own program headers"), values Linux fixes (page size,
 * clock ticks, uid) as numbers. Lines for entries only Linux has today (vDSO, rseq) start with "linux-only".
 * Build: user/sysroot/bin/v2-cc -O2 -o ab-auxv auxv.c (static-PIE, musl). */
#include <cpuid.h>
#include <elf.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

extern char **environ;
extern const Elf64_Ehdr __ehdr_start;
extern void _start(void);

static const char *name(uint64_t k) {
  switch (k) {
  case AT_PHDR: return "AT_PHDR"; case AT_PHENT: return "AT_PHENT"; case AT_PHNUM: return "AT_PHNUM";
  case AT_PAGESZ: return "AT_PAGESZ"; case AT_BASE: return "AT_BASE"; case AT_FLAGS: return "AT_FLAGS";
  case AT_ENTRY: return "AT_ENTRY"; case AT_UID: return "AT_UID"; case AT_EUID: return "AT_EUID";
  case AT_GID: return "AT_GID"; case AT_EGID: return "AT_EGID"; case AT_PLATFORM: return "AT_PLATFORM";
  case AT_HWCAP: return "AT_HWCAP"; case AT_CLKTCK: return "AT_CLKTCK"; case AT_SECURE: return "AT_SECURE";
  case AT_RANDOM: return "AT_RANDOM"; case AT_HWCAP2: return "AT_HWCAP2"; case AT_EXECFN: return "AT_EXECFN";
  case AT_SYSINFO_EHDR: return "AT_SYSINFO_EHDR"; case 51: return "AT_MINSIGSTKSZ";
  case 27: return "AT_RSEQ_FEATURE_SIZE"; case 28: return "AT_RSEQ_ALIGN";
  default: return 0;
  }
}

int main(int argc, char **argv) {
  /* The initial stack: argv, NULL, envp, NULL, auxv (musl's environ is the initial envp). */
  char **e = argv + argc + 1;
  int envc = 0;
  while (e[envc]) envc++;
  Elf64_auxv_t *av = (Elf64_auxv_t *)(e + envc + 1);
  printf("argc=%d\n", argc);
  for (int i = 0; i < argc; i++) printf("argv[%d]=\"%s\"\n", i, argv[i]);
  printf("envp is environ: %s\n", e == environ ? "yes" : "no");
  for (int i = 0; i < envc; i++) printf("envp[%d]=\"%s\"\n", i, e[i]);
  printf("argv%%16=%u\n", (unsigned)((uintptr_t)argv % 16));
  /* Linux puts the strings in order argv..., envp..., then AT_EXECFN's string. */
  uintptr_t last = 0;
  int ordered = 1;
  for (int i = 0; i < argc; i++) { if ((uintptr_t)argv[i] <= last) ordered = 0; last = (uintptr_t)argv[i]; }
  for (int i = 0; i < envc; i++) { if ((uintptr_t)e[i] <= last) ordered = 0; last = (uintptr_t)e[i]; }
  unsigned a, b, c, d;
  __get_cpuid(1, &a, &b, &c, &d);
  int n = 0;
  for (; av->a_type != AT_NULL; av++) {
    uint64_t k = av->a_type, v = av->a_un.a_val;
    const char *nm = name(k);
    if (!nm) { printf("unknown auxv key %llu\n", (unsigned long long)k); continue; }
    if (k == AT_SYSINFO_EHDR || k == 27 || k == 28) { printf("linux-only %s\n", nm); continue; }
    n++;
    printf("%s=", nm);
    switch (k) {
    case AT_PHDR: {
      const char *own = (const char *)&__ehdr_start + __ehdr_start.e_phoff;
      printf("%s\n", (const char *)v == own ? "own program headers" : "WRONG"); break;
    }
    case AT_PHNUM: printf("%s\n", v == __ehdr_start.e_phnum ? "e_phnum" : "WRONG"); break;
    case AT_ENTRY: printf("%s\n", v == (uintptr_t)_start ? "_start" : "WRONG"); break;
    case AT_HWCAP: printf("%s\n", v == d ? "cpuid(1).edx" : "WRONG"); break;
    case AT_HWCAP2: printf("%s\n", v == 0 ? "0" : v == 2 ? "FSGSBASE" : "OTHER"); break;
    case AT_UID: printf("%s\n", v == getuid() ? "getuid" : "WRONG"); break;
    case AT_EUID: printf("%s\n", v == geteuid() ? "geteuid" : "WRONG"); break;
    case AT_GID: printf("%s\n", v == getgid() ? "getgid" : "WRONG"); break;
    case AT_EGID: printf("%s\n", v == getegid() ? "getegid" : "WRONG"); break;
    case AT_RANDOM: {
      const unsigned char *r = (const unsigned char *)v;
      int zero = 1;
      for (int i = 0; i < 16; i++) if (r[i]) zero = 0;
      printf("%s\n", (uintptr_t)r > (uintptr_t)av && !zero ? "16 bytes above the table, not zero" : "WRONG"); break;
    }
    case AT_EXECFN: {
      const char *s = (const char *)v, *base = strrchr(s, '/');
      printf("%s%s\n", base ? base + 1 : s, (uintptr_t)s > last ? " (after the env strings)" : " WRONG PLACE");
      break;
    }
    case AT_PLATFORM: printf("%s\n", (const char *)v); break;
    case 51: printf("%s\n", v >= 1024 && v % 16 == 0 ? ">=1024, 16-aligned" : "WRONG"); break;
    default: printf("%llu\n", (unsigned long long)v);
    }
  }
  printf("auxv entries without linux-only ones: counted %d\n", n);
  printf("strings ordered argv..env: %s\n", ordered ? "yes" : "no");
  return 0;
}
